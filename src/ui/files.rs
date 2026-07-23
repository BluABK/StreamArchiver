//! Files view: drive/instance/location mapping scan and batch edits.

use super::*;

/// Off-thread scan behind the Files tab: where everything is mapped, what's
/// actually on disk, and how full each drive is. Recomputed on tab open /
/// ⟳ / after an edit — never on the render path (dir stats + drive queries
/// can spin up a sleeping USB drive).
pub(super) struct FilesScan {
    /// Per drive letter: (letter, online free/total, DB recordings bytes, count).
    pub(super) drives: Vec<(char, Option<(u64, u64)>, i64, i64)>,
    /// Per instance: monitor id, channel, platform, tool label, output dir,
    /// recording count, total DB bytes, resolved current cache dir.
    pub(super) instances: Vec<FilesInstanceRow>,
    /// Per distinct recording location: dir, on-disk?, count, DB bytes, and
    /// the instances currently mapped there (empty = history only).
    pub(super) dirs: Vec<FilesDirRow>,
}

pub(super) struct FilesInstanceRow {
    pub(super) monitor_id: i64,
    pub(super) channel: String,
    pub(super) platform: crate::models::Platform,
    pub(super) tool: String,
    pub(super) output_dir: String,
    pub(super) rec_count: i64,
    pub(super) rec_bytes: i64,
    pub(super) cache_dir: String,
}

pub(super) struct FilesDirRow {
    pub(super) dir: String,
    pub(super) exists: bool,
    pub(super) rec_count: i64,
    pub(super) rec_bytes: i64,
    pub(super) mapped: Vec<String>,
}

/// The drive letter a Windows path starts with (`"A:\foo"` → `Some('A')`),
/// or `None` for anything else (relative path, UNC, no drive prefix). Used
/// to gate the drive-letter swap in "Redirect all instances on drive" —
/// deliberately stricter than a bare first-char check (requires the `:`)
/// since a match here gets `dir[1..]` sliced to build a new path.
fn drive_letter_of(path: &str) -> Option<char> {
    let mut chars = path.chars();
    let letter = chars.next()?;
    (letter.is_ascii_alphabetic() && chars.next() == Some(':')).then(|| letter.to_ascii_uppercase())
}

/// Parse a user-typed drive letter, tolerating a trailing colon and
/// surrounding whitespace (`"g"`, `"G:"`, `" G "` all parse to `Some('G')`).
fn parse_drive_letter(s: &str) -> Option<char> {
    s.trim()
        .trim_end_matches(':')
        .chars()
        .next()
        .filter(|c| c.is_ascii_alphabetic())
        .map(|c| c.to_ascii_uppercase())
}

