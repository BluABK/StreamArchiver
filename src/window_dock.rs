//! Docking the chat popup to a running player window — `<video>|<chat>` as
//! one unit, the way the website lays it out.
//!
//! A single background thread polls both windows' DWM frame rectangles every
//! [`TICK`] and mirrors changes **both ways**: drag the player and the chat
//! follows, drag the chat and the player follows, minimize either and both
//! minimize. All geometry runs OS-side (`SetWindowPos` on both HWNDs, the
//! chat viewport's included) in physical virtual-desktop pixels. The egui
//! route — `ViewportCommand::OuterPosition` — is deliberately not used for
//! following: it is denominated in points (per-viewport `pixels_per_point`,
//! which differs across mixed-DPI monitors) and round-trips the winit event
//! loop, so the pair would visibly rubber-band during a drag.
//!
//! Polling was chosen over `SetWinEventHook` on purpose: two DWM reads and
//! two `IsIconic` calls per dock per 20 ms is unmeasurable, needs no message
//! pump and no extra crate feature, cannot desync on a missed event, and the
//! hook cannot be pid-scoped up front anyway (on the Streamlink path the
//! player's real pid is only learned after Streamlink spawns it).
//!
//! The egui side talks to this module only through free functions over
//! global registries — same pattern as `OPEN_PLAYERS` in `ui::player` — so
//! the chat popup's deferred render closure (which has no `&mut self`) can
//! toggle a dock directly.
//!
//! What the thread never does: touch a window while either half is
//! minimized, fight the player's own fullscreen (it suspends and re-snaps
//! after), or move anything when nothing changed (every write is guarded by
//! expected-rect bookkeeping, see [`decide`]).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use crate::display::PixelRect;

/// Poll cadence for the follow loop. Drag lag is at most one tick, which
/// reads as "magnetic" rather than "towed".
const TICK: Duration = Duration::from_millis(20);

/// How long a dock may sit waiting for its windows before it is dropped.
/// Streamlink can retry for a while before mpv exists (cf. the 60 s IPC
/// connect timeout the title updater allows the same path).
const PENDING_TIMEOUT: Duration = Duration::from_secs(60);

/// How long a vanished chat HWND is re-searched before the dock gives up.
/// The deferred-viewport machinery can recreate the popup's native window
/// (fresh HWND, same title); that recreation must re-resolve, never undock.
const CHAT_REFIND_GRACE: Duration = Duration::from_secs(5);

/// Two rects within this many pixels on every edge count as "didn't move".
/// Absorbs DWM rounding on mixed-DPI monitors without masking a real drag.
const TOLERANCE: i32 = 2;

/// Which side of the player the chat pane sticks to.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DockSide {
    /// Video left, chat right — the website's own arrangement.
    #[default]
    Right,
    Left,
}

impl DockSide {
    pub fn from_setting(v: &str) -> Self {
        if v.eq_ignore_ascii_case("left") { DockSide::Left } else { DockSide::Right }
    }
}

/// What the chat toolbar's 🔗 toggle shows.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DockStatus {
    Undocked,
    /// Requested, still waiting for one of the two windows to exist.
    Pending,
    Docked,
}

