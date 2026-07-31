//! Managed bgutil GVS PO token server.
//!
//! YouTube SABR captures need a "GVS PO token" minted by the bgutil provider
//! (`bgutil-ytdlp-pot-provider`): yt-dlp's plugin asks an HTTP server
//! (default `http://127.0.0.1:4416`, see [`crate::downloader::tools::SABR_DEFAULT_POT_ARGS`])
//! to generate tokens. Without it a from-start SABR capture dies mid-stream
//! with `PoTokenError: This stream requires a GVS PO Token to continue`
//! (observed 2026-07-18: a girl_dm_ capture crash-looped for 20+ minutes,
//! burning a fresh take every backoff cycle, because the server had stopped).
//!
//! This module makes the server a managed dependency instead of an assumed
//! one:
//! - a **watchdog task** (spawned from [`crate::app_core::AppCore::start`])
//!   pings `GET /ping` every 30 s, launches `node main.js -p <port>` when the
//!   server is wanted-but-down, and restarts it if it crashes (exponential
//!   backoff, one 🔔 notification per down-episode);
//! - the capture path calls [`ensure_up`] when a take dies with a PO-token
//!   error, so the in-flight SABR retry resumes the same take against a live
//!   server instead of failing three times against a dead one;
//! - the UI gets [`status`]/[`request_start`]/[`request_stop`] plus the
//!   server's combined stdout+stderr in `logs\pot_server.log` ([`log_path`]).
//!
//! An **externally started** server is detected via the same ping and simply
//! adopted as `External` — never spawned over, never killed automatically.
//! The user can still take it over explicitly ([`stop_external`] /
//! [`take_control`], which find the port's owning pid). A server *we*
//! spawned has its pid persisted so the next app run can re-adopt it as
//! `Managed` (the default quit path leaves it running on purpose: detached
//! SABR captures still need tokens after the app exits).

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::store::Store;

/// Settings key: directory holding the built server (`main.js`).
pub const K_POT_SERVER_DIR: &str = "pot_server_dir";
/// Settings key: node binary used to run it (name on PATH or full path).
pub const K_POT_SERVER_NODE: &str = "pot_server_node";
/// Settings key: launch the server automatically at app startup ("1"/"0",
/// absent ⇒ on).
pub const K_POT_SERVER_AUTOSTART: &str = "pot_server_autostart";
/// Settings keys persisting the pid (+ its kernel start time, guarding
/// against pid recycling) of a server *we* spawned, so the next app run can
/// re-adopt a still-running instance as Managed instead of seeing it as
/// External (which would disable the Stop button forever).
const K_POT_SERVER_PID: &str = "pot_server_pid";
const K_POT_SERVER_PID_START: &str = "pot_server_pid_start";

/// Default server dir when the setting has never been written. Points at the
/// standard clone location on this machine; on a machine without it the
/// watchdog logs one warning and idles — a missing dir is never fatal.
pub const DEFAULT_SERVER_DIR: &str = r"C:\git\bgutil-ytdlp-pot-provider\server\build";
/// Default node binary (resolved from PATH).
pub const DEFAULT_NODE_BIN: &str = "node";
/// Base URL used when no `base_url=` can be parsed out of the PO-token
/// extractor-args setting (matches the plugin's own default).
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:4416";

/// Healthy-server poll interval.
const POLL_INTERVAL: Duration = Duration::from_secs(30);
/// How long a freshly spawned server gets to answer `/ping` before the spawn
/// counts as failed (node + jsdom startup is a few seconds on a busy disk).
/// A YouTube PO-token rejection STORM is in progress when at least this many
/// takes were refused within [`STORM_WINDOW_SECS`]. During one, the token
/// server is typically healthy but saturated: the token-binding experiment
/// forces a full BotGuard attestation per mint (the integrity-token cache
/// never applies), node's event loop pegs, and /ping starves — which the
/// watchdog used to misreport as "server not responding" (2026-07-31).
const STORM_MIN_TAKES: i64 = 2;
/// Look-back window for storm detection; also the quiet period that ends one.
const STORM_WINDOW_SECS: i64 = 15 * 60;

/// Re-check delay after a live server missed a single /ping (see
/// `Action::Grace`) — short, so a genuinely wedged server still reaches the
/// spawn path within ~seconds rather than a full poll interval.
const GRACE_INTERVAL: Duration = Duration::from_secs(5);
const STARTUP_WAIT: Duration = Duration::from_secs(15);
/// Idle re-check interval when the server isn't wanted (nudges cut it short).
const IDLE_INTERVAL: Duration = Duration::from_secs(300);

/// What the user/session wants the watchdog to do, layered over the persisted
/// autostart setting. Session-only — a manual Start forces the server up even
/// with autostart off; a manual Stop keeps it down (and stops the watchdog
/// from fighting the user) until Start is clicked again or the app restarts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desired {
    /// Follow the autostart setting.
    Auto,
    /// User clicked Start (or a capture needed the server on demand).
    ForcedOn,
    /// User clicked Stop.
    ForcedOff,
}

