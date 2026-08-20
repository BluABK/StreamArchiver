//! Tool binary resolution (yt-dlp/SABR/custom tools), SABR config, and
//! auth source resolution.

use super::*;

/// Default SABR format selector when the setting is unset/empty.
pub const SABR_DEFAULT_FORMAT: &str = "ba[protocol=sabr]+bv[protocol=sabr]";
/// Default SABR `--extractor-args` when the setting is unset/empty.
pub const SABR_DEFAULT_EXTRACTOR_ARGS: &str =
    "youtube:formats=duplicate,missing_pot;player-client=web;webpage-client=web";
/// Default PO-token-provider `--extractor-args` (bgutil HTTP server on its default
/// port). Passed as a *separate* `--extractor-args` entry because it targets a
/// different extractor key (`youtubepot-bgutilhttp`) than the `youtube:` args.
/// Used when the setting key has never been written; an explicit empty value
/// disables it (rely on the plugin's own auto-detection instead).
pub const SABR_DEFAULT_POT_ARGS: &str = "youtubepot-bgutilhttp:base_url=http://127.0.0.1:4416";
/// Consecutive from-start SABR stalls ("not near live head") tolerated with
/// deep-rewind enabled before giving up and falling back to live-edge capture.
/// Deep-rewind extends the DVR window, so the *first* stall may be transient;
/// but a persistent stall repeats every attempt (each re-downloading the opening
/// — observed ~190 MiB — then dying), so we tolerate one retry then fall back.
/// With deep-rewind off a stall is a true window expiry and we fall back at once.
pub(super) const SABR_STALL_FALLBACK_TRIES: u32 = 2;
/// Default DASH-companion format selector when the setting is unset/empty.
pub const DASH_DEFAULT_FORMAT: &str = "bestvideo+bestaudio/best";

/// Default PO-token fallback client: after a take dies to a rejected GVS PO
/// token, the retry captures via this yt-dlp client instead of `web`. The
/// `tv` (TVHTML5) client has no GVS PO-token policy at all — no token is
/// minted or attached, so a platform-side rejection wave can't touch it.
/// Verified live 2026-07-31 during an active wave: full-speed from-start
/// SABR capture (same 140/303 itags, deep rewind to sq0) while every web
/// token was refused with ATTESTATION_REQUIRED.
pub const SABR_PO_FALLBACK_DEFAULT_CLIENT: &str = "tv";
/// Default PRIMARY yt-dlp client for public YouTube live SABR captures. `tv`
/// (TVHTML5) has no GVS PO-token policy — smart TVs can't run BotGuard, so
/// YouTube exempts the client — which makes it immune to the
/// ATTESTATION_REQUIRED rejection waves that have hit `web` captures daily
/// (see the Warnings history): with `web` primary, every wave burned a doomed
/// take before the per-take fallback switched to `tv` anyway. Members-only
/// broadcasts still capture via `web` + cookies (entitlement lives on the
/// account) — see `Supervisor::apply_client_policy`.
pub const SABR_PRIMARY_DEFAULT_CLIENT: &str = "tv";
/// Settings key for the primary SABR client. Absent ⇒ the default (`tv`);
/// present but empty ⇒ leave the preset's own client (`web`) untouched —
/// same present-vs-absent semantics as `ytdlp_sabr_po_fallback_client`.
pub const K_SABR_PRIMARY_CLIENT: &str = "ytdlp_sabr_primary_client";

/// The configured primary SABR client ("" = keep the preset's client).
pub(crate) fn sabr_primary_client(store: &Store) -> String {
    match store.get_setting(K_SABR_PRIMARY_CLIENT) {
        Ok(Some(v)) => v.trim().to_string(),
        _ => SABR_PRIMARY_DEFAULT_CLIENT.to_string(),
    }
}