impl StreamArchiverApp {
    /// Kick off the off-thread Files scan (dir existence + drive space can
    /// spin up a sleeping USB drive — never on the render path).
    pub(super) fn spawn_files_scan(&mut self) {
        let core = self.core.clone();
        let (tx, rx) = std::sync::mpsc::channel();
        self.files_scan_rx = Some(rx);
        std::thread::Builder::new()
            .name("files-scan".into())
            .spawn(move || {
                let mons = core.store.list_monitors_with_channels().unwrap_or_default();
                let stats: std::collections::HashMap<i64, (i64, i64)> = core
                    .store
                    .recording_stats_by_monitor()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|(id, n, b)| (id, (n, b)))
                    .collect();
                let instances: Vec<FilesInstanceRow> = mons
                    .iter()
                    .map(|m| {
                        let (n, b) = stats.get(&m.monitor.id).copied().unwrap_or((0, 0));
                        FilesInstanceRow {
                            monitor_id: m.monitor.id,
                            channel: m.channel.name.clone(),
                            platform: m.monitor.platform(),
                            tool: m.monitor.tool.label().to_string(),
                            output_dir: m.monitor.output_dir.clone(),
                            rec_count: n,
                            rec_bytes: b,
                            cache_dir: crate::downloader::cache_dir_candidates(
                                std::path::Path::new(&m.monitor.output_dir),
                            )
                            .first()
                            .map(|p| p.to_string_lossy().into_owned())
                            .unwrap_or_default(),
                        }
                    })
                    .collect();
                // Distinct recording locations (cache paths mapped to their
                // promoted parent), with per-dir count/size from the DB.
                let mut per_dir: std::collections::HashMap<String, (i64, i64)> =
                    std::collections::HashMap::new();
                for (p, bytes) in core.store.recording_paths_with_bytes().unwrap_or_default() {
                    let path = std::path::PathBuf::from(&p);
                    let promoted =
                        crate::downloader::strip_cache_component(&path).unwrap_or(path);
                    let Some(dir) = promoted.parent() else { continue };
                    let e = per_dir
                        .entry(dir.to_string_lossy().into_owned())
                        .or_insert((0, 0));
                    e.0 += 1;
                    e.1 += bytes;
                }
                let mut dirs: Vec<FilesDirRow> = per_dir
                    .into_iter()
                    .map(|(dir, (n, b))| {
                        let mapped: Vec<String> = mons
                            .iter()
                            .filter(|m| m.monitor.output_dir.trim_end_matches(['\\', '/'])
                                == dir.trim_end_matches(['\\', '/']))
                            .map(|m| format!("{} ({})", m.channel.name, m.monitor.tool.label()))
                            .collect();
                        FilesDirRow {
                            exists: crate::iomon::fs::is_dir_sync(
                                crate::iomon::Cat::FsProbe,
                                std::path::Path::new(&dir),
                            ),
                            dir,
                            rec_count: n,
                            rec_bytes: b,
                            mapped,
                        }
                    })
                    .collect();
                dirs.sort_by(|a, b| a.dir.cmp(&b.dir));
                // Per-drive rollup: DB size/count of everything on that letter
                // + live free/total (None = offline/unmapped).
                let mut per_drive: std::collections::HashMap<char, (i64, i64)> =
                    std::collections::HashMap::new();
                for d in &dirs {
                    let letter = d
                        .dir
                        .chars()
                        .next()
                        .filter(|c| c.is_ascii_alphabetic())
                        .map(|c| c.to_ascii_uppercase());
                    if let Some(l) = letter {
                        let e = per_drive.entry(l).or_insert((0, 0));
                        e.0 += d.rec_bytes;
                        e.1 += d.rec_count;
                    }
                }
                // Instance drives with no recordings yet still show up.
                for m in &mons {
                    if let Some(l) = m
                        .monitor
                        .output_dir
                        .chars()
                        .next()
                        .filter(|c| c.is_ascii_alphabetic())
                    {
                        per_drive.entry(l.to_ascii_uppercase()).or_insert((0, 0));
                    }
                }
                let mut drives: Vec<(char, Option<(u64, u64)>, i64, i64)> = per_drive
                    .into_iter()
                    .map(|(l, (b, n))| (l, crate::platform::disk_space(l), b, n))
                    .collect();
                drives.sort_by_key(|d| d.0);
                let _ = tx.send(FilesScan { drives, instances, dirs });
            })
            .ok();
    }

    /// Files view: what is mapped to which path (instances → output folders,
    /// recordings → the dirs they actually sit in, per-drive totals), with
    /// inline + batch editing and a DB path-relocation tool for drive moves.
    pub(super) fn files_view(&mut self, ui: &mut egui::Ui) {
        // Receive a finished scan / kick one off.
        if let Some(rx) = &self.files_scan_rx {
            if let Ok(scan) = rx.try_recv() {
                self.files_scan = Some(scan);
                self.files_scan_rx = None;
            }
        } else if self.files_scan.is_none() {
            self.spawn_files_scan();
        }

        ui.horizontal(|ui| {
            ui.heading("File management");
            if ui
                .button("⟳ Rescan")
                .on_hover_text(
                    "Refresh drive free/total space, per-folder recording counts/sizes, \
                     and each instance's resolved cache dir. This tab doesn't stat drives \
                     on every frame (it can wake a sleeping USB drive), so changes made \
                     outside the app — freed disk space, a drive coming back online — \
                     aren't picked up until you rescan or make an edit here.",
                )
                .clicked()
            {
                self.files_scan = None;
                self.files_scan_rx = None;
            }
            if !self.files_status.is_empty() {
                ui.separator();
                ui.weak(&self.files_status);
            }
        });
        let Some(scan_ptr) = self.files_scan.as_ref() else {
            ui.spinner();
            ui.weak("Scanning drives and recording locations…");
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(200));
            return;
        };
        // Borrow-friendly copies for the closures below.
        let drives = scan_ptr.drives.clone();
        let instances: Vec<_> = scan_ptr
            .instances
            .iter()
            .map(|r| {
                (r.monitor_id, r.channel.clone(), r.platform, r.tool.clone(),
                 r.output_dir.clone(), r.rec_count, r.rec_bytes, r.cache_dir.clone())
            })
            .collect();
        let dir_rows: Vec<_> = scan_ptr
            .dirs
            .iter()
            .map(|d| (d.dir.clone(), d.exists, d.rec_count, d.rec_bytes, d.mapped.clone()))
            .collect();

        let mut rescan = false;
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            // ── Drives ────────────────────────────────────────────────────
            ui.add_space(4.0);
            ui.strong("Drives");
            egui::Grid::new("files_drives").striped(true).min_col_width(70.0).show(ui, |ui| {
                ui.weak("drive");
                ui.weak("status");
                ui.weak("recordings (DB)");
                ui.weak("free / total");
                ui.end_row();
                for (letter, space, bytes, count) in &drives {
                    ui.label(format!("{letter}:"));
                    match space {
                        Some(_) => { ui.label("online"); }
                        None => { ui.colored_label(ui.visuals().error_fg_color, "offline"); }
                    }
                    ui.label(format!("{} in {} recording(s)", fmt_bytes(*bytes), count));
                    match space {
                        Some((free, total)) => {
                            let resp = ui.label(format!(
                                "{} / {}",
                                fmt_bytes(*free as i64),
                                fmt_bytes(*total as i64)
                            ));
                            if *free < total / 20 {
                                resp.on_hover_text(
                                    "Under 5% free — consider retargeting instances to \
                                     another drive (the old recordings stay where they \
                                     are and remain tracked).",
                                );
                            }
                        }
                        None => { ui.label("-"); }
                    }
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.separator();

            // ── Instances → output folders ────────────────────────────────
            ui.strong("Instances");
            ui.weak(
                "Where each instance records TO from now on. Editing a folder only \
                 affects future takes — existing recordings keep their location and \
                 stay tracked (playback, Issues, I/O monitor).",
            );
            let mut set_dir: Vec<(i64, String)> = Vec::new();
            egui::Grid::new("files_instances").striped(true).min_col_width(60.0).show(ui, |ui| {
                ui.weak("");
                ui.weak("channel");
                ui.weak("platform");
                ui.weak("tool");
                ui.weak("output folder (future takes)");
                ui.weak("recordings");
                ui.weak("size (DB)");
                ui.weak("cache dir (resolved)");
                ui.end_row();
                for (mid, channel, platform, tool, output_dir, n, b, cache) in &instances {
                    let mut sel = self.files_selected.contains(mid);
                    if ui.checkbox(&mut sel, "").changed() {
                        if sel {
                            self.files_selected.insert(*mid);
                        } else {
                            self.files_selected.remove(mid);
                        }
                    }
                    ui.label(channel);
                    ui.label(platform.label());
                    ui.label(tool);
                    ui.horizontal(|ui| {
                        let draft = self
                            .files_edit
                            .entry(*mid)
                            .or_insert_with(|| output_dir.clone());
                        ui.add(egui::TextEdit::singleline(draft).desired_width(280.0));
                        let changed = draft.trim() != output_dir.trim();
                        if changed {
                            let draft_now = draft.trim().to_string();
                            if ui.button("💾").on_hover_text("Apply new output folder").clicked()
                                && !draft_now.is_empty()
                            {
                                set_dir.push((*mid, draft_now));
                            }
                            if ui.button("↶").on_hover_text("Revert").clicked() {
                                self.files_edit.insert(*mid, output_dir.clone());
                            }
                        }
                    });
                    ui.label(n.to_string());
                    ui.label(fmt_bytes(*b));
                    ui.label(cache).on_hover_text(
                        "Where this instance's IN-PROGRESS captures go under the current \
                         cache layout (Settings → Recording → Capture cache location).",
                    );
                    ui.end_row();
                }
            });
            // Batch bar.
            ui.horizontal(|ui| {
                let n_sel = self.files_selected.len();
                ui.weak(format!("{n_sel} selected"));
                if n_sel > 0 && ui.button("Clear selection").clicked() {
                    self.files_selected.clear();
                }
                ui.separator();
                ui.label("Set folder for selected:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.files_batch_dir)
                        .hint_text(r"D:\streams\{channel} — literal path; applied as-is")
                        .desired_width(280.0),
                );
                let batch_dir = self.files_batch_dir.trim().to_string();
                if ui
                    .add_enabled(n_sel > 0 && !batch_dir.is_empty(), egui::Button::new("Apply"))
                    .on_hover_text(
                        "Sets this output folder on every selected instance. {channel} \
                         expands to each instance's channel name.",
                    )
                    .clicked()
                {
                    for (mid, channel, ..) in instances.iter().filter(|r| self.files_selected.contains(&r.0)) {
                        let dir = batch_dir
                            .replace("{channel}", &crate::downloader::sanitize_filename(channel));
                        set_dir.push((*mid, dir));
                    }
                }
            });
            // Redirect-by-drive bar: bulk version of the row/selection edits
            // above, matched by current drive letter instead — the "this
            // drive is full, send future takes to a different one" case. Only
            // the drive letter changes; the rest of each instance's path (and
            // every existing recording) is untouched.
            ui.horizontal(|ui| {
                ui.label("Redirect all instances on drive:");
                egui::ComboBox::from_id_salt("files_redirect_from")
                    .selected_text(if self.files_redirect_from.is_empty() {
                        "— pick —".to_string()
                    } else {
                        format!("{}:", self.files_redirect_from)
                    })
                    .show_ui(ui, |ui| {
                        for (letter, ..) in &drives {
                            ui.selectable_value(
                                &mut self.files_redirect_from,
                                letter.to_string(),
                                format!("{letter}:"),
                            );
                        }
                    });
                ui.label("→");
                ui.add(
                    egui::TextEdit::singleline(&mut self.files_redirect_to)
                        .hint_text("G")
                        .desired_width(30.0),
                );
                let from_letter = parse_drive_letter(&self.files_redirect_from);
                let to_letter = parse_drive_letter(&self.files_redirect_to);
                let matched: Vec<(i64, String)> = match from_letter {
                    Some(fl) => instances
                        .iter()
                        .filter(|r| drive_letter_of(&r.4) == Some(fl))
                        .map(|r| (r.0, r.4.clone()))
                        .collect(),
                    None => Vec::new(),
                };
                if let Some(fl) = from_letter {
                    ui.weak(format!("{} instance(s) on {fl}:", matched.len()));
                }
                if ui
                    .add_enabled(
                        from_letter.is_some()
                            && to_letter.is_some()
                            && from_letter != to_letter
                            && !matched.is_empty(),
                        egui::Button::new("Redirect"),
                    )
                    .on_hover_text(
                        "Sets a new output folder (same sub-path, new drive letter) on \
                         every instance currently on the 'from' drive. Only affects \
                         future takes — existing recordings keep their location and \
                         stay tracked.",
                    )
                    .clicked()
                    && let Some(tl) = to_letter
                {
                    for (mid, dir) in &matched {
                        set_dir.push((*mid, format!("{tl}{}", &dir[1..])));
                    }
                }
            });
            if !set_dir.is_empty() {
                let mut last_err: Option<String> = None;
                let mut updated = 0usize;
                for (mid, dir) in &set_dir {
                    if let Err(e) = self.core.store.set_monitor_output_dir(*mid, dir) {
                        last_err = Some(format!("{e:#}"));
                    } else {
                        self.files_edit.insert(*mid, dir.clone());
                        updated += 1;
                    }
                }
                self.files_status = match last_err {
                    None => format!("Updated output folder on {updated} instance(s)."),
                    Some(e) => format!(
                        "Updated {updated} of {} instance(s) — last error: {e}",
                        set_dir.len()
                    ),
                };
                refresh_iomon_roots(&self.core.store, &self.settings.default_output_dir);
                rescan = true;
            }
            ui.add_space(8.0);
            ui.separator();

            // ── Recording locations ───────────────────────────────────────
            ui.strong("Recording locations");
            ui.weak(
                "Every folder recordings actually sit in per the database — including \
                 folders no instance points at anymore (e.g. a previous drive).",
            );
            egui::Grid::new("files_dirs").striped(true).min_col_width(60.0).show(ui, |ui| {
                ui.weak("folder");
                ui.weak("on disk");
                ui.weak("recordings");
                ui.weak("size (DB)");
                ui.weak("mapped instances");
                ui.end_row();
                for (dir, exists, n, b, mapped) in &dir_rows {
                    ui.label(dir);
                    if *exists {
                        ui.label("✔");
                    } else {
                        ui.colored_label(ui.visuals().error_fg_color, "missing")
                            .on_hover_text(
                                "The folder isn't reachable right now — drive offline, or \
                                 the files were moved. If you moved them, use \
                                 'Relocate recorded paths' below to update the database.",
                            );
                    }
                    ui.label(n.to_string());
                    ui.label(fmt_bytes(*b));
                    if mapped.is_empty() {
                        ui.weak("— (history)");
                    } else {
                        ui.label(mapped.join(", "));
                    }
                    ui.end_row();
                }
            });
            ui.add_space(8.0);
            ui.separator();

            // ── Relocate recorded paths (DB remap after a physical move) ──
            ui.strong("Relocate recorded paths");
            ui.weak(
                "For after YOU move files (drive swap, folder rename): rewrites the \
                 leading path prefix in the database — recordings (incl. head/full/\
                 recovered/VOD companions) and video downloads. No files are touched.",
            );
            ui.horizontal(|ui| {
                ui.label("From prefix:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.files_reloc_from)
                        .hint_text(r"A:\streams")
                        .desired_width(220.0),
                );
                ui.label("→ To prefix:");
                ui.add(
                    egui::TextEdit::singleline(&mut self.files_reloc_to)
                        .hint_text(r"D:\streams")
                        .desired_width(220.0),
                );
                ui.checkbox(&mut self.files_reloc_monitors, "also retarget instance folders");
            });
            let from = self.files_reloc_from.trim().to_string();
            let to = self.files_reloc_to.trim().to_string();
            ui.horizontal(|ui| {
                if ui
                    .add_enabled(!from.is_empty(), egui::Button::new("Preview"))
                    .clicked()
                {
                    match self.core.store.count_path_prefix_matches(&from) {
                        Ok((r, v, m)) => self.files_reloc_preview = Some((from.clone(), r, v, m)),
                        Err(e) => self.files_status = format!("Preview failed: {e:#}"),
                    }
                }
                let previewed = self
                    .files_reloc_preview
                    .as_ref()
                    .is_some_and(|(f, ..)| *f == from);
                if let Some((_, r, v, m)) = self
                    .files_reloc_preview
                    .as_ref()
                    .filter(|(f, ..)| *f == from)
                {
                    ui.weak(format!(
                        "matches {r} recording(s), {v} video(s){}",
                        if self.files_reloc_monitors {
                            format!(", {m} instance folder(s)")
                        } else {
                            String::new()
                        }
                    ));
                }
                if ui
                    .add_enabled(
                        previewed && !to.is_empty() && from != to,
                        egui::Button::new("Apply relocation"),
                    )
                    .on_hover_text("Rewrites the matched paths in the database.")
                    .clicked()
                {
                    match self
                        .core
                        .store
                        .replace_path_prefix(&from, &to, self.files_reloc_monitors)
                    {
                        Ok((r, v, m)) => {
                            self.files_status = format!(
                                "Relocated: {r} recording path(s), {v} video path(s), \
                                 {m} instance folder(s)."
                            );
                            self.files_reloc_preview = None;
                            refresh_iomon_roots(
                                &self.core.store,
                                &self.settings.default_output_dir,
                            );
                            rescan = true;
                        }
                        Err(e) => self.files_status = format!("Relocation failed: {e:#}"),
                    }
                }
            });
            ui.add_space(8.0);
        });
        if rescan {
            self.files_scan = None;
            self.files_scan_rx = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drive_letter_of_requires_letter_colon_prefix() {
        assert_eq!(drive_letter_of(r"A:\streams\channel"), Some('A'));
        assert_eq!(drive_letter_of(r"g:\streams"), Some('G'));
        assert_eq!(drive_letter_of(r"\\server\share\streams"), None);
        assert_eq!(drive_letter_of("streams"), None);
        assert_eq!(drive_letter_of(""), None);
    }

    #[test]
    fn parse_drive_letter_tolerates_colon_and_whitespace() {
        assert_eq!(parse_drive_letter("g"), Some('G'));
        assert_eq!(parse_drive_letter("G:"), Some('G'));
        assert_eq!(parse_drive_letter(" G "), Some('G'));
        assert_eq!(parse_drive_letter(""), None);
        assert_eq!(parse_drive_letter("1"), None);
    }
}