/// Where the server currently stands, as far as the watchdog knows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PotMode {
    /// Not wanted (autostart off / user-stopped) and not detected running.
    Disabled,
    /// Wanted but not currently up (between spawn attempts).
    Down,
    /// Spawn issued, waiting for `/ping` to go green.
    Starting,
    /// A server this app spawned (or re-adopted from a previous run).
    Managed { pid: u32 },
    /// A responding server someone else started — used, never killed.
    External,
    /// The last spawn attempt failed (missing dir, node error, no ping).
    Failed { reason: String },
}

/// Last successful `/ping` payload.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PingInfo {
    pub uptime_secs: f64,
    pub version: String,
}

/// UI-facing snapshot of the watchdog state.
#[derive(Clone, Debug)]
pub struct PotStatus {
    pub mode: PotMode,
    pub desired: Desired,
    pub last_ping: Option<PingInfo>,
    pub base_url: String,
}

impl Default for PotStatus {
    fn default() -> Self {
        Self {
            mode: PotMode::Disabled,
            desired: Desired::Auto,
            last_ping: None,
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

/// Settings snapshot the watchdog runs against. Refreshed by [`init`] and
/// [`apply_settings`] (i.e. on startup and on every settings save), not per
/// tick — the watchdog itself never touches the DB.
#[derive(Clone, Debug)]
pub struct PotConfig {
    pub autostart: bool,
    pub dir: String,
    pub node: String,
    pub base_url: String,
}

impl Default for PotConfig {
    fn default() -> Self {
        Self {
            autostart: true,
            dir: DEFAULT_SERVER_DIR.to_string(),
            node: DEFAULT_NODE_BIN.to_string(),
            base_url: DEFAULT_BASE_URL.to_string(),
        }
    }
}

static STATUS: Mutex<Option<PotStatus>> = Mutex::new(None);
static DESIRED: Mutex<Desired> = Mutex::new(Desired::Auto);
static CONFIG: Mutex<Option<PotConfig>> = Mutex::new(None);
/// Pid re-adopted from a previous app run (set by [`init`], consumed by the
/// watchdog's first iteration).
static ADOPTED_PID: Mutex<Option<u32>> = Mutex::new(None);
/// Pid of the managed child, mirrored outside the watchdog loop so the quit
/// path ([`kill_managed`]) can reach it synchronously.
static OWNED_PID: Mutex<Option<u32>> = Mutex::new(None);
static NUDGE: OnceLock<tokio::sync::Notify> = OnceLock::new();
static HTTP: OnceLock<reqwest::Client> = OnceLock::new();

fn nudge_handle() -> &'static tokio::sync::Notify {
    NUDGE.get_or_init(tokio::sync::Notify::new)
}

fn http() -> &'static reqwest::Client {
    HTTP.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
            .expect("reqwest client")
    })
}

/// Current UI-facing status snapshot.
pub fn status() -> PotStatus {
    let mut s = STATUS.lock().unwrap().clone().unwrap_or_default();
    s.desired = *DESIRED.lock().unwrap();
    s
}

fn set_status(mode: PotMode, last_ping: Option<PingInfo>) {
    let base_url = config().base_url;
    *STATUS.lock().unwrap() = Some(PotStatus {
        mode,
        desired: *DESIRED.lock().unwrap(),
        last_ping,
        base_url,
    });
}

fn config() -> PotConfig {
    CONFIG.lock().unwrap().clone().unwrap_or_default()
}

/// The managed server's combined stdout+stderr log.
pub fn log_path() -> PathBuf {
    crate::app_paths::logs_dir().join("pot_server.log")
}

/// Wake the watchdog for an immediate health check (e.g. right after a
/// capture failed with a PO-token error, or after a settings change).
pub fn nudge() {
    nudge_handle().notify_one();
}

/// User clicked Start: force the server up regardless of the autostart
/// setting (session-only) and check immediately.
pub fn request_start() {
    *DESIRED.lock().unwrap() = Desired::ForcedOn;
    nudge();
}

/// User clicked Stop: kill the managed server (on the watchdog thread) and
/// keep it down until Start is clicked or the app restarts. An External
/// server needs [`stop_external`] instead — the watchdog holds no handle to
/// kill it with.
pub fn request_stop() {
    *DESIRED.lock().unwrap() = Desired::ForcedOff;
    nudge();
}

/// Kill the EXTERNAL server currently answering on the configured port, by
/// looking up which process owns the listener. Only reachable from an
/// explicit user click (the UI offers it only in `External` mode); returns
/// the killed pid, or an error when no listener could be attributed. Sets
/// `ForcedOff` first so the watchdog doesn't immediately respawn a managed
/// replacement (that's [`take_control`]'s job).
pub fn stop_external() -> Result<u32, String> {
    *DESIRED.lock().unwrap() = Desired::ForcedOff;
    let pid = kill_external_listener()?;
    nudge();
    Ok(pid)
}