/// Settings key: try public YouTube content **anonymously as a last resort**,
/// after the cookie path has failed repeatedly.
///
/// This used to mean "always capture public YouTube anonymously", and was on
/// by default: cookies change what a GVS PO token must be bound to (account
/// identity instead of the anonymous visitor data bgutil mints for) and put
/// the account inside YouTube's attestation experiments. Sound reasoning, and
/// it held until 2026-08-18, when YouTube began refusing *every* anonymous
/// request from this network — both clients, measured — and anonymous-first
/// meant capturing nothing at all.
///
/// So the ladder inverted. Cookies are the normal path; anonymity is the rung
/// tried only once cookies have failed [`ANON_FALLBACK_AFTER`] times in a row
/// with nothing captured, on the theory that whatever is refusing the account
/// might not refuse a stranger. Off means never try it.
pub const K_YT_ANON_PUBLIC: &str = "youtube_anonymous_fallback";

/// Consecutive failed captures on the cookie path before a monitor is allowed
/// one anonymous attempt. Low enough to be reached within a broadcast, high
/// enough that an ordinary blip (a stall, a restart) never spends it.
pub const ANON_FALLBACK_AFTER: u32 = 3;

/// Whether an anonymous last-resort attempt is permitted at all. Default ON —
/// as a *fallback* it costs nothing until the cookie path is already failing.
pub(crate) fn yt_anonymous_fallback(store: &Store) -> bool {
    match store.get_setting(K_YT_ANON_PUBLIC) {
        Ok(Some(v)) => v != "0",
        _ => true,
    }
}

/// Pick the yt-dlp client for one YouTube monitor's SABR config (callers gate
/// on platform + `sabr.usable()`).
///
/// **The client is a function of the auth, not an independent choice.** Only
/// two of the four combinations are coherent, measured live against a public
/// stream on 2026-08-20:
///
/// | client | auth | result |
/// |---|---|---|
/// | `tv` | anonymous | `Sign in to confirm you're not a bot` |
/// | `tv` | cookies | `The page needs to be reloaded` |
/// | `web` | anonymous | `Sign in to confirm you're not a bot` |
/// | `web` | cookies | **works** |
///
/// So cookies force `web`, full stop: `tv` cannot load a page with cookies
/// attached. The configured primary (default `tv`) applies only to an
/// anonymous attempt, which is the case it exists for — `tv` has no GVS
/// PO-token policy, so an attestation rejection wave can't touch it.
///
/// This used to be two decisions taken apart: auth here, client there,
/// reconciled only inside the supervisor's bot-wall special case. When the
/// auth ladder inverted (cookies became the normal rung, anonymity the last
/// resort) that left the *ordinary* public capture running cookies + `tv` —
/// the one combination that fails both ways — and the live-edge preview with
/// it. Pairing them here is the whole fix; the special case is now just the
/// general rule.
pub(crate) fn apply_yt_client_policy(
    store: &Store,
    monitor_id: i64,
    sabr: &mut SabrConfig,
    auth: &AuthSource,
) {
    // Cookies present (the normal path), or a members-only monitor whose
    // entitlement lives on the account: `web` is the only client that works.
    // This overrides hand-written extractor-args deliberately — respecting a
    // custom `player-client=tv` here would mean shipping a combination that
    // cannot fetch anything.
    if !matches!(auth, AuthSource::None) || store.monitor_members_only(monitor_id) {
        sabr.extractor_args = with_player_client(&sabr.extractor_args, "web");
        return;
    }
    // "Custom" has to mean DIFFERENT FROM THE DEFAULT, not merely present.
    // The Settings form writes every field on save, so the built-in default
    // gets persisted verbatim the first time anyone saves anything at all — and
    // a non-empty test then reads the app's own default back as a hand-written
    // override and stops applying the primary-client policy, permanently and
    // silently.
    //
    // Panko, 2026-08-17: `ytdlp_sabr_extractor_args` held exactly
    // `SABR_DEFAULT_EXTRACTOR_ARGS`, so `primary_client = tv` was configured and
    // ignored, every public capture stayed pinned to `web` (which requires a GVS
    // PO token), and the take died at frag 309/555 when the token was rejected.
    let stored = setting_str(store, "ytdlp_sabr_extractor_args");
    let stored = stored.trim();
    let custom_xargs = !stored.is_empty() && stored != SABR_DEFAULT_EXTRACTOR_ARGS;
    let primary = sabr_primary_client(store);
    if !custom_xargs && !primary.is_empty() {
        sabr.extractor_args = with_player_client(&sabr.extractor_args, &primary);
    }
}

