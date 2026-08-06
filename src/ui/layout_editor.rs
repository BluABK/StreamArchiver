//! The "🖌 Custom…" window from either "Play all collab instances" Layout
//! submenu: a to-scale, drag/resize canvas — one rectangle per connected
//! physical display, drawn in their real relative arrangement — for placing
//! each collab angle exactly where it should land before playing (and,
//! optionally, saving the arrangement as a reusable named preset via
//! `crate::layout`).

use super::*;

/// One collab angle's placement on the canvas — `label` is display-only
/// (see `grid::generic_layout_labels`), `monitor_index`/`rect_frac` are
/// exactly a `crate::layout::LayoutSlot`'s fields, kept apart here only so
/// the label can travel alongside them while the user drags.
pub(super) struct LayoutEntry {
    pub(super) label: String,
    pub(super) monitor_index: usize,
    pub(super) rect_frac: (f32, f32, f32, f32),
}

pub(super) struct LayoutEditorPopupState {
    pub(super) monitors: Vec<crate::display::PhysicalMonitor>,
    pub(super) entries: Vec<LayoutEntry>,
    /// What to actually play once "Apply now"/"Save as preset…" fires —
    /// carried through unchanged from the menu click that opened this editor.
    pub(super) targets: super::grid::LayoutEditorTargets,
    pub(super) name_draft: String,
    pub(super) do_apply: bool,
    pub(super) do_save_as: bool,
    pub(super) closed: bool,
}

/// Distinct colors for up to a handful of chips before repeating — enough to
/// tell collab angles apart at a glance; a repeat beyond this count just
/// means two chips share a color, which the label still disambiguates.
const CHIP_COLORS: [egui::Color32; 6] = [
    egui::Color32::from_rgb(0x4a, 0x9e, 0xd6),
    egui::Color32::from_rgb(0xd6, 0x8a, 0x4a),
    egui::Color32::from_rgb(0x7a, 0xc9, 0x6a),
    egui::Color32::from_rgb(0xc9, 0x6a, 0xa8),
    egui::Color32::from_rgb(0xc9, 0xb8, 0x4a),
    egui::Color32::from_rgb(0x6a, 0xc9, 0xc0),
];

fn entry_pixel_rect(entry: &LayoutEntry, monitors: &[crate::display::PhysicalMonitor]) -> crate::display::PixelRect {
    let m = crate::display::resolve_monitor(monitors, entry.monitor_index);
    crate::layout::frac_to_pixels(m.work_rect, entry.rect_frac)
}