/// Replace an external server with a managed one: kill the port's owner,
/// then force our own instance up — from here on the watchdog supervises,
/// restarts, and the Stop button works. Returns the killed pid.
pub fn take_control() -> Result<u32, String> {
    let pid = kill_external_listener()?;
    *DESIRED.lock().unwrap() = Desired::ForcedOn;
    nudge();
    Ok(pid)
}

/// Find and kill the process listening on the configured port. The pid is
/// re-checked against our own managed child (paranoia: the UI only offers
/// this in External mode, but a race with the watchdog spawning must never
/// kill our own fresh child via the "external" path).
fn kill_external_listener() -> Result<u32, String> {
    let port = base_url_port(&config().base_url);
    let pid = crate::platform::pid_listening_on(port)
        .ok_or_else(|| format!("no process found listening on port {port}"))?;
    if pid == std::process::id() {
        return Err("port is owned by this app process".to_string());
    }
    if OWNED_PID.lock().unwrap().is_some_and(|own| own == pid) {
        return Err("server is already managed by this app".to_string());
    }
    info!(pid, port, "killing external PO token server (user request)");
    crate::platform::kill_process_tree(pid);
    Ok(pid)
}

/// Read the settings into the config snapshot and stage adoption of a server
/// left running by a previous app run. Called once from `AppCore::start`
/// before the watchdog spawns.
pub fn init(store: &Store) {
    refresh_config(store);
    let pid = store
        .get_setting(K_POT_SERVER_PID)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or(0);
    let start = store
        .get_setting(K_POT_SERVER_PID_START)
        .ok()
        .flatten()
        .and_then(|v| v.parse::<u64>().ok());
    if pid != 0
        && crate::platform::pid_alive(pid)
        && crate::platform::process_start_time(pid) == start
    {
        info!(pid, "re-adopting PO token server spawned by a previous run");
        *ADOPTED_PID.lock().unwrap() = Some(pid);
    }
}

/// Re-read the settings snapshot and wake the watchdog so a changed path,
/// port, or autostart toggle takes effect without an app restart. Called
/// after every settings save.
pub fn apply_settings(store: &Store) {
    refresh_config(store);
    nudge();
}

fn refresh_config(store: &Store) {
    let get = |k: &str| store.get_setting(k).ok().flatten();
    let autostart = get(K_POT_SERVER_AUTOSTART).map(|v| v != "0").unwrap_or(true);
    let dir = get(K_POT_SERVER_DIR)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_SERVER_DIR.to_string());
    let node = get(K_POT_SERVER_NODE)
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_NODE_BIN.to_string());
    // Same absent-vs-explicit-empty semantics as the yt-dlp arg builder: an
    // absent setting means the bgutil default; an explicit value (even one
    // without a base_url) falls back to the default URL for OUR ping/spawn,
    // since the plugin's own auto-detection also assumes 4416.
    let pot_args = get(crate::ui::K_SABR_POT_ARGS)
        .unwrap_or_else(|| crate::downloader::SABR_DEFAULT_POT_ARGS.to_string());
    let base_url = pot_base_url(&pot_args);
    *CONFIG.lock().unwrap() = Some(PotConfig { autostart, dir, node, base_url });
}

/// Extract the `base_url=` value out of a PO-token `--extractor-args` string
/// (e.g. `youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416`), so the
/// server we launch and ping is always the one yt-dlp will actually contact.
/// Falls back to [`DEFAULT_BASE_URL`].
pub fn pot_base_url(pot_args: &str) -> String {
    let Some(idx) = pot_args.find("base_url=") else {
        return DEFAULT_BASE_URL.to_string();
    };
    let rest = &pot_args[idx + "base_url=".len()..];
    let end = rest.find([';', ',', ' ']).unwrap_or(rest.len());
    let url = rest[..end].trim().trim_end_matches('/');
    if url.is_empty() { DEFAULT_BASE_URL.to_string() } else { url.to_string() }
}

/// Port to pass to `main.js -p`, parsed from the base URL (falls back to the
/// bgutil default 4416).
pub fn base_url_port(base_url: &str) -> u16 {
    let host_part = base_url.split("://").nth(1).unwrap_or(base_url);
    let host_port = host_part.split('/').next().unwrap_or(host_part);
    host_port
        .rsplit_once(':')
        .and_then(|(_, p)| p.parse::<u16>().ok())
        .unwrap_or(4416)
}

/// What the watchdog should do this tick. Pure so the branching is testable
/// without processes or sockets.
#[derive(Debug, PartialEq, Eq)]
enum Action {
    /// Not wanted, nothing running that's ours: sleep until nudged.
    Idle,
    /// Ping answered: record health, sleep the normal interval.
    Healthy,
    /// Wanted but down: (re)spawn.
    Spawn,
    /// Wanted, not answering, but our own server process is still alive and
    /// this is its first missed ping — re-check shortly instead of spawning
    /// a duplicate over it (see the body's comment).
    Grace,
    /// Not wanted but our child is alive: kill it.
    Kill,
}