/// Strip account cookies from a resolved auth for PUBLIC YouTube content
/// when the 🕶 anonymous switch is on; members-only monitors keep theirs.
/// Callers gate on platform. Returns whether the auth was anonymized so the
/// caller can log it in its own context.
pub(crate) fn yt_public_auth(store: &Store, monitor_id: i64, auth: &mut AuthSource) -> bool {
    if !matches!(auth, AuthSource::None)
        && yt_anonymous_fallback(store)
        && !store.monitor_members_only(monitor_id)
    {
        *auth = AuthSource::None;
        true
    } else {
        false
    }
}
/// Settings key for the PO-token fallback client. Absent (never written) ⇒
/// the default (`tv`); present but empty ⇒ fallback disabled (always web,
/// with the escalating PO cooldown instead) — same present-vs-absent
/// semantics as `ytdlp_sabr_pot_args`.
pub const K_SABR_PO_FALLBACK_CLIENT: &str = "ytdlp_sabr_po_fallback_client";

/// The configured PO-token fallback client ("" = disabled). Shared by
/// [`load_ytdlp_bins`] and the supervisor's backoff logic (which must know
/// whether a rejected take still has a fallback left before escalating the
/// cooldown to 5-15 minutes).
pub(crate) fn sabr_po_fallback_client(store: &Store) -> String {
    match store.get_setting(K_SABR_PO_FALLBACK_CLIENT) {
        Ok(Some(v)) => v.trim().to_string(),
        _ => SABR_PO_FALLBACK_DEFAULT_CLIENT.to_string(),
    }
}

/// Rewrite the `player-client=` entry inside a `youtube:...` extractor-args
/// value (appending one if absent), leaving every other arg untouched.
pub(crate) fn with_player_client(extractor_args: &str, client: &str) -> String {
    let Some((ns, rest)) = extractor_args.split_once(':') else {
        return format!("{extractor_args}:player-client={client}");
    };
    let mut found = false;
    let mut parts: Vec<String> = rest
        .split(';')
        .map(|p| {
            if p.trim_start().starts_with("player-client=") {
                found = true;
                format!("player-client={client}")
            } else {
                p.to_string()
            }
        })
        .collect();
    if !found {
        parts.push(format!("player-client={client}"));
    }
    format!("{ns}:{}", parts.join(";"))
}

/// SABR (Server Adaptive Bit Rate) capture configuration for YouTube. SABR is the
/// only protocol that reliably supports `--live-from-start` today, but it lives in
/// a yt-dlp dev fork (a separate binary). See the YouTube SABR settings section.
#[derive(Clone, Debug, Default)]
pub struct SabrConfig {
    /// Master toggle (Settings). When false, YouTube capture-from-start uses the
    /// system binary's normal path.
    pub enabled: bool,
    /// Path to the SABR dev-build binary; empty ⇒ SABR unavailable.
    pub binary: String,
    /// Format selector injected by the preset (e.g. `ba[protocol=sabr]+bv[protocol=sabr]`).
    pub format: String,
    /// `--extractor-args` value injected by the preset.
    pub extractor_args: String,
    /// Manual raw args; when non-empty, replaces the format + extractor-args preset.
    pub raw_args: String,
    /// PO-token-provider `--extractor-args`, passed as its own `--extractor-args`
    /// entry (different extractor key than `extractor_args`). Empty ⇒ not passed.
    /// Applied regardless of the preset/`raw_args` choice (it's orthogonal to
    /// format selection).
    pub pot_args: String,
    /// GLOBAL default video codec/quality preference (a `-S` sort layered on the
    /// selector). A monitor's own pref overrides this unless it's `Inherit`.
    pub codec_pref: SabrCodecPref,
    /// GLOBAL raw `-S` string when `codec_pref == Custom`.
    pub codec_custom: String,
    /// yt-dlp client for the PO-token fallback retry ("" = disabled). Applied
    /// by the supervisor per-take (see `po_fallback_pending`), never here.
    pub po_fallback_client: String,
}

