//! Inspect Mode: a DevTools-style widget inspector (F12).
//!
//! Widgets opt in by chaining [`Inspectable::inspect`] on their
//! [`egui::Response`]; each call registers an [`InspectInfo`] (name, id, rect,
//! state, custom props, and the `#[track_caller]` source location) into a
//! thread-local per-frame registry. The Inspector window drains the registry
//! once per frame — as the *final* statement of `ui()`, because child
//! viewports register after the root CentralPanel — and displays the previous
//! frame's snapshot (double-buffered, as immediate-mode requires).
//!
//! The highlight rect is painted *at registration time* via `response.ctx`:
//! every child window is an immediate viewport with its own pass, and by the
//! time the snapshot is drained those passes have ended, so post-hoc painting
//! would land on the wrong viewport. Painting inside `.inspect()` is always
//! the correct viewport and the current rect, at the cost of a uniform
//! one-frame lag on which widget is targeted (standard for immediate mode).
//!
//! Thread-local caveat: all of the app's child windows are
//! `show_viewport_immediate` (same thread). If a *deferred* viewport (own
//! thread) is ever added, its registrations would be silently lost — switch
//! the registry to a `Mutex<Vec<_>>` then.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};

use eframe::egui;

thread_local! {
    /// Widgets registered so far this frame (drained by [`InspectorState::end_frame`]).
    static REGISTRY: RefCell<Vec<InspectInfo>> = const { RefCell::new(Vec::new()) };
    /// The widget id to outline this frame (published at the end of the previous one).
    static HIGHLIGHT: Cell<Option<egui::Id>> = const { Cell::new(None) };
}

/// Whether the inspector is collecting. Checked first in `.inspect()` so the
/// call is a single relaxed load + branch when the inspector is closed.
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Stroke/label color for the highlight rect.
const HIGHLIGHT_COLOR: egui::Color32 = egui::Color32::from_rgb(0x37, 0xa8, 0xff);