fn watch_action(
    desired: Desired,
    autostart: bool,
    ping_ok: bool,
    // A server process is live: our child, an adopted pid, OR any foreign
    // listener on the configured port. A busy external server deserves the
    // same missed-ping grace as our own — relaunching mid-token-storm used
    // to spawn over the previous session's still-working server on its
    // first missed ping (2026-07-31 20:17).
    server_present: bool,
    ping_failures: u32,
) -> Action {
    let wanted = match desired {
        Desired::ForcedOn => true,
        Desired::ForcedOff => false,
        Desired::Auto => autostart,
    };
    if !wanted {
        if server_present { Action::Kill } else { Action::Idle }
    } else if ping_ok {
        Action::Healthy
    } else if server_present && ping_failures < 3 {
        // A live server missed a /ping (or two). That is NOT yet grounds to
        // spawn a replacement: a BotGuard mint pegs node's event loop for
        // seconds at a time while the server is perfectly healthy — and
        // spawning over a live server is exactly how the 2026-07-31
        // duplicate-server pileup started (the second instance half-binds
        // the other address family, both stay up, and every later spawn
        // EADDRINUSE-loops). Three strikes before acting.
        Action::Grace
    } else {
        Action::Spawn
    }
}

/// Delay before the next spawn attempt after `n` consecutive failures —
/// exponential so a broken config doesn't spin node in a tight loop.
fn spawn_backoff(consecutive_failures: u32) -> Duration {
    Duration::from_secs(match consecutive_failures {
        0 | 1 => 30,
        2 => 60,
        3 => 120,
        _ => 300,
    })
}

/// `GET {base}/ping` — `Some(PingInfo)` iff a bgutil server answered.
/// 4s, not 2: a healthy server can spend seconds inside one BotGuard mint
/// (node's event loop pegs), and every false ping miss walks the watchdog one
/// step toward replacing a working server.
pub async fn ping(base_url: &str) -> Option<PingInfo> {
    ping_with(base_url, Duration::from_secs(4)).await
}

/// [`ping`] with an explicit timeout (the pre-kill last-chance probe waits
/// far longer than the routine health check).
async fn ping_with(base_url: &str, timeout: Duration) -> Option<PingInfo> {
    let url = format!("{}/ping", base_url.trim_end_matches('/'));
    let resp = http().get(&url).timeout(timeout).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    Some(PingInfo {
        uptime_secs: v.get("server_uptime").and_then(serde_json::Value::as_f64).unwrap_or(0.0),
        version: v
            .get("version")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("?")
            .to_string(),
    })
}

/// Make sure the server is reachable, starting it if allowed, waiting up to
/// `timeout` for `/ping` to go green. Used by the capture path when a take
/// died with a PO-token error, right before the in-flight SABR retry — so the
/// retry resumes against a live server instead of dying identically.
///
/// Respects an explicit user Stop (`ForcedOff` ⇒ never restarts); otherwise
/// forces the server on for the rest of the session — a capture proved it's
/// needed, even if autostart is off.
pub async fn ensure_up(timeout: Duration) -> bool {
    let base = config().base_url;
    if ping(&base).await.is_some() {
        return true;
    }
    {
        let mut desired = DESIRED.lock().unwrap();
        if *desired == Desired::ForcedOff {
            debug!("PO token server down but user-stopped; not restarting for capture retry");
            return false;
        }
        if *desired == Desired::Auto && !config().autostart {
            info!("starting PO token server on demand (a capture needs tokens, autostart is off)");
            *desired = Desired::ForcedOn;
        }
    }
    nudge();
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_secs(1)).await;
        if ping(&base).await.is_some() {
            return true;
        }
    }
    false
}

/// Kill a managed server synchronously (quit-and-stop path). External servers
/// are untouched. Clears the persisted adoption pid so the next run doesn't
/// look for a corpse.
pub fn kill_managed(store: &Store) {
    let pid = OWNED_PID.lock().unwrap().take();
    if let Some(pid) = pid {
        info!(pid, "stopping managed PO token server");
        crate::platform::kill_process_tree(pid);
    }
    let _ = store.set_setting(K_POT_SERVER_PID, "");
    let _ = store.set_setting(K_POT_SERVER_PID_START, "");
}

/// Spawn the watchdog onto the core runtime. Call once from `AppCore::start`
/// (after [`init`]).
pub fn start_watchdog(
    store: Arc<Store>,
    events: crate::events::EventTx,
    shutdown: Arc<AtomicBool>,
    rt: &tokio::runtime::Handle,
) {
    rt.spawn(watchdog_loop(store, events, shutdown));
}

