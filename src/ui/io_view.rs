//! I/O monitor tab.

use super::*;

/// Which series set the I/O tab's rate graph shows.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum IoPlotMetric {
    /// Write B/s: tools, app, physical disks.
    Write,
    /// Read B/s (capture-tool reads are mostly CDN network).
    Read,
    /// Physical-disk queue depth (the USB-drop early-warning signal).
    Queue,
}
/// Plain-text dump of the I/O monitor state for the "Copy summary" button.
pub(super) fn io_summary_text(
    snap: &crate::iomon::CountersSnapshot,
    latest: Option<&crate::iomon::Sample>,
) -> String {
    use crate::iomon::{Cat, Region};
    let mut out = format!(
        "StreamArchiver I/O summary — {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
    );
    if let Some(s) = latest {
        out.push_str(&format!(
            "tools: read {}/s write {}/s | app: read {}/s write {}/s | unattributed {}/s | db+wal {}\n",
            fmt_bytes(s.child_read_bps as i64),
            fmt_bytes(s.child_write_bps as i64),
            fmt_bytes(s.self_read_bps as i64),
            fmt_bytes(s.self_write_bps as i64),
            fmt_bytes(s.unattributed_bps as i64),
            fmt_bytes(s.db_bytes as i64),
        ));
        for d in &s.disks {
            out.push_str(&format!(
                "disk {}: read {}/s write {}/s queue {}\n",
                d.letter,
                fmt_bytes(d.read_bps as i64),
                fmt_bytes(d.write_bps as i64),
                d.queue_depth
            ));
        }
        for p in &s.procs {
            out.push_str(&format!(
                "proc {} [{} pid {} {}]: read {}/s write {}/s (total {} / {})\n",
                p.label,
                p.tool,
                p.pid,
                p.purpose,
                fmt_bytes(p.read_bps as i64),
                fmt_bytes(p.write_bps as i64),
                fmt_bytes(p.total_read as i64),
                fmt_bytes(p.total_write as i64),
            ));
        }
    }
    out.push_str("\nregion | total read | total written | ops | slow\n");
    for region in Region::ALL {
        let t = snap.region_total(region);
        out.push_str(&format!(
            "{} | {} | {} | {} | {}\n",
            region.label(),
            fmt_bytes(t.read_bytes as i64),
            fmt_bytes(t.write_bytes as i64),
            t.ops(),
            t.slow_ops
        ));
    }
    out.push_str("\ncategory | total read | total written | ops | slow | max op | time\n");
    for cat in Cat::ALL {
        let t = snap.cat_total(cat);
        if t.ops() == 0 {
            continue;
        }
        out.push_str(&format!(
            "{} | {} | {} | {} | {} | {} | {:.1}s\n",
            cat.label(),
            fmt_bytes(t.read_bytes as i64),
            fmt_bytes(t.write_bytes as i64),
            t.ops(),
            t.slow_ops,
            fmt_bytes(t.max_op_bytes as i64),
            t.total_ns as f64 / 1e9
        ));
    }
    out
}

pub(super) fn fmt_bytes(bytes: i64) -> String {
    let b = bytes.max(0) as f64;
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    if b >= GB {
        format!("{:.2} GB", b / GB)
    } else if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{b:.0} B")
    }
}
/// Human-readable download speed (e.g. `1.2 MB/s`); empty when not downloading.
pub(super) fn fmt_speed(bytes_per_sec: f64) -> String {
    if bytes_per_sec <= 0.0 {
        return String::new();
    }
    format!("{}/s", fmt_bytes(bytes_per_sec as i64))
}

