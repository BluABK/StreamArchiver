//! Shared infrastructure for popup windows migrated from
//! `show_viewport_immediate` to `show_viewport_deferred`.
//!
//! `show_viewport_immediate` closures only run as a side effect of the root
//! viewport's own frame (`StreamArchiverApp::ui()`). When the root window is
//! minimized, Windows stops delivering it repaint events, eframe stops
//! calling `ui()` at all, and since the popup's `show_viewport_immediate`
//! call didn't run that frame, egui's viewport GC treats it as gone and
//! destroys its native window — it gets recreated from scratch (new HWND,
//! default position) whenever the root is restored. Confirmed empirically
//! with a minimal eframe repro before this module was written, not guessed.
//!
//! **Deferred is only half the fix**: a deferred viewport still has to be
//! re-declared during every pass of its parent or egui's end-of-pass GC
//! destroys it just the same. eframe skips `App::ui` (but not `App::logic`)
//! whenever the root viewport reports itself invisible, and
//! `ViewportInfo::visible()` derives that from `minimized`/`occluded` — so a
//! merely minimized main window meant no `ui()`, no re-declaration, and every
//! popup's native window destroyed. That is why
//! `StreamArchiverApp::popup_windows` is called from `logic()` — see its docs, and
//! `examples/vp_minimize_probe.rs` for the probe that pins both halves down.
//!
//! `show_viewport_deferred` repaints independently of the root — the popup's
//! own UI no longer runs as a side effect of the root's frame — but its
//! callback must be
//! `Fn(...) + Send + Sync + 'static` — it cannot hold `&mut self`. Popup
//! state therefore has to live in `Arc<Mutex<T>>`. In practice every one of
//! the ~68 popups converted to this module found a way to avoid needing
//! direct `&mut self` access from inside the closure at all — either the
//! existing "collect during render, apply after `show_deferred_popup`
//! returns" shape (the wrapper still has real `&mut self`), or moving the
//! one `&mut self`-requiring action (e.g. `channel_props_apply_pref_change`,
//! which calls the app-wide `reload_rows()`) to the wrapper instead of the
//! closure. An escape-hatch action queue was built during Phase 0 for the
//! case neither shape would fit, but nothing ever needed it — removed
//! rather than kept as unused scaffolding. If a future popup genuinely can't
//! avoid a `&mut self` side effect from inside its closure, reintroduce the
//! same shape as `ui_rx: Receiver<UiCommand>` (`StreamArchiverApp::
//! pump_messages`): an `mpsc` channel drained once per frame in `logic()`.

use super::*;

/// Cheap-to-clone bundle of shared app state a deferred popup closure
/// commonly needs besides its own `Arc<Mutex<T>>` state. Build fresh via
/// [`StreamArchiverApp::popup_shared`] wherever a popup is declared/updated.
#[derive(Clone)]
pub(super) struct PopupShared {
    pub(super) core: Arc<AppCore>,
    pub(super) fs_probes: Arc<Mutex<FsProbes>>,
}

impl StreamArchiverApp {
    /// Build a [`PopupShared`] for this frame. Cheap — every field is an
    /// `Arc` clone.
    pub(super) fn popup_shared(&self) -> PopupShared {
        PopupShared { core: self.core.clone(), fs_probes: self.fs_probes.clone() }
    }
}

/// Show (or update) a deferred-viewport popup backed by `state`.
///
/// Unlike `show_viewport_immediate`, `render` runs on its own schedule,
/// independent of the root viewport's frame cadence — the whole point of
/// this module — so the window survives the root being minimized. Still
/// needs to be called every frame the popup should exist (matching
/// `show_viewport_immediate`'s own contract): cheap, since it just
/// re-registers the same `Arc` callback each time; the actual UI closure
/// execution is what's decoupled from the caller's frame.
///
/// `render` receives the raw `&egui::Context` (not a pre-built `Ui`) so it
/// can use whichever container the window already used — `CentralPanel`,
/// `egui::Window`, etc. — unchanged from before the migration.
pub(super) fn show_deferred_popup<T: Send + 'static>(
    ctx: &egui::Context,
    id: egui::ViewportId,
    builder: egui::ViewportBuilder,
    state: Arc<Mutex<T>>,
    shared: PopupShared,
    render: impl Fn(&egui::Context, &mut T, &PopupShared) + Send + Sync + 'static,
) {
    ctx.show_viewport_deferred(id, builder, move |ctx, _class| {
        if let Ok(mut guard) = state.lock() {
            render(ctx, &mut guard, &shared);
        }
    });
}

