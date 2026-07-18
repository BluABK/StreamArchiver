//! GVS PO token server log window: a live tail of the managed server's
//! combined stdout+stderr (`logs\pot_server.log`), opened from Settings →
//! Downloads → GVS PO token server or from the Background view's status line.

use super::*;

/// How much of the log's tail is loaded per refresh. The file is small (a few
/// KB of startup + token-generation lines per session) — 64 KiB covers hours.
const POT_LOG_TAIL_BYTES: u64 = 64 * 1024;

/// Read the last [`POT_LOG_TAIL_BYTES`] of the server log, starting at a line
/// boundary. Sync on the UI thread by design: the file lives under the app's
/// data dir on the system drive (not a recordings drive), and the read is
/// throttled to once per second while the window is open.
fn read_pot_log_tail() -> String {
    use std::io::{Read, Seek, SeekFrom};
    let path = crate::pot_server::log_path();
    let Ok(mut f) = crate::iomon::fs::open_sync(crate::iomon::Cat::LogRead, &path) else {
        return format!("({} does not exist yet — the server hasn't been launched)", path.display());
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(POT_LOG_TAIL_BYTES);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::with_capacity((len - start) as usize);
    let read_start = std::time::Instant::now();
    let _ = f.read_to_end(&mut buf);
    crate::iomon::record(
        crate::iomon::Cat::LogRead,
        &path,
        crate::iomon::OpKind::Read,
        buf.len() as u64,
        read_start.elapsed(),
    );
    let mut text = String::from_utf8_lossy(&buf).into_owned();
    // Drop the leading partial line of a mid-file start.
    if start > 0
        && let Some(nl) = text.find('\n')
    {
        text.drain(..=nl);
    }
    if text.trim().is_empty() {
        text = "(log is empty — the server writes here once launched)".to_string();
    }
    text
}

impl StreamArchiverApp {
    /// Separate-viewport live tail of the PO token server log. Mirrors the
    /// `notifications_window` pattern: gated on its `show_*` bool, refreshed
    /// on a throttle while open, closed via the OS close button.
    #[allow(deprecated)] // CentralPanel::show inside a viewport (matches notifications_window)
    pub(super) fn pot_server_log_window(&mut self, ctx: &egui::Context) {
        if !self.show_pot_server_log {
            return;
        }
        let stale = self
            .pot_log_refreshed
            .map(|t| t.elapsed() >= std::time::Duration::from_secs(1))
            .unwrap_or(true);
        if stale {
            self.pot_log_text = read_pot_log_tail();
            self.pot_log_refreshed = Some(std::time::Instant::now());
        }
        let mut open = true;
        ctx.show_viewport_immediate(
            egui::ViewportId::from_hash_of("pot_server_log_vp"),
            egui::ViewportBuilder::default()
                .with_title("🎫 GVS PO token server log")
                .with_inner_size([760.0, 480.0]),
            |ctx, _class| {
                if ctx.input(|i| i.viewport().close_requested()) {
                    open = false;
                }
                // Keep tailing while open (repaint drives the 1s refresh above).
                ctx.request_repaint_after(std::time::Duration::from_secs(1));
                egui::CentralPanel::default().show(ctx, |ui| {
                    ui.horizontal(|ui| {
                        let st = crate::pot_server::status();
                        ui.weak(format!(
                            "{} · last 64 KiB of {}",
                            st.base_url,
                            crate::pot_server::log_path().display()
                        ))
                        .on_hover_text(
                            "Combined stdout+stderr of the managed server. Truncated at \
                             the first launch of each app run; restarts within a run \
                             append, so earlier crash output is preserved.",
                        );
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui
                                .button("📂 Open file")
                                .on_hover_text("Open the full log in the default viewer.")
                                .clicked()
                            {
                                crate::platform::open_path(&crate::pot_server::log_path());
                            }
                        });
                    });
                    ui.separator();
                    egui::ScrollArea::vertical()
                        .auto_shrink([false, false])
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            ui.add(
                                egui::Label::new(
                                    egui::RichText::new(&self.pot_log_text).monospace(),
                                )
                                .wrap_mode(egui::TextWrapMode::Extend),
                            );
                        });
                });
            },
        );
        if !open {
            self.show_pot_server_log = false;
        }
    }
}