impl StreamArchiverApp {
    /// The I/O tab: live disk-load monitor — per-drive/per-category rates,
    /// per-process tool I/O, physical-disk queue depth, and a recent-ops log.
    /// All data comes from `iomon`'s atomics/rings/sampler; this fn never
    /// touches the filesystem.
    pub(super) fn io_view(&mut self, ui: &mut egui::Ui) {
        use crate::iomon::{self, Cat, Region};
        // Live tab: tick once a second, refresh the cached copies at most 1×/s.
        ui.ctx().request_repaint_after(std::time::Duration::from_secs(1));
        let stale = self
            .io_refreshed
            .map(|t| t.elapsed().as_millis() >= 900)
            .unwrap_or(true);
        if stale {
            self.io_hist = iomon::history();
            self.io_snap = Some(iomon::snapshot());
            self.io_refreshed = Some(std::time::Instant::now());
        }
        let latest = self.io_hist.last().cloned();
        let Some(snap) = self.io_snap.clone() else { return };
        let bps = |v: u64| format!("{}/s", fmt_bytes(v as i64));

        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ── Totals strip ─────────────────────────────────────────────
            ui.horizontal_wrapped(|ui| {
                ui.heading("I/O monitor");
                ui.separator();
                match &latest {
                    Some(s) => {
                        ui.label(format!("tools ↓{} ↑{}", bps(s.child_read_bps), bps(s.child_write_bps)))
                            .on_hover_text("All spawned capture/post-processing tools + their descendants. Read side of capture tools is mostly CDN network.");
                        ui.separator();
                        ui.label(format!("app ↓{} ↑{}", bps(s.self_read_bps), bps(s.self_write_bps)))
                            .on_hover_text("This process (GetProcessIoCounters) — includes network sockets.");
                        ui.separator();
                        ui.label(format!("unattributed {}", bps(s.unattributed_bps)))
                            .on_hover_text("App-process I/O the in-process instrumentation didn't classify: the app's own network traffic (API polls, image downloads), SQLite internals, the tracing log writer, egui persistence.");
                        ui.separator();
                        ui.label(format!("db+wal {}", fmt_bytes(s.db_bytes as i64)));
                        let age = chrono::Utc::now().timestamp_millis() - s.at_ms;
                        if age > 5_000 {
                            ui.separator();
                            ui.colored_label(ui.visuals().warn_fg_color, format!("sampler stalled? last sample {}s ago", age / 1000));
                        }
                    }
                    None => {
                        ui.label("sampler warming up…");
                    }
                }
            });
            ui.horizontal(|ui| {
                if ui.button("📋 Copy summary").clicked() {
                    ui.ctx().copy_text(io_summary_text(&snap, latest.as_ref()));
                }
                if ui.button("📂 Open sample log folder").clicked() {
                    crate::platform::open_path(&iomon::sample_log_dir());
                }
                let fin = iomon::finished_child_totals();
                ui.label(format!(
                    "session: {} finished tool(s), ↓{} ↑{}",
                    fin.count,
                    fmt_bytes(fin.read_bytes as i64),
                    fmt_bytes(fin.write_bytes as i64)
                ));
                if !iomon::sample_logging() {
                    ui.colored_label(ui.visuals().weak_text_color(), "sample log off (Settings → Recording)");
                }
            });
            ui.separator();

            // ── Rate graph ───────────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.io_plot_metric, IoPlotMetric::Write, "Write B/s");
                ui.selectable_value(&mut self.io_plot_metric, IoPlotMetric::Read, "Read B/s");
                ui.selectable_value(&mut self.io_plot_metric, IoPlotMetric::Queue, "Disk queue depth");
            });
            let metric = self.io_plot_metric;
            let now_ms = latest.as_ref().map(|s| s.at_ms).unwrap_or(0);
            fn pts_of(
                hist: &[iomon::Sample],
                now_ms: i64,
                f: impl Fn(&iomon::Sample) -> f64,
            ) -> Vec<[f64; 2]> {
                hist.iter()
                    .map(|s| [(s.at_ms - now_ms) as f64 / 1000.0, f(s)])
                    .collect()
            }
            // Series: (name, points)
            let mut series: Vec<(String, Vec<[f64; 2]>)> = Vec::new();
            match metric {
                IoPlotMetric::Write => {
                    series.push(("tools".into(), pts_of(&self.io_hist, now_ms, |s| s.child_write_bps as f64)));
                    series.push(("app".into(), pts_of(&self.io_hist, now_ms, |s| s.self_write_bps as f64)));
                    series.push((
                        "recordings (in-app)".into(),
                        pts_of(&self.io_hist, now_ms, |s| s.per_region[Region::Recordings as usize].1 as f64),
                    ));
                }
                IoPlotMetric::Read => {
                    series.push(("tools".into(), pts_of(&self.io_hist, now_ms, |s| s.child_read_bps as f64)));
                    series.push(("app".into(), pts_of(&self.io_hist, now_ms, |s| s.self_read_bps as f64)));
                    series.push((
                        "recordings (in-app)".into(),
                        pts_of(&self.io_hist, now_ms, |s| s.per_region[Region::Recordings as usize].0 as f64),
                    ));
                }
                IoPlotMetric::Queue => {}
            }
            // Per-physical-disk series (letters present in the history).
            let mut letters: Vec<char> = Vec::new();
            for s in &self.io_hist {
                for d in &s.disks {
                    if !letters.contains(&d.letter) {
                        letters.push(d.letter);
                    }
                }
            }
            for l in letters {
                let pts = pts_of(&self.io_hist, now_ms, |s| {
                    s.disks
                        .iter()
                        .find(|d| d.letter == l)
                        .map(|d| match metric {
                            IoPlotMetric::Write => d.write_bps as f64,
                            IoPlotMetric::Read => d.read_bps as f64,
                            IoPlotMetric::Queue => d.queue_depth as f64,
                        })
                        .unwrap_or(0.0)
                });
                series.push((format!("disk {l}:"), pts));
            }
            let is_bytes = metric != IoPlotMetric::Queue;
            egui_plot::Plot::new("io_rate_plot")
                .height(200.0)
                .legend(egui_plot::Legend::default())
                .allow_scroll(false)
                .include_y(0.0)
                .x_axis_formatter(|mark, _| format!("{:.0}s", mark.value))
                .y_axis_formatter(move |mark, _| {
                    if is_bytes {
                        format!("{}/s", fmt_bytes(mark.value.max(0.0) as i64))
                    } else {
                        format!("{:.0}", mark.value)
                    }
                })
                .label_formatter(move |name, value| {
                    let v = if is_bytes {
                        format!("{}/s", fmt_bytes(value.y.max(0.0) as i64))
                    } else {
                        format!("{:.0}", value.y)
                    };
                    format!("{name}\n{:.0}s: {v}", value.x)
                })
                .show(ui, |plot_ui| {
                    for (name, pts) in series {
                        plot_ui.line(egui_plot::Line::new(name, egui_plot::PlotPoints::from(pts)));
                    }
                });
            ui.separator();

            // ── Storage regions + physical disks ─────────────────────────
            ui.columns(2, |cols| {
                cols[0].strong("In-app I/O by storage region");
                egui::Grid::new("io_regions").striped(true).min_col_width(60.0).show(&mut cols[0], |ui| {
                    ui.weak("region");
                    ui.weak("read");
                    ui.weak("write");
                    ui.weak("total read");
                    ui.weak("total written");
                    ui.weak("slow ops");
                    ui.end_row();
                    for region in Region::ALL {
                        let tot = snap.region_total(region);
                        let (r, w) = latest.as_ref().map(|s| s.per_region[region as usize]).unwrap_or((0, 0));
                        if region == Region::Recordings {
                            ui.colored_label(ui.visuals().warn_fg_color, region.label());
                        } else {
                            ui.label(region.label());
                        }
                        ui.label(bps(r));
                        ui.label(bps(w));
                        ui.label(fmt_bytes(tot.read_bytes as i64));
                        ui.label(fmt_bytes(tot.write_bytes as i64));
                        ui.label(tot.slow_ops.to_string());
                        ui.end_row();
                    }
                });
                cols[1].strong("Physical disks (whole spindle, all processes)");
                let qstats = iomon::disk_queue_stats();
                match latest.as_ref().filter(|s| !s.disks.is_empty()) {
                    Some(s) => {
                        // Whole-spindle rate minus everything this app and its
                        // tools account for on that drive = other programs
                        // (backup clients, antivirus, indexers) — the mystery
                        // load that queue-depth spikes otherwise get blamed on.
                        let rec_letters = iomon::recordings_drive_letters();
                        let data_letter = iomon::data_drive_letter();
                        let attributed = |letter: char| -> u64 {
                            let region_on_disk = |reg: iomon::Region| {
                                if rec_letters.contains(&letter) {
                                    reg == iomon::Region::Recordings
                                } else if Some(letter) == data_letter {
                                    matches!(reg, iomon::Region::AppData | iomon::Region::Temp)
                                } else {
                                    false
                                }
                            };
                            let mut total = 0u64;
                            for reg in iomon::Region::ALL {
                                if region_on_disk(reg) {
                                    let (r, w) = s.per_region[reg as usize];
                                    total += r + w;
                                }
                            }
                            for p in &s.procs {
                                if region_on_disk(p.region) {
                                    total += p.read_bps + p.write_bps;
                                }
                            }
                            total
                        };
                        egui::Grid::new("io_disks").striped(true).min_col_width(60.0).show(&mut cols[1], |ui| {
                            ui.weak("disk");
                            ui.weak("read");
                            ui.weak("write");
                            ui.weak("other").on_hover_text(
                                "Spindle traffic NOT accounted for by this app or its tool \
                                 processes — other programs hitting the same drive (backup \
                                 clients, antivirus scans, search indexers). Approximate: \
                                 tool 'read' rates include their network transfer, and \
                                 in-app rates count file bytes, not sector overhead.",
                            );
                            ui.weak("queue");
                            ui.weak("avg");
                            ui.weak("max").on_hover_text(
                                "Session peak queue depth and how long it sat there. \
                                 Hover a value for the top pressure episodes.",
                            );
                            ui.end_row();
                            for d in &s.disks {
                                ui.label(format!("{}:", d.letter));
                                ui.label(bps(d.read_bps));
                                ui.label(bps(d.write_bps));
                                let spindle = d.read_bps + d.write_bps;
                                let other = spindle.saturating_sub(attributed(d.letter));
                                // Only alarming when foreign load dominates a busy disk.
                                if other > 1024 * 1024 && other * 2 > spindle {
                                    ui.colored_label(ui.visuals().warn_fg_color, bps(other))
                                        .on_hover_text(
                                            "Most of this drive's traffic right now is from \
                                             OTHER programs — check backup/antivirus/indexer \
                                             activity before blaming a capture or remux.",
                                        );
                                } else {
                                    ui.label(bps(other));
                                }
                                if d.queue_depth >= 4 {
                                    ui.colored_label(ui.visuals().error_fg_color, d.queue_depth.to_string());
                                } else {
                                    ui.label(d.queue_depth.to_string());
                                }
                                match qstats.iter().find(|(l, _)| *l == d.letter) {
                                    Some((_, st)) => {
                                        ui.label(format!("{:.2}", st.avg()));
                                        let max_label = format!("{} ({}s)", st.max_depth, st.max_secs);
                                        let resp = if st.max_depth >= 4 {
                                            ui.colored_label(ui.visuals().warn_fg_color, max_label)
                                        } else {
                                            ui.label(max_label)
                                        };
                                        if !st.top.is_empty() {
                                            let lines: Vec<String> = st
                                                .top
                                                .iter()
                                                .map(|e| {
                                                    let when = chrono::DateTime::from_timestamp_millis(e.ended_at_ms)
                                                        .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S").to_string())
                                                        .unwrap_or_default();
                                                    if e.ongoing {
                                                        format!("depth {} for {}s — ongoing", e.peak, e.secs)
                                                    } else {
                                                        format!("depth {} for {}s — ended {when}", e.peak, e.secs)
                                                    }
                                                })
                                                .collect();
                                            resp.on_hover_text(format!(
                                                "Top pressure episodes (queue ≥ 2) this session:\n{}",
                                                lines.join("\n")
                                            ));
                                        }
                                    }
                                    None => {
                                        ui.label("-");
                                        ui.label("-");
                                    }
                                }
                                ui.end_row();
                            }
                        });
                        cols[1].weak("Sustained queue depth on the USB enclosure is the early-warning signal before it drops off the bus.");
                    }
                    None => {
                        cols[1].weak("n/a (disk performance counters unavailable)");
                    }
                }
            });
            ui.separator();

            // ── Per-process table ────────────────────────────────────────
            ui.strong("Tool processes (1s samples; descendants rolled up)");
            match latest.as_ref().filter(|s| !s.procs.is_empty()) {
                Some(s) => {
                    egui::Grid::new("io_procs").striped(true).min_col_width(60.0).show(ui, |ui| {
                        ui.weak("what");
                        ui.weak("tool");
                        ui.weak("pid");
                        ui.weak("purpose");
                        ui.weak("region");
                        ui.weak("read");
                        ui.weak("write");
                        ui.weak("total read");
                        ui.weak("total written");
                        ui.weak("procs");
                        ui.end_row();
                        for p in &s.procs {
                            ui.label(&p.label);
                            // What the tree is ACTUALLY running right now (a
                            // capture's yt-dlp switches to "yt-dlp + ffmpeg"
                            // during its post-stream merge); the registered
                            // tool stays in the hover.
                            let running = if p.tree.is_empty() { p.tool.as_str() } else { p.tree.as_str() };
                            ui.label(running)
                                .on_hover_text(format!("registered as: {}", p.tool));
                            ui.label(p.pid.to_string());
                            // The job's step, live: when a tool-internal ffmpeg
                            // appears under a downloader, the job has moved from
                            // downloading to an ffmpeg stage (post-stream format
                            // merge, HLS remux) — exactly the full-disk-speed
                            // phase worth spotting.
                            let ffmpeg_child =
                                p.tree.split(" + ").skip(1).any(|n| n == "ffmpeg");
                            if ffmpeg_child {
                                ui.label(format!("{} · ffmpeg pass", p.purpose)).on_hover_text(
                                    "A tool-internal ffmpeg is running under this job right \
                                     now — the post-stream format merge, or the tool's own \
                                     HLS/remux stage. These run at full disk speed unless \
                                     throttled via Settings → Recording → \"yt-dlp ffmpeg \
                                     throttle\".",
                                );
                            } else {
                                ui.label(&p.purpose);
                            }
                            ui.label(p.region.label());
                            ui.label(bps(p.read_bps));
                            ui.label(bps(p.write_bps));
                            ui.label(fmt_bytes(p.total_read as i64));
                            ui.label(fmt_bytes(p.total_write as i64));
                            ui.label((1 + p.descendants).to_string());
                            ui.end_row();
                        }
                    });
                }
                None => {
                    ui.weak("no tool processes running");
                }
            }
            ui.separator();

            // ── Per-category table (click headers to sort) ───────────────
            ui.strong("In-app I/O by category");
            let cat_bps = |cat: Cat| -> (u64, u64) {
                latest.as_ref().map(|s| s.per_cat[cat as usize]).unwrap_or((0, 0))
            };
            let mut rows: Vec<(Cat, crate::iomon::CellSnap, (u64, u64))> = Cat::ALL
                .iter()
                .map(|&c| (c, snap.cat_total(c), cat_bps(c)))
                .collect();
            let (sort_col, asc) = self.io_cat_sort;
            rows.sort_by_key(|(c, t, (rb, wb))| {
                let k = match sort_col {
                    1 => *rb as u128,
                    2 => *wb as u128,
                    3 => t.ops() as u128,
                    4 => t.bytes() as u128,
                    5 => t.max_op_bytes as u128,
                    6 => t.slow_ops as u128,
                    7 => t.total_ns as u128,
                    _ => return (0u128, c.label().to_string()),
                };
                (if asc { k } else { u128::MAX - k }, c.label().to_string())
            });
            let header = |ui: &mut egui::Ui, idx: usize, label: &str, sort: &mut (usize, bool)| {
                let marker = if sort.0 == idx { if sort.1 { " ⏶" } else { " ⏷" } } else { "" };
                if ui.button(format!("{label}{marker}")).clicked() {
                    *sort = if sort.0 == idx { (idx, !sort.1) } else { (idx, false) };
                }
            };
            let mut sort = self.io_cat_sort;
            egui::Grid::new("io_cats").striped(true).min_col_width(60.0).show(ui, |ui| {
                header(ui, 0, "category", &mut sort);
                header(ui, 1, "read", &mut sort);
                header(ui, 2, "write", &mut sort);
                header(ui, 3, "ops", &mut sort);
                header(ui, 4, "total bytes", &mut sort);
                header(ui, 5, "max op", &mut sort);
                header(ui, 6, "slow", &mut sort);
                header(ui, 7, "time", &mut sort);
                ui.end_row();
                for (cat, tot, (rb, wb)) in &rows {
                    if tot.ops() == 0 && *rb == 0 && *wb == 0 {
                        continue; // nothing ever happened in this category
                    }
                    ui.label(cat.label());
                    ui.label(bps(*rb));
                    ui.label(bps(*wb));
                    ui.label(tot.ops().to_string());
                    ui.label(fmt_bytes(tot.bytes() as i64));
                    ui.label(fmt_bytes(tot.max_op_bytes as i64));
                    if tot.slow_ops > 0 {
                        ui.colored_label(ui.visuals().warn_fg_color, tot.slow_ops.to_string());
                    } else {
                        ui.label("0");
                    }
                    ui.label(format!("{:.1}s", tot.total_ns as f64 / 1e9))
                        .on_hover_text("Cumulative wall time spent inside filesystem calls (for the database: lock hold time).");
                    ui.end_row();
                }
            });
            self.io_cat_sort = sort;
            ui.separator();

            // ── Recent operations ────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.strong("Recent operations");
                ui.separator();
                egui::ComboBox::from_id_salt("io_ops_cat")
                    .selected_text(self.io_ops_cat.map(|c| c.label()).unwrap_or("all categories"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.io_ops_cat, None, "all categories");
                        for c in Cat::ALL {
                            ui.selectable_value(&mut self.io_ops_cat, Some(c), c.label());
                        }
                    });
                egui::ComboBox::from_id_salt("io_ops_region")
                    .selected_text(self.io_ops_region.map(|r| r.label()).unwrap_or("all regions"))
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.io_ops_region, None, "all regions");
                        for r in Region::ALL {
                            ui.selectable_value(&mut self.io_ops_region, Some(r), r.label());
                        }
                    });
                ui.weak("(newest first; slow ops highlighted; database ops are counters-only)");
            });
            let ops_cat = self.io_ops_cat;
            let ops_region = self.io_ops_region;
            egui::ScrollArea::vertical()
                .id_salt("io_ops_scroll")
                .max_height(300.0)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    for r in snap.ring.iter().rev() {
                        if ops_cat.is_some_and(|c| c != r.cat) {
                            continue;
                        }
                        if ops_region.is_some_and(|reg| reg != r.region) {
                            continue;
                        }
                        let time = chrono::DateTime::from_timestamp_millis(r.at_ms)
                            .map(|t| t.with_timezone(&chrono::Local).format("%H:%M:%S%.3f").to_string())
                            .unwrap_or_default();
                        let ms = r.dur_us as f64 / 1000.0;
                        let line = format!(
                            "{time}  {:<18} {:<6} {:<16} {:>10}  {:>8.1}ms  [{}]  {}",
                            r.cat.label(),
                            r.kind.label(),
                            r.region.label(),
                            fmt_bytes(r.bytes as i64),
                            ms,
                            r.thread,
                            r.path
                        );
                        let slow = ms >= crate::iomon::SLOW_OP_MS as f64;
                        let text = egui::RichText::new(line).monospace().size(11.0);
                        if slow {
                            ui.colored_label(ui.visuals().warn_fg_color, text);
                        } else {
                            ui.label(text);
                        }
                    }
                });
            ui.add_space(16.0);
        });
    }
}