/// How often a [`throttled_spinner`] asks its viewport to repaint. ~16 fps —
/// smooth enough for a loading indicator, slow enough to leave the event loop
/// alone.
const SPINNER_TICK: std::time::Duration = std::time::Duration::from_millis(60);

/// A loading spinner that does **not** free-run its viewport. Use this instead
/// of [`egui::Ui::spinner`] anywhere inside a deferred popup.
///
/// `egui::Spinner::paint_at` calls `ui.request_repaint()` — a *zero-delay*
/// request — every pass it is visible. In a deferred viewport that saturates
/// eframe's event loop: the popup free-runs at ~165 passes/s and the **root**
/// viewport drops to **zero** passes per second (measured; reproduce with
/// `cargo run --example vp_repaint_probe`, `PROBE_CHILD_MS=0`). Since
/// `App::logic` only runs on a root pass, everything the root drives stops
/// too — message pumping, the drains, the UI-freeze watchdog's heartbeat, and
/// every wrapper that polls a background job.
///
/// That last one bites hardest here: a "Loading…" spinner in a popup stalls
/// the very load it is waiting for. The Properties window's off-thread load
/// finished in ~100 ms yet sat on its spinner for as long as the main window
/// stayed idle, because nothing was left to run its `try_recv`. At this
/// module's 60 ms tick the same test has the root at ~30 passes/s.
///
/// Same paint as `egui::Spinner` (arc of `n_points` along a time-driven
/// sweep), minus the repaint request.
pub(super) fn throttled_spinner(ui: &mut egui::Ui) -> egui::Response {
    let size = ui.style().spacing.interact_size.y;
    let (rect, response) = ui.allocate_exact_size(egui::vec2(size, size), egui::Sense::hover());
    if ui.is_rect_visible(rect) {
        ui.ctx().request_repaint_after(SPINNER_TICK);
        let color = ui.visuals().strong_text_color();
        let radius = (rect.height().min(rect.width()) / 2.0) - 2.0;
        let n_points = (radius.round() as u32).clamp(8, 128);
        let time = ui.input(|i| i.time);
        let start_angle = time * std::f64::consts::TAU;
        let end_angle = start_angle + 240f64.to_radians() * time.sin();
        let points: Vec<egui::Pos2> = (0..n_points)
            .map(|i| {
                let angle = egui::lerp(start_angle..=end_angle, i as f64 / n_points as f64);
                let (sin, cos) = angle.sin_cos();
                rect.center() + radius * egui::vec2(cos as f32, sin as f32)
            })
            .collect();
        ui.painter().add(egui::Shape::line(points, egui::Stroke::new(3.0, color)));
    }
    response
}

/// Per-key `Arc<Mutex<T>>` state for popups shaped like "`Vec<K>` of open
/// ids + derive each one's content once from a cache/store lookup" — channel
/// history, chapters, VOD/remux info, ad breaks, upcoming-schedule popups,
/// etc. `T` is created lazily via `get_or_init` the first time a key is seen,
/// and dropped once the caller's own open-id list stops naming it (call
/// `retain` with that same list each frame, mirroring the existing
/// `self.xxx_popups.retain(...)` cleanup every one of these windows already
/// does — see e.g. `history_popup_windows`).
pub(super) struct PopupRegistry<K, T> {
    states: HashMap<K, Arc<Mutex<T>>>,
}