/// One live player process for a monitor. `root_pid` may be a launcher
/// (Streamlink) rather than the player itself — window resolution walks the
/// whole process tree, so that distinction never matters here.
#[derive(Clone, Copy, Debug)]
struct PlayerProc {
    root_pid: u32,
    /// Monotonic spawn counter, global across all monitors. Auto-dock fires
    /// once per generation, which is what makes "dock on play" trigger for
    /// each new player without ever re-docking one the user detached.
    generation: u64,
    /// Live-edge play (true) vs a local file / VOD (false). Only live plays
    /// participate in auto-open — popping a live chat next to a week-old
    /// recording would be noise.
    live: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DockPhase {
    Pending,
    Active,
    /// The player went fullscreen; stop mirroring, leave the chat be, and
    /// re-snap when it comes back. Fighting mpv's `f` would be unwinnable.
    SuspendedFullscreen,
}

struct DockState {
    chat_title: String,
    side: DockSide,
    chat_width_px: i32,
    /// Set by the tick thread when the user resizes the chat while docked;
    /// drained by the app wrapper into the persisted setting (the thread has
    /// no store handle, deliberately).
    width_dirty: bool,
    player_pid: u32,
    /// `HWND` isn't `Send`; the raw value is. Reconstructed at use.
    player_hwnd: Option<isize>,
    chat_hwnd: Option<isize>,
    /// The frame rect we last observed-or-set for each window. Every one of
    /// our own `SetWindowPos` calls is immediately read back into these, so
    /// our own writes can never register as "the other window moved" — that
    /// read-back IS the feedback-loop guard.
    expected_player: Option<PixelRect>,
    expected_chat: Option<PixelRect>,
    /// Last mirrored iconic state, so a minimize is mirrored once, not
    /// re-asserted every tick against a user trying to restore.
    expected_min: Option<bool>,
    phase: DockPhase,
    pending_since: Instant,
    /// When the chat HWND stopped answering `IsWindow` (recreation grace).
    chat_lost_at: Option<Instant>,
}

static DOCKS: LazyLock<Mutex<HashMap<i64, DockState>>> = LazyLock::new(Default::default);
static PLAYERS: LazyLock<Mutex<HashMap<i64, Vec<PlayerProc>>>> = LazyLock::new(Default::default);
/// Docked chats whose player exited — the app drains this and closes them
/// (the pair behaves as one application: quit the video, the chat goes too).
static CHAT_CLOSE_REQUESTS: LazyLock<Mutex<Vec<i64>>> = LazyLock::new(Default::default);
/// Docks removed while the tick thread had their state checked out (see
/// `run_ticker`'s remove/reinsert dance): the state is out of `DOCKS` for the
/// duration of a tick so no lock is held across `SetWindowPos` — which can
/// block on the *UI thread's* message pump while the UI thread is itself
/// waiting on `DOCKS` in `dock_status`. Without this ledger, an undock that
/// landed during that window would be silently reverted by the reinsert.
static DOCK_REMOVALS: LazyLock<Mutex<std::collections::HashSet<i64>>> =
    LazyLock::new(Default::default);
/// What [`dock_status`] serves. A separate map — NOT a view over [`DOCKS`] —
/// because the tick thread checks dock states out of `DOCKS` for the length
/// of a tick, and a toolbar reading `DOCKS` directly would see its dock
/// "vanish" for the duration of every `SetWindowPos` and flicker the 🔗
/// toggle mid-drag.
static STATUS: LazyLock<Mutex<HashMap<i64, DockStatus>>> = LazyLock::new(Default::default);
static GENERATION: AtomicU64 = AtomicU64::new(1);
static TICKER_RUNNING: AtomicBool = AtomicBool::new(false);

// ---------------------------------------------------------------------------
// Registry API (called from ui::player's spawn/reap paths)
// ---------------------------------------------------------------------------

/// Record a freshly spawned player process for `monitor_id`. `live` marks a
/// live-edge play (auto-open eligible) vs a local-file/VOD play.
pub fn note_player_spawned(monitor_id: i64, root_pid: u32, live: bool) {
    if monitor_id <= 0 {
        return; // synthetic rows (follow-raid/collab partners) have no instance
    }
    let generation = GENERATION.fetch_add(1, Ordering::Relaxed);
    PLAYERS
        .lock()
        .unwrap()
        .entry(monitor_id)
        .or_default()
        .push(PlayerProc { root_pid, generation, live });
}

/// Remove an exited player. A dock bound to this pid is ended by the tick
/// thread (which also asks the app to close the docked chat).
pub fn note_player_exited(monitor_id: i64, root_pid: u32) {
    if monitor_id <= 0 {
        return;
    }
    let mut players = PLAYERS.lock().unwrap();
    if let Some(v) = players.get_mut(&monitor_id) {
        v.retain(|p| p.root_pid != root_pid);
        if v.is_empty() {
            players.remove(&monitor_id);
        }
    }
}

/// Whether any player is currently registered for this monitor — gates the
/// chat toolbar's dock toggle.
pub fn player_available(monitor_id: i64) -> bool {
    PLAYERS.lock().unwrap().get(&monitor_id).is_some_and(|v| !v.is_empty())
}

/// The newest **live** player generation for this monitor (0 = none). The
/// app's dock-on-play reconciliation compares this against the last value it
/// acted on, so each new play triggers exactly once.
pub fn live_player_generation(monitor_id: i64) -> u64 {
    PLAYERS
        .lock()
        .unwrap()
        .get(&monitor_id)
        .into_iter()
        .flatten()
        .filter(|p| p.live)
        .map(|p| p.generation)
        .max()
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// Dock API (called from the chat popup + the app wrapper)
// ---------------------------------------------------------------------------

/// Start (or re-target) a dock for this monitor's chat window. Binds to the
/// newest registered player; resolution of the actual windows happens on the
/// tick thread. No-op if no player is registered.
pub fn request_dock(monitor_id: i64, chat_title: String, side: DockSide, chat_width_px: i32) {
    let Some(pid) = PLAYERS
        .lock()
        .unwrap()
        .get(&monitor_id)
        .into_iter()
        .flatten()
        .max_by_key(|p| p.generation)
        .map(|p| p.root_pid)
    else {
        return;
    };
    DOCK_REMOVALS.lock().unwrap().remove(&monitor_id);
    STATUS.lock().unwrap().insert(monitor_id, DockStatus::Pending);
    DOCKS.lock().unwrap().insert(
        monitor_id,
        DockState {
            chat_title,
            side,
            chat_width_px: chat_width_px.max(120),
            width_dirty: false,
            player_pid: pid,
            player_hwnd: None,
            chat_hwnd: None,
            expected_player: None,
            expected_chat: None,
            expected_min: None,
            phase: DockPhase::Pending,
            pending_since: Instant::now(),
            chat_lost_at: None,
        },
    );
    ensure_ticker();
}

/// End a dock. The chat window stays exactly where it is — this is the
/// user's detach, not the player-exit path.
pub fn request_undock(monitor_id: i64) {
    DOCKS.lock().unwrap().remove(&monitor_id);
    DOCK_REMOVALS.lock().unwrap().insert(monitor_id);
    STATUS.lock().unwrap().remove(&monitor_id);
}

/// The chat popup for this monitor closed; drop any dock silently (the
/// player is never touched by a chat-side close).
pub fn note_chat_closed(monitor_id: i64) {
    DOCKS.lock().unwrap().remove(&monitor_id);
    DOCK_REMOVALS.lock().unwrap().insert(monitor_id);
    STATUS.lock().unwrap().remove(&monitor_id);
}

/// What the chat toolbar's toggle should show for this monitor.
pub fn dock_status(monitor_id: i64) -> DockStatus {
    STATUS.lock().unwrap().get(&monitor_id).copied().unwrap_or(DockStatus::Undocked)
}

/// Drain the "user resized the docked chat" width, if it changed since the
/// last call — the app wrapper persists it.
pub fn take_dirty_width(monitor_id: i64) -> Option<i32> {
    let mut docks = DOCKS.lock().unwrap();
    let d = docks.get_mut(&monitor_id)?;
    d.width_dirty.then(|| {
        d.width_dirty = false;
        d.chat_width_px
    })
}

/// Drain the monitors whose docked player exited — the app closes those chat
/// popups (quit the video, the whole thing is gone).
pub fn take_chat_close_requests() -> Vec<i64> {
    std::mem::take(&mut *CHAT_CLOSE_REQUESTS.lock().unwrap())
}

// ---------------------------------------------------------------------------
// Pure geometry + decision logic (unit-tested; no Win32 anywhere below until
// the #[cfg(windows)] section)
// ---------------------------------------------------------------------------

/// Where the chat's frame belongs for a given player frame: flush against
/// the chosen edge, same top, same height, its own width.
pub(crate) fn chat_target(player: PixelRect, chat_width: i32, side: DockSide) -> PixelRect {
    let x = match side {
        DockSide::Right => player.x + player.w,
        DockSide::Left => player.x - chat_width,
    };
    PixelRect { x, y: player.y, w: chat_width, h: player.h }
}

/// Where the player's frame belongs after the user dragged the chat: the
/// inverse of [`chat_target`], keeping the player's own size.
pub(crate) fn player_target_from_chat(
    chat: PixelRect,
    player_size: (i32, i32),
    side: DockSide,
) -> PixelRect {
    let (w, h) = player_size;
    let x = match side {
        DockSide::Right => chat.x - w,
        DockSide::Left => chat.x + chat.w,
    };
    PixelRect { x, y: chat.y, w, h }
}

/// Whether two frame rects are the same window position for our purposes.
pub(crate) fn rects_close(a: PixelRect, b: PixelRect, tol: i32) -> bool {
    (a.x - b.x).abs() <= tol
        && (a.y - b.y).abs() <= tol
        && (a.w - b.w).abs() <= tol
        && (a.h - b.h).abs() <= tol
}

/// What one tick should do, decided purely from rect bookkeeping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TickAction {
    Idle,
    /// First tick (or re-activation): put the chat at its target.
    SnapChat,
    /// The player moved/resized; the chat follows.
    FollowPlayer,
    /// The chat was dragged (translated, same size); the player follows.
    FollowChat,
    /// The chat was resized; adopt the new width, then re-pin the chat.
    /// The player is never moved by a pure chat resize.
    AcceptChatWidth(i32),
}

/// The mirroring decision for one tick. `expected_*` is the frame we last
/// observed-or-set; `cur_*` is what's on screen now.
///
/// Ordering matters: when **both** windows moved inside one tick (a snap
/// gesture nudging both, or the first tick after enable), the player wins —
/// video is the primary surface, and the chat re-pins to it.
pub(crate) fn decide(
    expected_player: Option<PixelRect>,
    cur_player: PixelRect,
    expected_chat: Option<PixelRect>,
    cur_chat: PixelRect,
) -> TickAction {
    let (Some(exp_p), Some(exp_c)) = (expected_player, expected_chat) else {
        return TickAction::SnapChat;
    };
    let player_moved = !rects_close(cur_player, exp_p, TOLERANCE);
    let chat_moved = !rects_close(cur_chat, exp_c, TOLERANCE);
    match (player_moved, chat_moved) {
        (true, _) => TickAction::FollowPlayer,
        (false, true) => {
            let resized = (cur_chat.w - exp_c.w).abs() > TOLERANCE
                || (cur_chat.h - exp_c.h).abs() > TOLERANCE;
            if resized {
                TickAction::AcceptChatWidth(cur_chat.w)
            } else {
                TickAction::FollowChat
            }
        }
        (false, false) => TickAction::Idle,
    }
}

/// Convert a desired DWM *frame* rect into the *window* rect `SetWindowPos`
/// expects, using the window's current frame-vs-window offsets (Win10/11's
/// invisible resize borders make these differ by ~7 px per side).
pub(crate) fn frame_to_window(
    desired_frame: PixelRect,
    window: PixelRect,
    frame: PixelRect,
) -> PixelRect {
    let left = frame.x - window.x; // how far the frame is inset from the window
    let top = frame.y - window.y;
    let w_pad = window.w - frame.w;
    let h_pad = window.h - frame.h;
    PixelRect {
        x: desired_frame.x - left,
        y: desired_frame.y - top,
        w: desired_frame.w + w_pad,
        h: desired_frame.h + h_pad,
    }
}

/// A minimize/restore that one tick should mirror onto the other window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MinAction {
    MinimizeChat,
    MinimizePlayer,
    RestoreChat,
    RestorePlayer,
}

