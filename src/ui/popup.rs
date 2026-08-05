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
//! `show_viewport_deferred` repaints independently of the root and doesn't
//! need to be re-declared every frame to survive, but its callback must be
//! `Fn(...) + Send + Sync + 'static` — it cannot hold `&mut self`. Popup
//! state therefore has to live in `Arc<Mutex<T>>`, and anything a popup used
//! to do directly against `&mut self` beyond its own state (open another
//! popup, set the status line, ...) goes through [`PopupAction`] instead,
//! queued here and applied on the main thread the next frame — the same
//! shape as the existing `ui_rx: Receiver<UiCommand>` tray/doorbell bus
//! (`StreamArchiverApp::pump_messages`).

use super::*;

/// Cheap-to-clone bundle of shared app state a deferred popup closure
/// commonly needs besides its own `Arc<Mutex<T>>` state. Build fresh via
/// [`StreamArchiverApp::popup_shared`] wherever a popup is declared/updated.
#[derive(Clone)]
pub(super) struct PopupShared {
    pub(super) core: Arc<AppCore>,
    pub(super) fs_probes: Arc<Mutex<FsProbes>>,
    pub(super) actions: std::sync::mpsc::Sender<PopupAction>,
}

/// A side effect a deferred popup closure can't perform directly (no
/// `&mut self`) — queued via `PopupShared::actions` and applied on the main
/// thread by [`StreamArchiverApp::pump_popup_actions`]. Grown incrementally:
/// add a variant only when a popup being migrated actually needs it.
pub(super) enum PopupAction {
    SetStatus(String),
}

impl StreamArchiverApp {
    /// Build a [`PopupShared`] for this frame. Cheap — every field is an
    /// `Arc` clone or a channel `Sender` clone.
    pub(super) fn popup_shared(&self) -> PopupShared {
        PopupShared {
            core: self.core.clone(),
            fs_probes: self.fs_probes.clone(),
            actions: self.popup_actions_tx.clone(),
        }
    }

    /// Drain [`PopupAction`]s queued by deferred popup closures since last
    /// frame. Called once per frame from `logic()`, alongside the other
    /// per-frame drains (`fs_probes.drain_results()`, `pump_messages`).
    pub(super) fn pump_popup_actions(&mut self) {
        while let Ok(action) = self.popup_actions_rx.try_recv() {
            match action {
                PopupAction::SetStatus(s) => self.status = s,
            }
        }
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
            body(ui, &s.payload, &mut s.result);
        });
    });
    None
}