/// One registered widget: everything the Elements tab shows.
///
/// Named `InspectInfo` (not `WidgetInfo`) to avoid colliding with
/// [`egui::WidgetInfo`].
#[derive(Clone, Debug)]
pub struct InspectInfo {
    pub name: String,
    pub id: egui::Id,
    pub rect: egui::Rect,
    pub interact_rect: egui::Rect,
    pub enabled: bool,
    pub hovered: bool,
    pub clicked: bool,
    pub props: Vec<(&'static str, String)>,
    /// The `.inspect()` call site — the lookup handle back into the codebase.
    pub location: &'static std::panic::Location<'static>,
    pub viewport: egui::ViewportId,
}

/// Opt a widget into the inspector by chaining right after creation:
/// `ui.button("Save").inspect("Save Button", &[])`.
pub trait Inspectable {
    /// Register with eagerly-built props. Note the `String`s are constructed
    /// at the call site even while the inspector is closed — for hot paths
    /// (per-row grid cells) use [`Inspectable::inspect_with`].
    #[track_caller]
    fn inspect(self, name: &str, props: &[(&'static str, String)]) -> Self;

    /// Register with lazily-built props: `props_fn` only runs while the
    /// inspector is open.
    #[track_caller]
    fn inspect_with(
        self,
        name: &str,
        props_fn: impl FnOnce() -> Vec<(&'static str, String)>,
    ) -> Self;
}

impl Inspectable for egui::Response {
    #[track_caller]
    fn inspect(self, name: &str, props: &[(&'static str, String)]) -> Self {
        if !ENABLED.load(Ordering::Relaxed) {
            return self;
        }
        register(&self, name, props.to_vec(), std::panic::Location::caller());
        self
    }

    #[track_caller]
    fn inspect_with(
        self,
        name: &str,
        props_fn: impl FnOnce() -> Vec<(&'static str, String)>,
    ) -> Self {
        if !ENABLED.load(Ordering::Relaxed) {
            return self;
        }
        register(&self, name, props_fn(), std::panic::Location::caller());
        self
    }
}

/// Shared tail of both trait methods: build the record, paint the highlight
/// if this widget is the current target, and push. The `InspectInfo` is fully
/// built *before* the registry is borrowed, so no user/egui code runs while
/// the `RefCell` is held.
fn register(
    resp: &egui::Response,
    name: &str,
    props: Vec<(&'static str, String)>,
    location: &'static std::panic::Location<'static>,
) {
    let info = InspectInfo {
        name: name.to_string(),
        id: resp.id,
        rect: resp.rect,
        interact_rect: resp.interact_rect,
        enabled: resp.enabled(),
        hovered: resp.hovered(),
        clicked: resp.clicked(),
        props,
        location,
        viewport: resp.ctx.viewport_id(),
    };
    // Registration-time painting: `resp.ctx` is the owning viewport's context
    // and its pass is current right now, so the rect lands on the right
    // window (post-hoc painting from the snapshot cannot — child-viewport
    // passes have already ended by drain time).
    if highlight() == Some(resp.id) {
        resp.ctx
            .debug_painter()
            .debug_rect(resp.rect, HIGHLIGHT_COLOR, name);
    }
    record(info);
}

/// Push one record for the current frame (the enabled gate lives in the trait).
pub(crate) fn record(info: InspectInfo) {
    REGISTRY.with(|r| r.borrow_mut().push(info));
}

/// Drain everything registered this frame.
pub(crate) fn take_frame() -> Vec<InspectInfo> {
    REGISTRY.with(|r| std::mem::take(&mut *r.borrow_mut()))
}

/// Turn collection on/off (mirrors the inspector window's open flag).
pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Relaxed);
}

#[cfg(test)]
pub(crate) fn is_enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Publish the widget id `.inspect()` should outline next frame.
pub(crate) fn set_highlight(id: Option<egui::Id>) {
    HIGHLIGHT.with(|h| h.set(id));
}

/// The currently published highlight target.
pub(crate) fn highlight() -> Option<egui::Id> {
    HIGHLIGHT.with(|h| h.get())
}

/// Inspector window tabs (SettingsTab pattern).
#[derive(Clone, Copy, PartialEq, Eq, Default)]
pub enum InspectorTab {
    #[default]
    Elements,
    Layout,
    Memory,
    Style,
}

impl InspectorTab {
    pub const ALL: [InspectorTab; 4] = [
        InspectorTab::Elements,
        InspectorTab::Layout,
        InspectorTab::Memory,
        InspectorTab::Style,
    ];

    pub fn label(self) -> &'static str {
        match self {
            InspectorTab::Elements => "Elements",
            InspectorTab::Layout => "Layout",
            InspectorTab::Memory => "Memory",
            InspectorTab::Style => "Style",
        }
    }
}

/// Per-app inspector state: the active tab, selection, and last frame's
/// drained snapshot (what the Elements tab displays).
#[derive(Default)]
pub struct InspectorState {
    pub tab: InspectorTab,
    /// Selection survives frames where the widget is off-screen; the panel
    /// then shows "(not on screen this frame)".
    pub selected: Option<egui::Id>,
    pub snapshot: Vec<InspectInfo>,
    /// Set while drawing the Elements list; a hovered row wins over the
    /// selection as the highlight target.
    hovered_row: Option<egui::Id>,
}

impl InspectorState {
    /// Frame barrier — must remain the FINAL statement of `ui()`: child
    /// viewports register after the root CentralPanel, so an earlier drain
    /// would split one frame's widgets across two snapshots.
    ///
    /// Publishing the highlight target here (not mid-inspector-draw) keeps
    /// the one-frame lag uniform for root-panel widgets (drawn before the
    /// inspector window) and child-window widgets (drawn after it).
    pub fn end_frame(&mut self, open: bool) {
        set_enabled(open);
        // Drain even when closed so a stale frame's entries never linger.
        self.snapshot = take_frame();
        let target = self
            .hovered_row
            .take()
            .or(if open { self.selected } else { None });
        set_highlight(target);
    }
}

/// Full window contents: the tab row plus the active tab's body.
pub fn ui_contents(ui: &mut egui::Ui, state: &mut InspectorState) {
    ui.horizontal(|ui| {
        for tab in InspectorTab::ALL {
            ui.selectable_value(&mut state.tab, tab, tab.label());
        }
    });
    ui.separator();
    let ctx = ui.ctx().clone();
    match state.tab {
        InspectorTab::Elements => elements_tab(ui, state),
        InspectorTab::Layout => {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| ctx.inspection_ui(ui));
        }
        InspectorTab::Memory => {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| ctx.memory_ui(ui));
        }
        InspectorTab::Style => {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| ctx.settings_ui(ui));
        }
    }
}

fn elements_tab(ui: &mut egui::Ui, state: &mut InspectorState) {
    if state.snapshot.is_empty() {
        ui.label("Collecting widgets…");
        ui.weak(
            "Only widgets instrumented with .inspect() are listed; the list \
             fills in once a frame completes with the inspector open.",
        );
        ui.ctx().request_repaint();
        return;
    }
    ui.weak(format!(
        "{} widget(s) registered last frame — click to select, hover to highlight",
        state.snapshot.len()
    ));

    // Top half: the widget list. Bottom half: the properties panel.
    let list_height = (ui.available_height() * 0.5).max(120.0);
    egui::ScrollArea::vertical()
        .id_salt("insp_elements_list")
        .auto_shrink([false, false])
        .max_height(list_height)
        .show(ui, |ui| {
            for info in &state.snapshot {
                let selected = state.selected == Some(info.id);
                let row = ui.selectable_label(
                    selected,
                    format!(
                        "{} — {}:{}",
                        info.name,
                        info.location.file(),
                        info.location.line()
                    ),
                );
                if row.hovered() {
                    state.hovered_row = Some(info.id);
                }
                if row.clicked() {
                    // Click selects; clicking the selected row deselects.
                    state.selected = if selected { None } else { Some(info.id) };
                }
            }
        });
    ui.separator();

    let Some(sel) = state.selected else {
        ui.weak("Click a widget to see its properties. Hovering a row highlights it on screen.");
        return;
    };
    let Some(info) = state.snapshot.iter().find(|i| i.id == sel) else {
        ui.label(format!("{} — (not on screen this frame)", sel.short_debug_format()));
        ui.weak("The selection is kept; properties reappear when the widget renders again.");
        return;
    };
    let info = info.clone(); // release the borrow on state.snapshot for copy_text etc.
    egui::ScrollArea::vertical()
        .id_salt("insp_props_panel")
        .auto_shrink([false, false])
        .show(ui, |ui| {
            egui::Grid::new("insp_props_grid")
                .num_columns(2)
                .striped(true)
                .show(ui, |ui| {
                    ui.label("Name");
                    ui.label(&info.name);
                    ui.end_row();
                    ui.label("Id");
                    ui.label(info.id.short_debug_format());
                    ui.end_row();
                    ui.label("Rect");
                    ui.label(format!(
                        "({:.0}, {:.0}) → ({:.0}, {:.0})",
                        info.rect.min.x, info.rect.min.y, info.rect.max.x, info.rect.max.y
                    ));
                    ui.end_row();
                    ui.label("Size");
                    ui.label(format!(
                        "{:.0} × {:.0}",
                        info.rect.width(),
                        info.rect.height()
                    ));
                    ui.end_row();
                    ui.label("Interact rect");
                    ui.label(format!(
                        "({:.0}, {:.0}) → ({:.0}, {:.0})",
                        info.interact_rect.min.x,
                        info.interact_rect.min.y,
                        info.interact_rect.max.x,
                        info.interact_rect.max.y
                    ));
                    ui.end_row();
                    ui.label("Enabled");
                    ui.label(if info.enabled { "yes" } else { "no" });
                    ui.end_row();
                    ui.label("Hovered");
                    ui.label(if info.hovered { "yes" } else { "no" });
                    ui.end_row();
                    ui.label("Clicked");
                    ui.label(if info.clicked { "yes" } else { "no" });
                    ui.end_row();
                    ui.label("Viewport");
                    ui.label(format!("{:?}", info.viewport));
                    ui.end_row();
                    ui.label("Source");
                    ui.horizontal(|ui| {
                        let loc = format!(
                            "{}:{}:{}",
                            info.location.file(),
                            info.location.line(),
                            info.location.column()
                        );
                        ui.monospace(&loc);
                        if ui.small_button("📋").on_hover_text("Copy source location").clicked()
                        {
                            ui.ctx().copy_text(loc);
                        }
                    });
                    ui.end_row();
                    for (k, v) in &info.props {
                        ui.label(*k);
                        ui.label(v);
                        ui.end_row();
                    }
                });
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy(name: &str, id: egui::Id) -> InspectInfo {
        InspectInfo {
            name: name.to_string(),
            id,
            rect: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0)),
            interact_rect: egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(10.0, 10.0)),
            enabled: true,
            hovered: false,
            clicked: false,
            props: Vec::new(),
            location: std::panic::Location::caller(),
            viewport: egui::ViewportId::ROOT,
        }
    }

    #[test]
    fn record_and_take_frame_in_order() {
        record(dummy("a", egui::Id::new("a")));
        record(dummy("b", egui::Id::new("b")));
        let taken = take_frame();
        assert_eq!(taken.len(), 2);
        assert_eq!(taken[0].name, "a");
        assert_eq!(taken[1].name, "b");
        assert!(take_frame().is_empty(), "second take must be empty");
    }

    #[test]
    fn registry_is_thread_local() {
        record(dummy("here", egui::Id::new("here")));
        std::thread::spawn(|| {
            assert!(take_frame().is_empty(), "fresh thread sees an empty registry");
        })
        .join()
        .unwrap();
        assert_eq!(take_frame().len(), 1, "our entry survives the other thread's take");
    }

    #[test]
    fn track_caller_captures_call_site() {
        #[track_caller]
        fn capture() -> &'static std::panic::Location<'static> {
            std::panic::Location::caller()
        }
        let loc = capture();
        assert!(loc.file().ends_with("inspector.rs"), "file was {}", loc.file());
        assert!(loc.line() > 0);
    }

    #[test]
    fn highlight_roundtrip() {
        assert_eq!(highlight(), None, "default is None");
        let id = egui::Id::new("hl");
        set_highlight(Some(id));
        assert_eq!(highlight(), Some(id));
        set_highlight(None);
        assert_eq!(highlight(), None);
    }

    /// Everything touching the global ENABLED atomic lives in this ONE test
    /// so parallel tests never race on it (end_frame writes it too).
    #[test]
    fn end_frame_drains_publishes_and_gates() {
        let prior = is_enabled();
        let a = egui::Id::new("hover-target");
        let b = egui::Id::new("selected-target");

        let mut st = InspectorState::default();
        record(dummy("w", a));
        st.selected = Some(b);
        st.hovered_row = Some(a);
        st.end_frame(true);
        assert!(is_enabled());
        assert_eq!(st.snapshot.len(), 1, "end_frame drains into the snapshot");
        assert!(take_frame().is_empty(), "registry is empty after end_frame");
        assert_eq!(highlight(), Some(a), "hovered row beats the selection");

        st.end_frame(true);
        assert!(st.snapshot.is_empty());
        assert_eq!(highlight(), Some(b), "no hover falls back to the selection");

        st.end_frame(false);
        assert!(!is_enabled());
        assert_eq!(highlight(), None, "closed inspector publishes no highlight");

        set_enabled(prior);
        set_highlight(None);
    }
}