/// Mirror iconic state both ways, at most one action per change. `expected`
/// is the last state this dock put (or observed) both windows in — without
/// it, a user restoring one window would fight a per-tick re-minimize.
pub(crate) fn mirror_min(
    expected: Option<bool>,
    player_iconic: bool,
    chat_iconic: bool,
) -> Option<MinAction> {
    let exp = expected.unwrap_or(false);
    match (player_iconic, chat_iconic) {
        (true, false) if !exp => Some(MinAction::MinimizeChat),
        (false, true) if !exp => Some(MinAction::MinimizePlayer),
        (false, true) if exp => Some(MinAction::RestoreChat),
        (true, false) if exp => Some(MinAction::RestorePlayer),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// The tick thread + Win32 (windows only)
// ---------------------------------------------------------------------------

fn ensure_ticker() {
    #[cfg(windows)]
    if !TICKER_RUNNING.swap(true, Ordering::SeqCst) {
        std::thread::Builder::new()
            .name("window-dock".into())
            .spawn(win::run_ticker)
            .expect("spawn window-dock thread");
    }
}

#[cfg(windows)]
mod win {
    use super::*;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
        IsWindowVisible, SetWindowPos, ShowWindow, SWP_NOACTIVATE, SWP_NOZORDER, SW_MINIMIZE,
        SW_RESTORE,
    };
    use windows::core::BOOL;

    fn hwnd(raw: isize) -> HWND {
        HWND(raw as *mut core::ffi::c_void)
    }

    fn to_rect(r: RECT) -> PixelRect {
        PixelRect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
    }

    /// The window's *visual* rect. DWM extended frame bounds where possible —
    /// `GetWindowRect` alone includes the invisible resize borders and would
    /// leave a ~7 px moat between "flush" windows.
    fn frame_rect(h: HWND) -> Option<PixelRect> {
        let mut r = RECT::default();
        let ok = unsafe {
            DwmGetWindowAttribute(
                h,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut r as *mut RECT as *mut _,
                std::mem::size_of::<RECT>() as u32,
            )
        };
        if ok.is_ok() {
            return Some(to_rect(r));
        }
        window_rect(h)
    }

    fn window_rect(h: HWND) -> Option<PixelRect> {
        let mut r = RECT::default();
        unsafe { GetWindowRect(h, &mut r) }.ok().map(|_| to_rect(r))
    }

    /// Move/size a window so its *frame* lands on `desired` (offset-corrected
    /// through [`frame_to_window`]), then read the frame back — the read-back
    /// is what feeds `expected_*`, and Windows may have clamped us.
    fn set_frame_rect(h: HWND, desired: PixelRect) -> Option<PixelRect> {
        let win = window_rect(h)?;
        let frame = frame_rect(h)?;
        let target = frame_to_window(desired, win, frame);
        unsafe {
            let _ = SetWindowPos(
                h,
                None,
                target.x,
                target.y,
                target.w,
                target.h,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
        frame_rect(h)
    }

    /// Find this process's chat popup window by exact title, excluding HWNDs
    /// already claimed by other docks (two monitors can carry the same
    /// channel name — main + alt account — and then title alone is
    /// ambiguous; first-come claim disambiguates).
    pub(super) fn find_chat_hwnd(
        title: &str,
        claimed: &std::collections::HashSet<isize>,
    ) -> Option<isize> {
        struct Ctx<'a> {
            own_pid: u32,
            title: &'a str,
            claimed: &'a std::collections::HashSet<isize>,
            found: Option<isize>,
        }
        let mut ctx =
            Ctx { own_pid: std::process::id(), title, claimed, found: None };

        unsafe extern "system" fn callback(h: HWND, lparam: LPARAM) -> BOOL {
            let ctx = unsafe { &mut *(lparam.0 as *mut Ctx) };
            unsafe {
                if !IsWindowVisible(h).as_bool() {
                    return true.into();
                }
                let mut pid = 0u32;
                GetWindowThreadProcessId(h, Some(&mut pid));
                if pid != ctx.own_pid {
                    return true.into();
                }
                let mut buf = [0u16; 256];
                let n = GetWindowTextW(h, &mut buf);
                let text = String::from_utf16_lossy(&buf[..n as usize]);
                if text == ctx.title && !ctx.claimed.contains(&(h.0 as isize)) {
                    ctx.found = Some(h.0 as isize);
                    return false.into();
                }
            }
            true.into()
        }
        unsafe {
            let _ = EnumWindows(Some(callback), LPARAM(&mut ctx as *mut Ctx as isize));
        }
        ctx.found
    }

    /// Whether a window's frame covers its whole monitor — mpv's `f`.
    fn is_fullscreen(h: HWND, frame: PixelRect) -> bool {
        unsafe {
            let mon = MonitorFromWindow(h, MONITOR_DEFAULTTONEAREST);
            let mut mi = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(mon, &mut mi).as_bool() {
                return false;
            }
            let m = to_rect(mi.rcMonitor);
            frame.x <= m.x && frame.y <= m.y && frame.w >= m.w && frame.h >= m.h
        }
    }

    /// One dock's tick. Returns `false` when the dock should be removed;
    /// pushes to [`CHAT_CLOSE_REQUESTS`] when the removal is a player exit.
    ///
    /// `claimed` is every OTHER dock's chat HWND — the caller computes it from
    /// its checked-out state map. Never lock [`DOCKS`] in here: the caller has
    /// deliberately released it so no lock is held across `SetWindowPos`,
    /// which can block on the UI thread's message pump.
    fn tick_dock(
        monitor_id: i64,
        d: &mut DockState,
        claimed: &std::collections::HashSet<isize>,
    ) -> bool {
        // --- resolve handles -------------------------------------------------
        if d.player_hwnd.is_none() {
            d.player_hwnd = crate::window_placement::find_top_level_window_for_pid_tree(
                d.player_pid,
            )
            .map(|h| h.0 as isize);
        }
        if d.chat_hwnd.is_none() {
            d.chat_hwnd = find_chat_hwnd(&d.chat_title, claimed);
        }
        let (Some(ph), Some(ch)) = (d.player_hwnd, d.chat_hwnd) else {
            if d.phase == DockPhase::Pending {
                // Still waiting for the pair to exist.
                if d.pending_since.elapsed() > PENDING_TIMEOUT {
                    tracing::warn!(monitor_id, "chat dock: gave up waiting for windows");
                    return false;
                }
                return true;
            }
            // Active but a handle went away — handled below via IsWindow, so
            // reaching here means resolution failed after a reset.
            return true;
        };
        let (ph, ch) = (hwnd(ph), hwnd(ch));

        // --- liveness ---------------------------------------------------------
        if !unsafe { IsWindow(Some(ph)) }.as_bool() {
            // One re-walk of the pid tree covers a recreated player window;
            // players don't normally recreate, so a miss means it exited.
            d.player_hwnd = crate::window_placement::find_top_level_window_for_pid_tree(
                d.player_pid,
            )
            .map(|h| h.0 as isize);
            if d.player_hwnd.is_none() {
                tracing::info!(monitor_id, "chat dock: player window gone — closing the pair");
                CHAT_CLOSE_REQUESTS.lock().unwrap().push(monitor_id);
                return false;
            }
            return true; // fresh handle next tick
        }
        if !unsafe { IsWindow(Some(ch)) }.as_bool() {
            // Deferred viewports can be recreated with a fresh HWND (same
            // title). Re-find with a grace window before giving up.
            d.chat_hwnd = None;
            let lost = *d.chat_lost_at.get_or_insert_with(Instant::now);
            if lost.elapsed() > CHAT_REFIND_GRACE {
                tracing::info!(monitor_id, "chat dock: chat window gone");
                return false;
            }
            return true;
        }
        d.chat_lost_at = None;

        if d.phase == DockPhase::Pending {
            d.phase = DockPhase::Active;
            d.expected_player = None; // force SnapChat below
            d.expected_chat = None;
        }

        // --- minimize mirroring ----------------------------------------------
        let p_ic = unsafe { IsIconic(ph) }.as_bool();
        let c_ic = unsafe { IsIconic(ch) }.as_bool();
        if let Some(act) = mirror_min(d.expected_min, p_ic, c_ic) {
            unsafe {
                match act {
                    MinAction::MinimizeChat => _ = ShowWindow(ch, SW_MINIMIZE),
                    MinAction::MinimizePlayer => _ = ShowWindow(ph, SW_MINIMIZE),
                    MinAction::RestoreChat => _ = ShowWindow(ch, SW_RESTORE),
                    MinAction::RestorePlayer => _ = ShowWindow(ph, SW_RESTORE),
                }
            }
            d.expected_min = Some(matches!(
                act,
                MinAction::MinimizeChat | MinAction::MinimizePlayer
            ));
            // A restore can land a window anywhere; re-snap next tick.
            if matches!(act, MinAction::RestoreChat | MinAction::RestorePlayer) {
                d.expected_player = None;
                d.expected_chat = None;
            }
            return true;
        }
        if p_ic || c_ic {
            return true; // both iconic (or mid-transition): nothing to mirror
        }
        d.expected_min = Some(false);

        // --- geometry ----------------------------------------------------------
        let (Some(cur_p), Some(cur_c)) = (frame_rect(ph), frame_rect(ch)) else {
            return true;
        };
        if is_fullscreen(ph, cur_p) {
            if d.phase != DockPhase::SuspendedFullscreen {
                d.phase = DockPhase::SuspendedFullscreen;
            }
            return true;
        }
        if d.phase == DockPhase::SuspendedFullscreen {
            d.phase = DockPhase::Active;
            d.expected_player = None; // re-snap now that fullscreen ended
            d.expected_chat = None;
        }

        match decide(d.expected_player, cur_p, d.expected_chat, cur_c) {
            TickAction::Idle => {}
            TickAction::SnapChat | TickAction::FollowPlayer => {
                let target = chat_target(cur_p, d.chat_width_px, d.side);
                d.expected_chat = set_frame_rect(ch, target);
                d.expected_player = Some(cur_p);
            }
            TickAction::FollowChat => {
                let target = player_target_from_chat(cur_c, (cur_p.w, cur_p.h), d.side);
                d.expected_player = set_frame_rect(ph, target);
                d.expected_chat = Some(cur_c);
            }
            TickAction::AcceptChatWidth(w) => {
                d.chat_width_px = w.max(120);
                d.width_dirty = true;
                let target = chat_target(cur_p, d.chat_width_px, d.side);
                d.expected_chat = set_frame_rect(ch, target);
                d.expected_player = Some(cur_p);
            }
        }
        true
    }

    pub(super) fn run_ticker() {
        loop {
            std::thread::sleep(TICK);
            // Check the states OUT of the map for the duration of the tick.
            // `SetWindowPos`/`ShowWindow` on the chat window can block until
            // the UI thread pumps the message — and the UI thread reads
            // `dock_status()` under this same mutex every toolbar frame, so
            // holding it across a window call is a deadlock waiting for a
            // slow frame. The cost is the removal race, which the
            // `DOCK_REMOVALS` ledger closes at reinsert time.
            let mut docks = DOCKS.lock().unwrap();
            if docks.is_empty() {
                TICKER_RUNNING.store(false, Ordering::SeqCst);
                return;
            }
            let states: HashMap<i64, DockState> = std::mem::take(&mut *docks);
            drop(docks);

            // Every dock's chat HWND, for the duplicate-title exclusion.
            let all_chat: HashMap<i64, isize> = states
                .iter()
                .filter_map(|(id, s)| s.chat_hwnd.map(|h| (*id, h)))
                .collect();

            let mut keep: Vec<(i64, DockState)> = Vec::new();
            for (id, mut s) in states {
                let claimed: std::collections::HashSet<isize> = all_chat
                    .iter()
                    .filter(|(mid, _)| **mid != id)
                    .map(|(_, h)| *h)
                    .collect();
                if tick_dock(id, &mut s, &claimed) {
                    keep.push((id, s));
                }
            }

            let mut docks = DOCKS.lock().unwrap();
            let mut removed = DOCK_REMOVALS.lock().unwrap();
            for (id, s) in keep {
                if removed.remove(&id) {
                    continue; // undocked while the state was checked out
                }
                // A fresh request_dock that raced in wins over our copy.
                docks.entry(id).or_insert(s);
            }
            // Removals for docks we dropped ourselves are stale bookkeeping.
            removed.retain(|id| docks.contains_key(id));
            // Rebuild the status view under the same lock, so it can never
            // disagree with the real map for longer than one tick.
            let mut st = STATUS.lock().unwrap();
            st.clear();
            for (id, s) in docks.iter() {
                let v = match s.phase {
                    DockPhase::Pending => DockStatus::Pending,
                    _ => DockStatus::Docked,
                };
                st.insert(*id, v);
            }
        }
    }
}

#[cfg(not(windows))]
mod win {
    pub(super) fn run_ticker() {}
}

#[cfg(test)]
mod tests {
    use super::*;

    const P: PixelRect = PixelRect { x: 100, y: 50, w: 1280, h: 720 };

    #[test]
    fn chat_sits_flush_on_the_chosen_side_with_the_players_height() {
        let right = chat_target(P, 480, DockSide::Right);
        assert_eq!(right, PixelRect { x: 1380, y: 50, w: 480, h: 720 });
        let left = chat_target(P, 480, DockSide::Left);
        assert_eq!(left, PixelRect { x: -380, y: 50, w: 480, h: 720 });

        // Negative coordinates (monitor left of primary) are ordinary values.
        let p2 = PixelRect { x: -1920, y: -200, w: 1920, h: 1080 };
        assert_eq!(chat_target(p2, 400, DockSide::Right).x, 0);
    }

    #[test]
    fn dragging_the_chat_reconstructs_the_players_place_exactly() {
        for side in [DockSide::Right, DockSide::Left] {
            let chat = chat_target(P, 480, side);
            assert_eq!(
                player_target_from_chat(chat, (P.w, P.h), side),
                P,
                "round-trip must be exact for {side:?}"
            );
        }
    }

    #[test]
    fn frame_to_window_re_inflates_the_invisible_borders() {
        // Window rect 7px wider on left/right/bottom than the visual frame —
        // the Win10/11 shape.
        let window = PixelRect { x: 93, y: 50, w: 1294, h: 727 };
        let frame = PixelRect { x: 100, y: 50, w: 1280, h: 720 };
        let desired = PixelRect { x: 500, y: 300, w: 1280, h: 720 };
        let out = frame_to_window(desired, window, frame);
        assert_eq!(out, PixelRect { x: 493, y: 300, w: 1294, h: 727 });
    }

    #[test]
    fn decide_covers_the_truth_table() {
        let p = P;
        let c = chat_target(p, 480, DockSide::Right);

        // First tick: no expectations yet -> snap.
        assert_eq!(decide(None, p, None, c), TickAction::SnapChat);

        // Nothing moved (within tolerance) -> idle.
        let jitter = PixelRect { x: p.x + 1, ..p };
        assert_eq!(decide(Some(p), jitter, Some(c), c), TickAction::Idle);

        // Player moved -> chat follows.
        let p2 = PixelRect { x: p.x + 300, ..p };
        assert_eq!(decide(Some(p), p2, Some(c), c), TickAction::FollowPlayer);

        // Chat translated, same size -> player follows.
        let c2 = PixelRect { x: c.x - 250, y: c.y + 40, ..c };
        assert_eq!(decide(Some(p), p, Some(c), c2), TickAction::FollowChat);

        // Chat resized -> adopt width, never move the player.
        let c3 = PixelRect { w: c.w + 120, ..c };
        assert_eq!(decide(Some(p), p, Some(c), c3), TickAction::AcceptChatWidth(c.w + 120));

        // Both moved in one tick -> the player wins.
        assert_eq!(decide(Some(p), p2, Some(c), c2), TickAction::FollowPlayer);
    }

    #[test]
    fn minimize_mirrors_both_ways_and_never_refires() {
        use MinAction::*;
        // Player minimized while the pair was up -> chat follows down.
        assert_eq!(mirror_min(Some(false), true, false), Some(MinimizeChat));
        // Chat minimized -> player follows down.
        assert_eq!(mirror_min(Some(false), false, true), Some(MinimizePlayer));
        // Pair is down, player restored -> chat follows up.
        assert_eq!(mirror_min(Some(true), false, true), Some(RestoreChat));
        // Pair is down, chat restored -> player follows up.
        assert_eq!(mirror_min(Some(true), true, false), Some(RestorePlayer));
        // Settled states never act (this is the no-refire guard: without it a
        // user restoring one window would be re-minimized every tick).
        assert_eq!(mirror_min(Some(true), true, true), None);
        assert_eq!(mirror_min(Some(false), false, false), None);
        // Unknown starting state assumes "up".
        assert_eq!(mirror_min(None, true, false), Some(MinimizeChat));
    }

    #[test]
    fn registry_generations_are_monotonic_and_exit_clears_availability() {
        // Use a monitor id no other test touches: globals are shared.
        let mid = 987_654;
        assert!(!player_available(mid));
        assert_eq!(live_player_generation(mid), 0);

        note_player_spawned(mid, 111, true);
        let g1 = live_player_generation(mid);
        assert!(g1 > 0);
        note_player_spawned(mid, 222, true);
        let g2 = live_player_generation(mid);
        assert!(g2 > g1, "a new spawn must advance the generation");

        // A local-file play registers availability but not a LIVE generation.
        note_player_exited(mid, 111);
        note_player_exited(mid, 222);
        note_player_spawned(mid, 333, false);
        assert!(player_available(mid));
        assert_eq!(live_player_generation(mid), 0, "local plays never auto-open chat");

        note_player_exited(mid, 333);
        assert!(!player_available(mid));

        // Synthetic rows (id <= 0) are ignored entirely.
        note_player_spawned(0, 999, true);
        assert!(!player_available(0));
    }

    /// The undock-while-checked-out race: `run_ticker` takes states out of
    /// DOCKS during a tick (no lock across `SetWindowPos`), so an undock that
    /// lands in that window removes nothing — the ledger is what makes it
    /// stick at reinsert time.
    #[test]
    fn an_undock_during_a_tick_is_not_resurrected() {
        let mid = 987_655;
        note_player_spawned(mid, 555, true);
        request_dock(mid, "💬  Chat — test".into(), DockSide::Right, 480);
        assert_eq!(dock_status(mid), DockStatus::Pending);

        // Simulate the ticker checking the state out...
        let state = DOCKS.lock().unwrap().remove(&mid).expect("dock exists");
        // ...the user undocks while it's out (map removal is a no-op)...
        request_undock(mid);
        // ...and the ticker's reinsert must honour the ledger.
        let mut docks = DOCKS.lock().unwrap();
        let mut removed = DOCK_REMOVALS.lock().unwrap();
        if !removed.remove(&mid) {
            docks.entry(mid).or_insert(state);
        }
        assert!(!docks.contains_key(&mid), "the undock must win");
        assert_eq!(dock_status(mid), DockStatus::Undocked);
        drop(docks);
        drop(removed);
        note_player_exited(mid, 555);
    }

    #[test]
    fn close_requests_drain_once() {
        CHAT_CLOSE_REQUESTS.lock().unwrap().push(42);
        assert_eq!(take_chat_close_requests(), vec![42]);
        assert!(take_chat_close_requests().is_empty(), "drain must consume");
    }
}