async fn watchdog_loop(
    store: Arc<Store>,
    events: crate::events::EventTx,
    shutdown: Arc<AtomicBool>,
) {
    // Child handle when we spawned this session; a re-adopted server from a
    // previous run is pid-only (no handle — liveness via pid_alive).
    let mut child: Option<tokio::process::Child> = None;
    let mut adopted: Option<u32> = ADOPTED_PID.lock().unwrap().take();
    // Truncate the log once per app run (first spawn), append across in-run
    // restarts — a restart must not destroy the previous crash's evidence.
    let mut truncated_this_run = false;
    let mut consecutive_failures: u32 = 0;
    // Consecutive watchdog ticks whose /ping went unanswered — feeds the
    // Grace decision in `watch_action` (one missed ping of a live server is
    // tolerated; two means it's really wedged).
    let mut ping_failures: u32 = 0;
    // Unix time a rejection storm was first detected; `None` = no storm.
    // Drives one 🔔 per episode plus the accurate degraded-mode wording, and
    // clears (with an info log) once the window goes quiet.
    let mut storm_since: Option<i64> = None;
    // One 🔔 notification per down-episode, not one per failed attempt.
    let mut notified_down = false;
    let mut warned_missing_dir = false;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let cfg = config();
        let desired = *DESIRED.lock().unwrap();

        // Reap/refresh our child's liveness before deciding anything.
        let mut exited: Option<String> = None;
        if let Some(c) = child.as_mut() {
            match c.try_wait() {
                Ok(Some(status)) => {
                    exited = Some(format!("{status}"));
                    child = None;
                }
                Ok(None) => {}
                Err(e) => {
                    exited = Some(format!("wait error: {e}"));
                    child = None;
                }
            }
        } else if let Some(pid) = adopted
            && !crate::platform::pid_alive(pid)
        {
            exited = Some("adopted process gone".to_string());
            adopted = None;
        }
        if exited.is_some() {
            *OWNED_PID.lock().unwrap() = None;
        }

        let ping_info = ping(&cfg.base_url).await;
        if ping_info.is_some() {
            ping_failures = 0;
        } else {
            ping_failures = ping_failures.saturating_add(1);
        }
        // A foreign listener on the port counts as a present server for the
        // missed-ping grace (only looked up when the ping just failed — one
        // TCP-table read per degraded tick, nothing on the happy path).
        let foreign_listener = ping_info.is_none()
            && crate::platform::pid_listening_on(base_url_port(&cfg.base_url))
                .is_some_and(|p| p != std::process::id());
        let child_alive = child.is_some() || adopted.is_some() || foreign_listener;

        // Rejection-storm edge detection (one cheap indexed COUNT per tick).
        let now = crate::models::now_unix();
        let (storm_takes, storm_channels) =
            store.po_rejections_since(now - STORM_WINDOW_SECS).unwrap_or((0, 0));
        let storm = storm_takes >= STORM_MIN_TAKES;
        match (storm, storm_since) {
            (true, None) => {
                storm_since = Some(now);
                warn!(
                    takes = storm_takes,
                    channels = storm_channels,
                    "YouTube PO-token rejection storm in progress — the token server is \
                     healthy but every mint needs a full attestation challenge, so it may \
                     answer slowly; captures back off with the escalating 5-15m cooldown"
                );
                let _ = events.send(crate::events::AppEvent::CaptureAlert {
                    severity: "error".to_string(),
                    title: "🎫 YouTube PO-token rejection storm".to_string(),
                    body: format!(
                        "{storm_takes} capture attempt(s) across {storm_channels} channel(s) \
                         had their GVS PO tokens refused in the last 15 minutes. This is a \
                         YouTube-side condition (ATTESTATION_REQUIRED) — the token server is \
                         working, just saturated: each token now needs a full attestation \
                         challenge, so it may answer health checks slowly. Captures retry \
                         with an escalating 5-15 minute cooldown; no action needed."
                    ),
                    monitor_id: None,
                    channel: String::new(),
                    recording_id: None,
                    ref_key: format!("po_storm:{now}"),
                });
            }
            (false, Some(since)) => {
                info!(
                    lasted_mins = (now - since) / 60,
                    "PO-token rejection storm cleared (no refusals in the last 15 minutes)"
                );
                storm_since = None;
            }
            _ => {}
        }

        // A managed server that died since the last tick is worth a warning
        // with whatever it last wrote — even if an external one answered the
        // ping in its place.
        if let Some(status) = &exited {
            let tail = crate::downloader::read_log_tail(&log_path(), 15).await;
            warn!(
                "managed PO token server exited ({status}) — last log lines:\n{}",
                tail.trim_end()
            );
        }

        match watch_action(desired, cfg.autostart, ping_info.is_some(), child_alive, ping_failures)
        {
            Action::Idle => {
                set_status(
                    if desired == Desired::ForcedOff { PotMode::Down } else { PotMode::Disabled },
                    None,
                );
                sleep_or_nudge(IDLE_INTERVAL, &shutdown).await;
            }
            Action::Healthy => {
                if consecutive_failures > 0 || notified_down {
                    info!("PO token server healthy again");
                }
                consecutive_failures = 0;
                ping_failures = 0;
                notified_down = false;
                warned_missing_dir = false;
                let mode = match owned_pid_now(&child, adopted) {
                    Some(pid) => PotMode::Managed { pid },
                    None => PotMode::External,
                };
                set_status(mode, ping_info);
                sleep_or_nudge(POLL_INTERVAL, &shutdown).await;
            }
            Action::Grace => {
                // Alive but slow — re-check soon, don't spawn over it.
                debug!(ping_failures, "PO token server missed a ping; re-checking before acting");
                sleep_or_nudge(GRACE_INTERVAL, &shutdown).await;
            }
            Action::Kill => {
                if let Some(pid) = owned_pid_now(&child, adopted) {
                    info!(pid, "stopping managed PO token server (user request)");
                    crate::platform::kill_process_tree(pid);
                }
                // Reap so the pid can't be misread as alive next tick.
                if let Some(mut c) = child.take() {
                    let _ = c.wait().await;
                }
                adopted = None;
                *OWNED_PID.lock().unwrap() = None;
                let _ = store.set_setting(K_POT_SERVER_PID, "");
                let _ = store.set_setting(K_POT_SERVER_PID_START, "");
                set_status(PotMode::Down, None);
            }
            Action::Spawn => {
                // A previous attempt this episode already failed and something
                // is STILL squatting the port without answering /ping (a
                // wedged or half-bound leftover server — e.g. one stuck deep
                // in BotGuard work, or an orphan from a crashed run). Spawning
                // under it just EADDRINUSE-loops forever, so clear it first.
                // Never reached on the first attempt of an episode, and never
                // for a server that answers pings (that's Action::Healthy).
                if consecutive_failures >= 1
                    && let Some(pid) = crate::platform::pid_listening_on(base_url_port(&cfg.base_url))
                    && pid != std::process::id()
                    && owned_pid_now(&child, adopted) != Some(pid)
                {
                    // Last chance before the kill: one generous ping. A server
                    // deep in BotGuard work answers slowly, not never — killing
                    // it mid-token-storm just replaces a warm server with a
                    // cold one that's even slower, which is how a kill/respawn
                    // loop starts. Only a listener that stays silent for a
                    // full 10s is treated as wedged.
                    if let Some(pi) = ping_with(&cfg.base_url, Duration::from_secs(10)).await {
                        info!(
                            pid,
                            version = %pi.version,
                            "port owner answered a slow ping — adopting as external instead of killing"
                        );
                        consecutive_failures = 0;
                        ping_failures = 0;
                        notified_down = false;
                        set_status(PotMode::External, Some(pi));
                        sleep_or_nudge(POLL_INTERVAL, &shutdown).await;
                        continue;
                    }
                    warn!(
                        pid,
                        "a PO token server is squatting the port without answering /ping — \
                         killing it before respawning"
                    );
                    crate::platform::kill_process_tree(pid);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                let main_js = std::path::Path::new(&cfg.dir).join("main.js");
                if !crate::iomon::fs::is_file_sync(crate::iomon::Cat::Startup, &main_js) {
                    if !warned_missing_dir {
                        warn!(
                            "PO token server not started: {} not found (set the server \
                             directory in Settings → Downloads → GVS PO token server)",
                            main_js.display()
                        );
                        warned_missing_dir = true;
                    }
                    set_status(
                        PotMode::Failed { reason: format!("{} not found", main_js.display()) },
                        None,
                    );
                    sleep_or_nudge(IDLE_INTERVAL, &shutdown).await;
                    continue;
                }
                set_status(PotMode::Starting, None);
                match spawn_server(&cfg, !truncated_this_run) {
                    Ok(mut c) => {
                        truncated_this_run = true;
                        let pid = c.id().unwrap_or(0);
                        info!(pid, port = base_url_port(&cfg.base_url), "PO token server starting");
                        // Persist for re-adoption by the next app run.
                        let _ = store.set_setting(K_POT_SERVER_PID, &pid.to_string());
                        let _ = store.set_setting(
                            K_POT_SERVER_PID_START,
                            &crate::platform::process_start_time(pid)
                                .map(|t| t.to_string())
                                .unwrap_or_default(),
                        );
                        *OWNED_PID.lock().unwrap() = Some(pid);
                        // Give it a startup window to answer /ping.
                        let mut up = None;
                        let mut child_exited = false;
                        let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
                        while tokio::time::Instant::now() < deadline {
                            tokio::time::sleep(Duration::from_secs(1)).await;
                            if let Ok(Some(status)) = c.try_wait() {
                                warn!("PO token server exited during startup ({status})");
                                child_exited = true;
                                break;
                            }
                            up = ping(&cfg.base_url).await;
                            if up.is_some() {
                                // Only creditable to OUR child if it's still
                                // alive — a child that lost the port race
                                // exits, and the answer then came from whoever
                                // holds the port. Claiming "up pid=<child>"
                                // for that (as this used to) meant the
                                // watchdog next tick saw its "managed server"
                                // exit and respawned in a loop, filling the
                                // log — the 2026-07-31 relaunch churn.
                                if matches!(c.try_wait(), Ok(Some(_))) {
                                    child_exited = true;
                                }
                                break;
                            }
                        }
                        if child_exited && up.is_none() {
                            // The child is gone; if something else answers the
                            // port, that's an adoptable server, not a failure.
                            up = ping(&cfg.base_url).await;
                        }
                        match up {
                            Some(pi) if !child_exited => {
                                info!(pid, version = %pi.version, "PO token server up");
                                child = Some(c);
                                consecutive_failures = 0;
                                ping_failures = 0;
                                notified_down = false;
                                set_status(PotMode::Managed { pid }, Some(pi));
                                sleep_or_nudge(POLL_INTERVAL, &shutdown).await;
                            }
                            Some(pi) => {
                                // Our child died to the port race but a live
                                // server IS answering there — use it instead
                                // of counting a failure and looping.
                                info!(
                                    version = %pi.version,
                                    "another PO token server already answers on this port — \
                                     using it (external)"
                                );
                                let _ = c.wait().await;
                                *OWNED_PID.lock().unwrap() = None;
                                let _ = store.set_setting(K_POT_SERVER_PID, "");
                                let _ = store.set_setting(K_POT_SERVER_PID_START, "");
                                consecutive_failures = 0;
                                ping_failures = 0;
                                notified_down = false;
                                set_status(PotMode::External, Some(pi));
                                sleep_or_nudge(POLL_INTERVAL, &shutdown).await;
                            }
                            None => {
                                // Startup failed: kill the half-started child (it
                                // never answered, so nothing depends on it yet).
                                crate::platform::kill_process_tree(pid);
                                let _ = c.wait().await;
                                *OWNED_PID.lock().unwrap() = None;
                                let _ = store.set_setting(K_POT_SERVER_PID, "");
                                let _ = store.set_setting(K_POT_SERVER_PID_START, "");
                                spawn_failed(
                                    &events,
                                    &mut consecutive_failures,
                                    &mut notified_down,
                                    "started but never answered /ping",
                                    storm_since.is_some(),
                                );
                                sleep_or_nudge(spawn_backoff(consecutive_failures), &shutdown).await;
                            }
                        }
                    }
                    Err(e) => {
                        spawn_failed(
                            &events,
                            &mut consecutive_failures,
                            &mut notified_down,
                            &format!("spawn failed: {e}"),
                            storm_since.is_some(),
                        );
                        sleep_or_nudge(spawn_backoff(consecutive_failures), &shutdown).await;
                    }
                }
            }
        }
    }
}