impl SabrConfig {
    /// True when SABR capture is configured and usable.
    pub(crate) fn usable(&self) -> bool {
        self.enabled && !self.binary.is_empty()
    }
}

/// A user-defined alternate yt-dlp-compatible binary (e.g. a personal fork or
/// a different dev build), selectable per-video download alongside the system
/// yt-dlp and the built-in SABR dev build. Uses the same yt-dlp argument
/// template as `Tool::YtDlp` — only the invoked program differs.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CustomTool {
    pub alias: String,
    pub path: String,
}

/// Settings key for the persisted custom-tools list (JSON-encoded
/// `Vec<CustomTool>`).
pub(super) const K_CUSTOM_TOOLS: &str = "custom_tools";

/// Reserved [`Video::tool_binary`] value selecting the built-in SABR dev build.
pub const TOOL_BINARY_SABR: &str = "sabr";

/// Load the user-defined custom tools list from settings.
pub fn load_custom_tools(store: &Store) -> Vec<CustomTool> {
    store
        .get_setting(K_CUSTOM_TOOLS)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the user-defined custom tools list to settings.
pub fn save_custom_tools(store: &Store, tools: &[CustomTool]) -> anyhow::Result<()> {
    store.set_setting(K_CUSTOM_TOOLS, &serde_json::to_string(tools)?)?;
    Ok(())
}

/// The yt-dlp-family binaries available to the supervisor: the system build
/// (PATH or an explicit path), the optional SABR dev build, and any
/// user-defined custom tools.
#[derive(Clone, Debug, Default)]
pub struct YtDlpBins {
    /// Explicit system yt-dlp path; empty ⇒ `yt-dlp` on PATH.
    pub system: String,
    pub sabr: SabrConfig,
    pub custom: Vec<CustomTool>,
}

impl YtDlpBins {
    /// The program name/path for the system yt-dlp.
    pub fn system_program(&self) -> String {
        if self.system.is_empty() {
            "yt-dlp".to_string()
        } else {
            self.system.clone()
        }
    }

    /// Resolve a [`Video::tool_binary`] value to the program to invoke: empty
    /// ⇒ the system yt-dlp, [`TOOL_BINARY_SABR`] ⇒ the SABR dev build, else a
    /// custom tool's path by alias. Falls back to the system yt-dlp if the
    /// SABR build isn't configured or the alias no longer exists.
    pub fn resolve_program(&self, tool_binary: &str) -> String {
        match tool_binary {
            "" => self.system_program(),
            TOOL_BINARY_SABR => {
                if self.sabr.binary.is_empty() {
                    self.system_program()
                } else {
                    self.sabr.binary.clone()
                }
            }
            alias => self
                .custom
                .iter()
                .find(|t| t.alias == alias)
                .map(|t| t.path.clone())
                .unwrap_or_else(|| self.system_program()),
        }
    }
}

/// Read a setting as a string, defaulting to empty when absent.
pub(super) fn setting_str(store: &Store, key: &str) -> String {
    store.get_setting(key).ok().flatten().unwrap_or_default()
}

/// Load the configured yt-dlp binaries + SABR preset from settings, applying the
/// built-in fallbacks for any empty preset fields.
pub(crate) fn load_ytdlp_bins(store: &Store) -> YtDlpBins {
    let enabled = store
        .get_setting("ytdlp_sabr_enabled")
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true);
    let fmt = setting_str(store, "ytdlp_sabr_format");
    let xargs = setting_str(store, "ytdlp_sabr_extractor_args");
    // Experimental deep-rewind: when on, append `enable_live_deep_rewind=true` to
    // the youtube extractor-args so SABR can rewind past YouTube's normal ~4h DVR
    // window (lets capture-from-start reach the start of a long stream instead of
    // stalling with "not near live head"). Dev-build-only feature; the upstream
    // code reads only the literal lowercase `true`. Off by default — a stock
    // yt-dlp would silently ignore it, and the upstream author marks it unstable.
    let deep_rewind = store
        .get_setting("ytdlp_sabr_deep_rewind")
        .ok()
        .flatten()
        .map(|v| v == "1")
        .unwrap_or(false);
    // PO-token args: absent (never written) ⇒ the bgutil default; present (even
    // empty) ⇒ honor it verbatim, so the user can deliberately disable it.
    let pot_args = match store.get_setting("ytdlp_sabr_pot_args") {
        Ok(Some(v)) => v,
        _ => SABR_DEFAULT_POT_ARGS.to_string(),
    };
    // Global codec/quality preference. Absent/unknown ⇒ Auto (yt-dlp default),
    // preserving prior behavior. (Only the per-monitor field uses `Inherit`.)
    let codec_pref = match SabrCodecPref::parse(&setting_str(store, "ytdlp_sabr_codec_pref")) {
        SabrCodecPref::Inherit => SabrCodecPref::Auto,
        other => other,
    };
    YtDlpBins {
        system: setting_str(store, "ytdlp_binary_path"),
        sabr: SabrConfig {
            enabled,
            binary: setting_str(store, "ytdlp_sabr_binary_path"),
            format: if fmt.is_empty() { SABR_DEFAULT_FORMAT.to_string() } else { fmt },
            extractor_args: {
                let base = if xargs.is_empty() {
                    SABR_DEFAULT_EXTRACTOR_ARGS.to_string()
                } else {
                    xargs
                };
                // Append under the same `youtube:` namespace (`;`-separated).
                // Guard against a double-append if the user already added it to
                // the extractor-args field by hand.
                if deep_rewind && !base.contains("enable_live_deep_rewind") {
                    format!("{base};enable_live_deep_rewind=true")
                } else {
                    base
                }
            },
            raw_args: setting_str(store, "ytdlp_sabr_raw_args"),
            pot_args,
            codec_pref,
            codec_custom: setting_str(store, "ytdlp_sabr_codec_custom"),
            po_fallback_client: sabr_po_fallback_client(store),
        },
        custom: load_custom_tools(store),
    }
}