/// Draw every monitor + every entry chip, handling drag (move) and a
/// bottom-right corner handle (resize). Dragging a chip past its current
/// monitor's bounds re-homes it onto whichever monitor now contains its
/// center — crossing a display boundary "just works" the way it visually
/// looks like it should.
fn render_canvas(ui: &mut egui::Ui, s: &mut LayoutEditorPopupState) {
    if s.monitors.is_empty() {
        ui.colored_label(egui::Color32::YELLOW, "No displays detected.");
        return;
    }
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
    for m in &s.monitors {
        min_x = min_x.min(m.rect.x);
        min_y = min_y.min(m.rect.y);
        max_x = max_x.max(m.rect.x + m.rect.w);
        max_y = max_y.max(m.rect.y + m.rect.h);
    }
    let bbox_w = (max_x - min_x).max(1) as f32;
    let bbox_h = (max_y - min_y).max(1) as f32;

    let avail = ui.available_size_before_wrap();
    let canvas_size = egui::vec2(avail.x, (avail.y - 44.0).max(200.0));
    let scale = (canvas_size.x / bbox_w).min(canvas_size.y / bbox_h).max(0.001);
    let drawn = egui::vec2(bbox_w * scale, bbox_h * scale);

    let (rect, _resp) = ui.allocate_exact_size(canvas_size, egui::Sense::hover());
    let origin = rect.min + (canvas_size - drawn) / 2.0;
    let to_screen = |x: i32, y: i32| -> egui::Pos2 {
        origin + egui::vec2((x - min_x) as f32, (y - min_y) as f32) * scale
    };

    let painter = ui.painter_at(rect);
    for m in &s.monitors {
        let r = egui::Rect::from_min_max(
            to_screen(m.rect.x, m.rect.y),
            to_screen(m.rect.x + m.rect.w, m.rect.y + m.rect.h),
        );
        painter.rect_filled(r, 2.0, ui.visuals().extreme_bg_color);
        painter.rect_stroke(r, 2.0, ui.visuals().widgets.noninteractive.fg_stroke, egui::StrokeKind::Inside);
        painter.text(
            r.min + egui::vec2(4.0, 2.0),
            egui::Align2::LEFT_TOP,
            format!("{}{}", m.name, if m.is_primary { " (primary)" } else { "" }),
            egui::FontId::monospace(11.0),
            ui.visuals().weak_text_color(),
        );
    }

    for i in 0..s.entries.len() {
        let abs = entry_pixel_rect(&s.entries[i], &s.monitors);
        let chip_rect =
            egui::Rect::from_min_max(to_screen(abs.x, abs.y), to_screen(abs.x + abs.w, abs.y + abs.h));
        let color = CHIP_COLORS[i % CHIP_COLORS.len()];
        let move_id = ui.id().with("layout_chip_move").with(i);
        let move_resp = ui.interact(chip_rect, move_id, egui::Sense::click_and_drag());
        painter.rect_filled(chip_rect, 3.0, color.linear_multiply(0.35));
        painter.rect_stroke(chip_rect, 3.0, egui::Stroke::new(2.0, color), egui::StrokeKind::Inside);
        painter.text(
            chip_rect.center(),
            egui::Align2::CENTER_CENTER,
            &s.entries[i].label,
            egui::FontId::proportional(13.0),
            ui.visuals().text_color(),
        );

        if move_resp.dragged() {
            let delta = move_resp.drag_delta() / scale;
            let new_x = abs.x + delta.x.round() as i32;
            let new_y = abs.y + delta.y.round() as i32;
            let center = (new_x + abs.w / 2, new_y + abs.h / 2);
            let home = s
                .monitors
                .iter()
                .find(|m| {
                    center.0 >= m.rect.x
                        && center.0 < m.rect.x + m.rect.w
                        && center.1 >= m.rect.y
                        && center.1 < m.rect.y + m.rect.h
                })
                .unwrap_or(&s.monitors[s.entries[i].monitor_index.min(s.monitors.len() - 1)]);
            let new_abs = crate::display::PixelRect { x: new_x, y: new_y, w: abs.w, h: abs.h };
            let frac = crate::layout::pixels_to_frac(home.work_rect, new_abs);
            s.entries[i].monitor_index = home.index;
            s.entries[i].rect_frac = frac;
        }

        let handle_size = 10.0;
        let handle_rect = egui::Rect::from_min_size(
            chip_rect.max - egui::vec2(handle_size, handle_size),
            egui::vec2(handle_size, handle_size),
        );
        let resize_id = ui.id().with("layout_chip_resize").with(i);
        let resize_resp = ui.interact(handle_rect, resize_id, egui::Sense::drag());
        painter.rect_filled(handle_rect, 1.0, ui.visuals().strong_text_color());
        if resize_resp.dragged() {
            let delta = resize_resp.drag_delta() / scale;
            let home = crate::display::resolve_monitor(&s.monitors, s.entries[i].monitor_index);
            let min_frac_w = (80.0 / home.work_rect.w.max(1) as f32).min(1.0);
            let min_frac_h = (60.0 / home.work_rect.h.max(1) as f32).min(1.0);
            let new_w = ((abs.w as f32 + delta.x) / home.work_rect.w.max(1) as f32).max(min_frac_w);
            let new_h = ((abs.h as f32 + delta.y) / home.work_rect.h.max(1) as f32).max(min_frac_h);
            s.entries[i].rect_frac.2 = new_w.clamp(0.0, 1.0 - s.entries[i].rect_frac.0.max(0.0));
            s.entries[i].rect_frac.3 = new_h.clamp(0.0, 1.0 - s.entries[i].rect_frac.1.max(0.0));
        }
    }
}