/// Record a failed spawn attempt: status, log, and (once per down-episode) an
/// in-app 🔔 notification. `storm` = a rejection storm is in progress: the
/// server is almost certainly saturated rather than down, so the log line
/// says that instead, the status reads "saturated", and the misleading
/// "server not responding" error notification is skipped entirely — the
/// storm already raised its own, accurate 🔔 (2026-07-31 20:17: a relaunch
/// mid-storm toasted "started but never answered /ping" about a server that
/// was busily minting the whole time).
fn spawn_failed(
    events: &crate::events::EventTx,
    consecutive_failures: &mut u32,
    notified_down: &mut bool,
    reason: &str,
    storm: bool,
) {
    *consecutive_failures += 1;
    if storm {
        warn!(
            attempts = *consecutive_failures,
            "PO token server didn't answer ({reason}) — likely saturated by the ongoing \
             PO-token rejection storm (every mint needs a full attestation challenge), \
             not down; will keep re-checking (log: {})",
            log_path().display()
        );
        set_status(
            PotMode::Failed { reason: "saturated (rejection storm) — retrying".to_string() },
            None,
        );
        return;
    }
    warn!(
        attempts = *consecutive_failures,
        "PO token server failed to start: {reason} — SABR captures will lack GVS PO tokens \
         until it's up (log: {})",
        log_path().display()
    );
    set_status(PotMode::Failed { reason: reason.to_string() }, None);
    if !*notified_down {
        *notified_down = true;
        let _ = events.send(crate::events::AppEvent::Error {
            context: "PO token server".to_string(),
            message: format!(
                "{reason} — YouTube SABR captures may fail with PO-token errors until it's \
                 running (see Settings → Downloads → GVS PO token server)"
            ),
        });
    }
}