/// Resolve a monitor's effective SABR format-sort (`-S` value): the monitor's own
/// codec preference, or the global default when the monitor is set to `Inherit`.
/// `""` = add no `-S` (yt-dlp's default codec preference).
pub(super) fn resolve_sabr_sort(m: &Monitor, sabr: &SabrConfig) -> String {
    let (pref, custom) = if m.sabr_codec_pref == SabrCodecPref::Inherit {
        (sabr.codec_pref, sabr.codec_custom.as_str())
    } else {
        (m.sabr_codec_pref, m.sabr_codec_custom.as_str())
    };
    pref.sort_arg(custom)
}

/// Load the DASH-companion format selector (dual capture), with fallback.
pub(super) fn load_dash_format(store: &Store) -> String {
    let f = setting_str(store, "ytdlp_dash_format");
    if f.is_empty() { DASH_DEFAULT_FORMAT.to_string() } else { f }
}

/// Resolved download authentication for a monitor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AuthSource {
    None,
    /// yt-dlp `--cookies-from-browser <browser>`.
    CookiesBrowser(String),
    /// yt-dlp `--cookies <path>`.
    CookiesFile(String),
    /// Twitch `--twitch-api-header=Authorization=OAuth <token>` (streamlink).
    Token(String),
}

/// Resolve the effective auth for a monitor from its override + the global default.
pub fn resolve_auth(
    m: &MonitorWithChannel,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    resolve_auth_for(
        m.monitor.auth_kind,
        &m.monitor.auth_value,
        global_method,
        global_browser,
    )
}

/// Resolve an auth source from an `(auth_kind, auth_value)` pair plus the global
/// default — shared by monitors and on-demand videos.
pub fn resolve_auth_for(
    auth_kind: AuthKind,
    auth_value: &str,
    global_method: &str,
    global_browser: &str,
) -> AuthSource {
    let val = auth_value.trim();
    let browser = global_browser.trim();
    match auth_kind {
        AuthKind::Inherit => match global_method {
            "cookies" if !browser.is_empty() => AuthSource::CookiesBrowser(browser.to_string()),
            _ => AuthSource::None,
        },
        AuthKind::Disabled => AuthSource::None,
        AuthKind::CookiesBrowser => {
            let b = if val.is_empty() { browser } else { val };
            if b.is_empty() {
                AuthSource::None
            } else {
                AuthSource::CookiesBrowser(b.to_string())
            }
        }
        AuthKind::CookiesFile if !val.is_empty() => AuthSource::CookiesFile(val.to_string()),
        AuthKind::Token if !val.is_empty() => AuthSource::Token(val.to_string()),
        _ => AuthSource::None,
    }
}