impl StreamArchiverApp {
    /// Seed a fresh Custom layout editor from a menu click — one entry per
    /// label, initially arranged via [`crate::layout::BuiltinPreset::TileEqually`]
    /// on the primary monitor as a reasonable drag-from-here starting point.
    pub(super) fn open_layout_editor(
        &mut self,
        labels: Vec<String>,
        targets: super::grid::LayoutEditorTargets,
    ) {
        let monitors = crate::display::enumerate_monitors();
        let primary = crate::display::primary_or_fallback(&monitors);
        let seed_rects =
            crate::layout::resolve_builtin(crate::layout::BuiltinPreset::TileEqually, &primary, labels.len());
        let entries = labels
            .into_iter()
            .zip(seed_rects)
            .map(|(label, rect)| LayoutEntry {
                label,
                monitor_index: primary.index,
                rect_frac: crate::layout::pixels_to_frac(primary.work_rect, rect),
            })
            .collect();
        self.layout_editor = Some(Arc::new(Mutex::new(LayoutEditorPopupState {
            monitors,
            entries,
            targets,
            name_draft: String::new(),
            do_apply: false,
            do_save_as: false,
            closed: false,
        })));
    }

    /// Dispatch whatever a Custom layout was built for — shared by "Apply
    /// now" and "Save as preset…" (which saves, then applies the same way).
    fn dispatch_layout_editor_targets(
        &mut self,
        targets: super::grid::LayoutEditorTargets,
        choice: crate::layout::LayoutChoice,
    ) {
        match targets {
            super::grid::LayoutEditorTargets::Current(v) => self.dispatch_play_collab_current(v, choice),
            super::grid::LayoutEditorTargets::LiveEdge(mid, partners, untracked) => {
                self.dispatch_play_collab_live_edge(mid, partners, untracked, choice)
            }
        }
    }

    #[allow(deprecated)] // CentralPanel::show(ctx) is correct inside a viewport closure
    pub(super) fn layout_editor_window(&mut self, ctx: &egui::Context) {
        let Some(state) = self.layout_editor.clone() else {
            return;
        };
        let shared = self.popup_shared();
        show_deferred_popup(
            ctx,
            egui::ViewportId::from_hash_of("layout_editor_vp"),
            egui::ViewportBuilder::default().with_title("Custom layout").with_inner_size([820.0, 560.0]),
            state.clone(),
            shared,
            |ctx, s, _shared| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    s.closed = true;
                }
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.label(
                        "Drag a chip to move it (across displays too); drag its bottom-right \
                         corner to resize.",
                    );
                    render_canvas(ui, s);
                    ui.separator();
                    ui.horizontal(|ui| {
                        if ui.button("Apply now").clicked() {
                            s.do_apply = true;
                        }
                        ui.add(
                            egui::TextEdit::singleline(&mut s.name_draft)
                                .hint_text("Preset name")
                                .desired_width(160.0),
                        );
                        let can_save = !s.name_draft.trim().is_empty();
                        if ui.add_enabled(can_save, egui::Button::new("Save as preset…")).clicked() {
                            s.do_save_as = true;
                        }
                        if ui.button("Cancel").clicked() {
                            s.closed = true;
                        }
                    });
                });
            },
        );

        let (do_apply, do_save_as, closed) = {
            let s = state.lock().unwrap();
            (s.do_apply, s.do_save_as, s.closed)
        };
        if do_save_as {
            let (name, slots) = {
                let s = state.lock().unwrap();
                let slots = s
                    .entries
                    .iter()
                    .map(|e| crate::layout::LayoutSlot {
                        monitor_index: e.monitor_index,
                        rect_frac: e.rect_frac,
                    })
                    .collect::<Vec<_>>();
                (s.name_draft.trim().to_string(), slots)
            };
            crate::layout::upsert_layout(
                &self.core.store,
                crate::layout::CustomLayout { name: name.clone(), slots: slots.clone() },
            );
            self.status = format!("Layout \"{name}\" saved.");
            let targets = state.lock().unwrap().targets.clone();
            self.dispatch_layout_editor_targets(targets, crate::layout::LayoutChoice::Custom(slots));
            self.layout_editor = None;
        } else if do_apply {
            let (targets, slots) = {
                let s = state.lock().unwrap();
                let slots = s
                    .entries
                    .iter()
                    .map(|e| crate::layout::LayoutSlot {
                        monitor_index: e.monitor_index,
                        rect_frac: e.rect_frac,
                    })
                    .collect::<Vec<_>>();
                (s.targets.clone(), slots)
            };
            self.dispatch_layout_editor_targets(targets, crate::layout::LayoutChoice::Custom(slots));
            self.layout_editor = None;
        } else if closed {
            self.layout_editor = None;
        }
    }
}