impl<K: Eq + std::hash::Hash + Clone, T> Default for PopupRegistry<K, T> {
    fn default() -> Self {
        PopupRegistry { states: HashMap::new() }
    }
}

impl<K: Eq + std::hash::Hash + Clone, T> PopupRegistry<K, T> {
    pub(super) fn get_or_init(&mut self, key: K, init: impl FnOnce() -> T) -> Arc<Mutex<T>> {
        self.states.entry(key).or_insert_with(|| Arc::new(Mutex::new(init()))).clone()
    }

    /// Drop state for any key no longer in `keep` — call alongside the
    /// caller's own open-id-list `.retain(...)`.
    pub(super) fn retain(&mut self, keep: &[K]) {
        self.states.retain(|k, _| keep.contains(k));
    }

    /// Drop state for one key — for a registry whose entries are removed
    /// individually rather than via a caller-side open-id list (e.g. once a
    /// background load completes and the entry's job is done).
    pub(super) fn remove(&mut self, key: &K) {
        self.states.remove(key);
    }
}

/// Backing state for the generic "are you sure?" confirm dialog
/// ([`confirm_dialog_deferred`]). `payload` is whatever the dialog needs to
/// render its message (an id, a name, a list of items to delete, ...) —
/// fixed at open time, never mutated by the dialog itself.
pub(super) struct ConfirmDialogState<T> {
    pub(super) payload: T,
    pub(super) open: bool,
    /// `Some(true)` = confirmed, `Some(false)` = cancelled. Set by the
    /// dialog's own `body` closure; read back by the caller next frame.
    pub(super) result: Option<bool>,
}

impl<T> ConfirmDialogState<T> {
    pub(super) fn open(payload: T) -> Arc<Mutex<ConfirmDialogState<T>>> {
        Arc::new(Mutex::new(ConfirmDialogState { payload, open: true, result: None }))
    }
}

/// Drive one confirm dialog through to a decision across frames. Call every
/// frame the dialog's backing `Option<Arc<Mutex<ConfirmDialogState<T>>>>` is
/// `Some` — cheap no-op re-registration when nothing has changed, same
/// contract as [`show_deferred_popup`].
///
/// Returns `Some(true)`/`Some(false)` exactly once the user confirms/cancels
/// (including closing the window, which counts as cancel) — the caller
/// should clear its `Option` field in both cases and act on `Some(true)`.
/// Returns `None` while still open and undecided (nothing to do yet).
///
/// `body` renders the dialog's message + buttons; call
/// `*result = Some(true)` / `Some(false)` from a button's `.clicked()` — see
/// any of the `confirm_*_window` functions in `dialogs.rs` for the shape.
#[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
pub(super) fn confirm_dialog_deferred<T: Send + 'static>(
    ctx: &egui::Context,
    shared: PopupShared,
    id: egui::ViewportId,
    builder: egui::ViewportBuilder,
    state: &Arc<Mutex<ConfirmDialogState<T>>>,
    body: impl Fn(&mut egui::Ui, &T, &mut Option<bool>) + Send + Sync + 'static,
) -> Option<bool> {
    {
        let s = state.lock().unwrap();
        if !s.open {
            return Some(false);
        }
        if let Some(r) = s.result {
            return Some(r);
        }
    }
    show_deferred_popup(ctx, id, builder, state.clone(), shared, move |ctx, s, _shared| {
        if ctx.input(|i| i.viewport().close_requested()) {
            s.open = false;
        }
        egui::CentralPanel::default().show(ctx, |ui| {
            // Scrollable because a confirm dialog's message embeds user data —
            // a take's full title, an absolute path — whose height nobody can
            // predict when picking `with_inner_size`. Every caller draws its
            // buttons at the end of `body`, so without this an over-long
            // message pushes them past the bottom edge of a fixed-size,
            // non-resizable window and the dialog becomes impossible to
            // answer (reported for "Delete file from disk").
            egui::ScrollArea::vertical().show(ui, |ui| {
                body(ui, &s.payload, &mut s.result);
            });
        });
    });
    None
}