#[cfg(test)]
mod tests {
    #![allow(clippy::disallowed_methods)]

    use super::*;
    #[allow(unused_imports)]
    use crate::models::{Channel, Container, DetectionMethod, Monitor, Tool};
    #[allow(unused_imports)]
    use crate::downloader::test_util::*;

    #[test]
    fn resolve_auth_precedence() {
        // Inherit + global cookies -> browser cookies.
        let mut r = row(Tool::YtDlp, Container::Mkv, Platform::YouTube);
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::CookiesBrowser("chrome".into())
        );
        // Per-channel override wins over global.
        r.monitor.auth_kind = AuthKind::Token;
        r.monitor.auth_value = "tok".into();
        assert_eq!(
            resolve_auth(&r, "cookies", "chrome"),
            AuthSource::Token("tok".into())
        );
        // Disabled forces none even if a global default exists.
        r.monitor.auth_kind = AuthKind::Disabled;
        assert_eq!(resolve_auth(&r, "cookies", "chrome"), AuthSource::None);
    }
    #[test]
    fn explicit_system_binary_path_is_used() {
        let bins = YtDlpBins {
            system: "C:/tools/yt-dlp.exe".into(),
            ..Default::default()
        };
        let plan = build_plan(
            &row(Tool::YtDlp, Container::Mkv, Platform::YouTube),
            1_700_000_000,
            &AuthSource::None,
            &[],
            None,
            "",
            None,
            0,
            &bins,
        );
        assert_eq!(plan.program, "C:/tools/yt-dlp.exe");
    }
    #[test]
    fn resolve_program_falls_back_to_system_for_unknown_binary() {
        let bins = YtDlpBins::default();
        // No SABR build configured and no matching custom tool -> system yt-dlp.
        assert_eq!(bins.resolve_program(TOOL_BINARY_SABR), "yt-dlp");
        assert_eq!(bins.resolve_program("no-such-alias"), "yt-dlp");
        assert_eq!(bins.resolve_program(""), "yt-dlp");
    }

    #[test]
    fn persisting_the_default_extractor_args_does_not_disable_tv_primary() {
        // The regression: saving Settings writes every field, so the built-in
        // default lands in the store verbatim. Read back as "custom", it pinned
        // every public capture to `web` — which needs a GVS PO token — while
        // `primary_client = tv` sat there being ignored.
        let store = Store::open_in_memory().unwrap();
        let mut sabr = SabrConfig {
            extractor_args: SABR_DEFAULT_EXTRACTOR_ARGS.to_string(),
            ..Default::default()
        };
        store
            .set_setting("ytdlp_sabr_extractor_args", SABR_DEFAULT_EXTRACTOR_ARGS)
            .unwrap();
        apply_yt_client_policy(&store, 1, &mut sabr, &AuthSource::None);
        assert!(
            sabr.extractor_args.contains("player-client=tv"),
            "the stored default must not read as a hand-written override: {}",
            sabr.extractor_args
        );
        // Everything else in the preset survives the swap.
        assert!(sabr.extractor_args.contains("webpage-client=web"));
        assert!(sabr.extractor_args.contains("missing_pot"));
    }

    /// Only two of the four (client, auth) combinations work. Measured live
    /// against a public YouTube stream on 2026-08-20:
    ///
    /// * `tv`  + anonymous -> "Sign in to confirm you're not a bot"
    /// * `tv`  + cookies   -> "The page needs to be reloaded"
    /// * `web` + anonymous -> "Sign in to confirm you're not a bot"
    /// * `web` + cookies   -> works
    ///
    /// So the client cannot be chosen independently of the auth. It was, and
    /// when the auth ladder inverted (cookies became the normal rung) that
    /// left every ordinary public capture — and every live-edge play — on
    /// cookies + `tv`, which fails both ways.
    #[test]
    fn cookies_force_the_web_client_whatever_the_primary_says() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting("ytdlp_sabr_primary_client", "tv").unwrap();
        let preset = || SabrConfig {
            extractor_args: SABR_DEFAULT_EXTRACTOR_ARGS.to_string(),
            ..Default::default()
        };

        // Anonymous: the primary is the point — `tv` has no GVS PO-token
        // policy, so an attestation wave cannot touch it.
        let mut anon = preset();
        apply_yt_client_policy(&store, 1, &mut anon, &AuthSource::None);
        assert!(anon.extractor_args.contains("player-client=tv"), "{}", anon.extractor_args);

        // Cookies of either kind: `web`, because `tv` cannot load a page with
        // cookies attached.
        for auth in [
            AuthSource::CookiesBrowser("firefox".into()),
            AuthSource::CookiesFile("c.txt".into()),
        ] {
            let mut with_cookies = preset();
            apply_yt_client_policy(&store, 1, &mut with_cookies, &auth);
            assert!(
                with_cookies.extractor_args.contains("player-client=web"),
                "cookies must force web, got {}",
                with_cookies.extractor_args
            );
        }
    }

    /// The pairing is a correctness constraint, not a preference, so it wins
    /// over hand-written extractor-args: honouring a custom `player-client=tv`
    /// alongside cookies would mean shipping a combination that cannot fetch.
    #[test]
    fn cookies_override_even_hand_written_extractor_args() {
        let store = Store::open_in_memory().unwrap();
        let hand = "youtube:formats=duplicate;player-client=tv";
        store.set_setting("ytdlp_sabr_extractor_args", hand).unwrap();
        let mut sabr = SabrConfig { extractor_args: hand.to_string(), ..Default::default() };
        apply_yt_client_policy(&store, 1, &mut sabr, &AuthSource::CookiesBrowser("firefox".into()));
        assert!(sabr.extractor_args.contains("player-client=web"));
        // Everything else the user wrote survives.
        assert!(sabr.extractor_args.contains("formats=duplicate"));
    }

    #[test]
    fn genuinely_hand_written_extractor_args_are_still_respected_verbatim() {
        // The behaviour the non-empty test was reaching for, kept intact.
        let store = Store::open_in_memory().unwrap();
        let hand = "youtube:formats=duplicate;player-client=web_safari";
        let mut sabr = SabrConfig {
            extractor_args: hand.to_string(),
            ..Default::default()
        };
        store.set_setting("ytdlp_sabr_extractor_args", hand).unwrap();
        apply_yt_client_policy(&store, 1, &mut sabr, &AuthSource::None);
        assert_eq!(sabr.extractor_args, hand, "a real override must not be rewritten");
    }

    #[test]
    fn an_unset_extractor_args_setting_still_gets_the_primary_client() {
        let store = Store::open_in_memory().unwrap();
        let mut sabr = SabrConfig {
            extractor_args: SABR_DEFAULT_EXTRACTOR_ARGS.to_string(),
            ..Default::default()
        };
        apply_yt_client_policy(&store, 1, &mut sabr, &AuthSource::None);
        assert!(sabr.extractor_args.contains("player-client=tv"));
    }

    #[test]
    fn with_player_client_swaps_only_the_client() {
        // The default preset: both player-client and webpage-client present —
        // only the former is swapped, everything else survives verbatim.
        assert_eq!(
            with_player_client(SABR_DEFAULT_EXTRACTOR_ARGS, "tv"),
            "youtube:formats=duplicate,missing_pot;player-client=tv;webpage-client=web"
        );
        // Deep-rewind (or any other appended arg) is untouched.
        assert_eq!(
            with_player_client(
                "youtube:player-client=web,web_safari;enable_live_deep_rewind=true",
                "tv"
            ),
            "youtube:player-client=tv;enable_live_deep_rewind=true"
        );
        // No player-client in the preset: one is appended.
        assert_eq!(
            with_player_client("youtube:formats=duplicate", "tv"),
            "youtube:formats=duplicate;player-client=tv"
        );
    }
}