fn owned_pid_now(child: &Option<tokio::process::Child>, adopted: Option<u32>) -> Option<u32> {
    child.as_ref().and_then(|c| c.id()).or(adopted)
}

async fn sleep_or_nudge(dur: Duration, shutdown: &Arc<AtomicBool>) {
    let step = Duration::from_millis(500);
    let mut remaining = dur;
    loop {
        if shutdown.load(Ordering::SeqCst) {
            return;
        }
        let chunk = remaining.min(step);
        tokio::select! {
            _ = tokio::time::sleep(chunk) => {}
            _ = nudge_handle().notified() => return,
        }
        remaining = remaining.saturating_sub(chunk);
        if remaining.is_zero() {
            return;
        }
    }
}

/// Launch `node main.js -p <port>` in the server dir, windowless, with
/// stdout+stderr appended to [`log_path`] (truncated on the first spawn of
/// this app run only — later restarts keep the previous attempt's evidence).
fn spawn_server(cfg: &PotConfig, truncate_log: bool) -> std::io::Result<tokio::process::Child> {
    use crate::iomon::Cat;
    let path = log_path();
    if truncate_log {
        let _ = crate::iomon::fs::create_sync(Cat::ToolLog, &path)?;
    }
    let out = crate::iomon::fs::open_with_sync(Cat::ToolLog, &path, |o| {
        o.create(true).append(true);
    })?;
    let err = out.try_clone()?;
    let mut cmd = tokio::process::Command::new(&cfg.node);
    cmd.arg("main.js")
        .arg("-p")
        .arg(base_url_port(&cfg.base_url).to_string())
        .current_dir(&cfg.dir)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);
    #[cfg(windows)]
    {
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.spawn()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pot_base_url_parses_default_args() {
        assert_eq!(
            pot_base_url("youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416"),
            "http://127.0.0.1:4416"
        );
    }

    #[test]
    fn pot_base_url_custom_port_and_trailing_fields() {
        assert_eq!(
            pot_base_url("youtubepot-bgutilhttp:base_url=http://localhost:9999;other=1"),
            "http://localhost:9999"
        );
        assert_eq!(pot_base_url("base_url=http://10.0.0.2:4416/"), "http://10.0.0.2:4416");
    }

    #[test]
    fn pot_base_url_falls_back_when_absent_or_empty() {
        assert_eq!(pot_base_url(""), DEFAULT_BASE_URL);
        assert_eq!(pot_base_url("youtubepot-bgutilhttp:disable_innertube=1"), DEFAULT_BASE_URL);
        assert_eq!(pot_base_url("base_url="), DEFAULT_BASE_URL);
    }

    #[test]
    fn base_url_port_parses() {
        assert_eq!(base_url_port("http://127.0.0.1:4416"), 4416);
        assert_eq!(base_url_port("http://localhost:9999"), 9999);
        assert_eq!(base_url_port("http://host.example/path"), 4416, "no port ⇒ default");
        assert_eq!(base_url_port("127.0.0.1:8080"), 8080, "schemeless");
    }

    #[test]
    fn watch_action_decision_table() {
        use Action::*;
        use Desired::*;
        // desired, autostart, ping_ok, server_present, ping_failures => expected
        let table = [
            (Auto, true, true, true, 0, Healthy),
            (Auto, true, true, false, 0, Healthy), // external server answering
            (Auto, true, false, false, 1, Spawn),  // nothing alive: no grace
            // A live server (ours, adopted, OR a foreign listener on the
            // port) gets three missed pings of grace before any spawn — a
            // BotGuard mint pegs node for seconds, and both halves of the
            // 2026-07-31 incidents started with a premature spawn over a
            // busy-but-healthy server.
            (Auto, true, false, true, 1, Grace),
            (Auto, true, false, true, 2, Grace),
            (Auto, true, false, true, 3, Spawn),
            (Auto, false, false, false, 1, Idle),
            (Auto, false, true, false, 0, Idle), // external running, we don't manage
            (Auto, false, false, true, 1, Kill), // autostart turned off with child up
            (ForcedOn, false, false, false, 1, Spawn),
            (ForcedOn, false, true, false, 0, Healthy),
            (ForcedOn, false, false, true, 2, Grace),
            (ForcedOff, true, true, true, 0, Kill),
            (ForcedOff, true, true, false, 0, Idle), // external stays untouched
            (ForcedOff, true, false, false, 1, Idle),
        ];
        for (desired, autostart, ping_ok, server_present, ping_failures, expected) in table {
            assert_eq!(
                watch_action(desired, autostart, ping_ok, server_present, ping_failures),
                expected,
                "desired={desired:?} autostart={autostart} ping={ping_ok} \
                 present={server_present} ping_failures={ping_failures}"
            );
        }
    }

    #[test]
    fn spawn_backoff_grows_and_caps() {
        assert_eq!(spawn_backoff(1), Duration::from_secs(30));
        assert_eq!(spawn_backoff(2), Duration::from_secs(60));
        assert_eq!(spawn_backoff(3), Duration::from_secs(120));
        assert_eq!(spawn_backoff(4), Duration::from_secs(300));
        assert_eq!(spawn_backoff(99), Duration::from_secs(300));
    }
}
