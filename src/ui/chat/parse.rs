//! Reading a chat sidecar: chunked parsing, tail loading, and the
//! per-platform line parsers (Twitch IRC JSON, YouTube live_chat).

use super::*;

/// How much of the file's tail the phase-1 (instant) parse covers. Enough for
/// hundreds of Twitch lines / dozens of (much fatter) YouTube lines.
pub(in crate::ui) const CHAT_TAIL_BYTES: u64 = 512 * 1024;

/// Parse the byte range `[from, to)` of a chat file (`to == None` reads to the
/// current EOF). Only complete (newline-terminated) lines are parsed; a
/// trailing partial line — the logger may be mid-write — is left for the next
/// pass via `parsed_to`, so incremental tail reads never split a message. Both
/// formats (Twitch `.chat.jsonl`, YouTube `.live_chat.json`) are line-delimited
/// JSON, so byte-offset resumption is exact.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn parse_chat_chunk(
    path: &Path,
    from: u64,
    to: Option<u64>,
    start_unix_secs: i64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    source_partners: &HashMap<String, crate::models::CollabPartner>,
    badge_dirs: &TwitchBadgeDirs,
) -> anyhow::Result<ChatChunk> {
    use std::io::{Read, Seek, SeekFrom};
    // Read window: bounds peak memory on huge logs — the previous whole-range
    // slurp held roughly 2x the file size in RAM for a marathon stream's
    // phase-2 parse.
    const WINDOW: usize = 8 * 1024 * 1024;
    let chat_region = crate::iomon::classify(path);
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    let end = to.unwrap_or(len).min(len);
    if from >= end {
        return Ok(ChatChunk {
            messages: Vec::new(),
            fetches: Vec::new(),
            parsed_to: from,
            markers: Vec::new(),
        });
    }
    f.seek(SeekFrom::Start(from))?;
    let is_yt = path.to_string_lossy().ends_with("live_chat.json");
    let start_ms = start_unix_secs as f64 * 1000.0;
    let mut messages = Vec::new();
    let mut fetches: Vec<EmojiFetch> = Vec::new();
    let mut markers: Vec<MarkerAt> = Vec::new();
    // YouTube only: where in the stream the last message landed, so an
    // untimestamped moderation action can be stamped at its place in the file
    // (see `parse_yt_chat_line`). A chunk that starts mid-file begins at 0,
    // which is correct for a purge — it strikes everything before it anyway.
    let mut yt_last_ts = 0.0_f64;
    let mut parsed_to = from;
    let mut pos = from;
    // Carries a partial line across window boundaries.
    let mut buf: Vec<u8> = Vec::new();
    while pos < end {
        let take = WINDOW.min((end - pos) as usize);
        let old_len = buf.len();
        buf.resize(old_len + take, 0);
        let read_start = std::time::Instant::now();
        let read_res = f.read_exact(&mut buf[old_len..]);
        crate::iomon::record_region(
            crate::iomon::Cat::ChatSidecar,
            chat_region,
            crate::iomon::OpKind::Read,
            take as u64,
            read_start.elapsed(),
            true,
        );
        read_res?;
        pos += take as u64;
        let complete = match buf.iter().rposition(|&b| b == b'\n') {
            Some(i) => i + 1,
            // No boundary yet — a single line larger than the window; grow
            // the buffer with the next window.
            None if pos < end => continue,
            // A bounded chunk ends on a known line boundary; an unbounded
            // tail can end mid-line while the logger is writing.
            None if to.is_some() => buf.len(),
            None => 0,
        };
        {
            let text = String::from_utf8_lossy(&buf[..complete]);
            for line in text.lines() {
                if line.is_empty() {
                    continue;
                }
                if is_yt {
                    parse_yt_chat_line(
                        line,
                        &mut messages,
                        &mut markers,
                        &mut fetches,
                        &mut yt_last_ts,
                    );
                } else if line.contains("\"marker\":") {
                    // Moderation marker / notice line (cheap substring gate —
                    // markers are rare next to messages).
                    if let Some((marker, notice)) = parse_twitch_marker_line(line, start_ms) {
                        if let Some(m) = marker {
                            markers.push(m);
                        }
                        if let Some(n) = notice {
                            messages.push(n);
                        }
                    }
                } else if let Some(m) = parse_twitch_chat_line(
                    line,
                    start_ms,
                    emote_map,
                    twitch_dir,
                    twitch_fallback_index,
                    fetch_unknown_emotes,
                    &mut fetches,
                    source_partners,
                    badge_dirs,
                ) {
                    messages.push(m);
                }
            }
        }
        buf.drain(..complete);
        parsed_to = pos - buf.len() as u64;
    }
    // De-duplicate so the same emoji isn't downloaded once per occurrence.
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    Ok(ChatChunk { messages, fetches, parsed_to, markers })
}

/// The first line boundary within the file's last [`CHAT_TAIL_BYTES`] — where
/// the phase-1 tail parse starts. 0 for small files (just parse everything).
pub(in crate::ui) fn chat_tail_start(path: &Path) -> anyhow::Result<u64> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let len = f.metadata()?.len();
    if len <= CHAT_TAIL_BYTES {
        return Ok(0);
    }
    f.seek(SeekFrom::Start(len - CHAT_TAIL_BYTES))?;
    let mut buf = vec![0u8; CHAT_TAIL_BYTES as usize];
    let read_start = std::time::Instant::now();
    let read_res = f.read_exact(&mut buf);
    crate::iomon::record(
        crate::iomon::Cat::ChatSidecar,
        path,
        crate::iomon::OpKind::Read,
        CHAT_TAIL_BYTES,
        read_start.elapsed(),
    );
    read_res?;
    Ok(match buf.iter().position(|&b| b == b'\n') {
        Some(i) => len - CHAT_TAIL_BYTES + i as u64 + 1,
        None => 0, // no boundary in the tail (one giant line) — parse it all
    })
}

/// Parse a chunk of a chat file off the UI thread, mapping both join and parse
/// errors to a string for [`ChatLoadState::Error`]. `pub(crate)`: also used
/// by `downloader::supervisor::cmd_fetch_missing_chat_emotes` to parse a
/// whole log (`from: 0, to: None`) during the maintenance sweep.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn parse_chunk_blocking(
    path: std::path::PathBuf,
    from: u64,
    to: Option<u64>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
) -> Result<ChatChunk, String> {
    tokio::task::spawn_blocking(move || {
        parse_chat_chunk(
            &path,
            from,
            to,
            start_ts,
            &emote_map,
            twitch_dir.as_deref(),
            &twitch_fallback_index,
            fetch_unknown_emotes,
            &source_partners,
            &badge_dirs,
        )
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())
}

/// Promote pending emote images that have landed on disk since the parse.
/// The existence checks run OUTSIDE the chat mutex and off the async threads
/// (`spawn_blocking`) — stat-ing thousands of segments while holding the lock
/// the renderer takes every frame froze the whole app.
pub(in crate::ui) async fn upgrade_pending_emotes(state: &Arc<Mutex<ChatLoadState>>) {
    let pending: Vec<std::path::PathBuf> = {
        let st = state.lock().unwrap();
        let ChatLoadState::Loaded(log) = &*st else { return };
        let mut set: HashSet<std::path::PathBuf> = HashSet::new();
        for m in &log.messages {
            for seg in &m.segments {
                if let ChatSegment::Emote { file: None, pending: Some(p), .. } = seg {
                    set.insert(p.clone());
                }
            }
        }
        set.into_iter().collect()
    };
    if pending.is_empty() {
        return;
    }
    let on_disk: HashSet<std::path::PathBuf> = tokio::task::spawn_blocking(move || {
        pending.into_iter().filter(|p| crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, p)).collect()
    })
    .await
    .unwrap_or_default();
    if on_disk.is_empty() {
        return;
    }
    if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
        for m in &mut log.messages {
            for seg in &mut m.segments {
                if let ChatSegment::Emote { file, pending, .. } = seg
                    && file.is_none()
                    && pending.as_ref().is_some_and(|p| on_disk.contains(p))
                {
                    *file = pending.take();
                }
            }
        }
    }
}

/// Download missing emoji/emoji-emote images (sequential, best-effort; 404s for a
/// liberally-detected non-emoji just leave the glyph). Capped so a pathological
/// message can't trigger thousands of requests.
///
/// `pace`: delay after each successful download, `None` for the interactive
/// per-popup path (`load_chat`/`tail_chat` — a user is waiting, and a normal
/// chat log only ever queues a handful of emoji). The "Fetch missing chat
/// emotes" maintenance sweep (`downloader::supervisor::cmd_fetch_missing_chat_emotes`)
/// passes `Some(150ms)` — matching every other bulk emote fetcher in
/// `assets.rs` (BTTV/FFZ/7TV/Twitch channel emotes all pace themselves the
/// same way) — since a sweep across months of chat logs can turn up
/// hundreds of distinct missing ids in one run, all hitting Twitch's CDN
/// back to back with no delay otherwise.
pub(crate) async fn download_emoji_images(fetches: &[EmojiFetch], pace: Option<std::time::Duration>) {
    let Ok(client) = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .build()
    else {
        return;
    };
    for f in fetches.iter().take(300) {
        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &f.dest) {
            continue;
        }
        let mut got = false;
        let mut network_error = false; // a transient failure → don't negative-cache
        for url in &f.urls {
            match client.get(url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(bytes) = resp.bytes().await {
                        if let Some(parent) = f.dest.parent() {
                            let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
                        }
                        if crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &f.dest, &bytes).await.is_ok() {
                            got = true;
                            break;
                        }
                    }
                }
                // 404 = this candidate name doesn't exist → try the next candidate.
                Ok(resp) if resp.status() == reqwest::StatusCode::NOT_FOUND => {}
                // Any other HTTP status or a transport error is transient-ish.
                Ok(_) | Err(_) => network_error = true,
            }
        }
        if got && let Some(d) = pace {
            tokio::time::sleep(d).await;
        }
        // Negative-cache ONLY a definitive miss (every candidate 404'd, no network
        // error), so a transient offline failure can't permanently block a real
        // emoji. `dest` is `{key}.png` → marker `{key}.404`.
        if !got && !network_error {
            let marker = f.dest.with_extension("404");
            if let Some(parent) = marker.parent() {
                let _ = crate::iomon::fs::create_dir_all(crate::iomon::Cat::AssetCache, parent).await;
            }
            let _ = crate::iomon::fs::write(crate::iomon::Cat::AssetCache, &marker, b"").await;
        }
    }
}

/// Load a chat file into `state`, tail-first: the newest [`CHAT_TAIL_BYTES`]
/// parse and display immediately, then the rest of the file parses in the
/// background and is spliced in front (`loading_older` marks the gap). Then —
/// when `fetch_emoji` — missing emoji images download once and upgrade the
/// in-memory segments in place. Runs entirely off the UI thread.
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) async fn load_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: Option<std::path::PathBuf>,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    fetch_emoji: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
    ctx: egui::Context,
) {
    let Some(path) = path else {
        *state.lock().unwrap() = ChatLoadState::NoFile;
        ctx.request_repaint();
        return;
    };
    // Phase 1: the file's tail — the newest messages show instantly instead of
    // waiting for a full-file parse.
    let head_end = {
        let p = path.clone();
        match tokio::task::spawn_blocking(move || chat_tail_start(&p)).await {
            Ok(Ok(off)) => off,
            Ok(Err(e)) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
            Err(e) => {
                *state.lock().unwrap() = ChatLoadState::Error(e.to_string());
                ctx.request_repaint();
                return;
            }
        }
    };
    let mut fetches = match parse_chunk_blocking(
        path.clone(),
        head_end,
        None,
        start_ts,
        emote_map.clone(),
        twitch_dir.clone(),
        twitch_fallback_index.clone(),
        fetch_unknown_emotes,
        source_partners.clone(),
        badge_dirs.clone(),
    )
    .await
    {
        Ok(chunk) => {
            let mut log = ChatLog {
                messages: chunk.messages,
                row_heights: Vec::new(),
                measured_key: (0.0, 0),
                parsed_to: chunk.parsed_to,
                loading_older: head_end > 0,
                markers: chunk.markers,
            };
            log.apply_markers();
            *state.lock().unwrap() = ChatLoadState::Loaded(log);
            ctx.request_repaint();
            chunk.fetches
        }
        Err(e) => {
            *state.lock().unwrap() = ChatLoadState::Error(e);
            ctx.request_repaint();
            return;
        }
    };
    // Phase 2: everything before the tail, spliced in front when ready.
    if head_end > 0 {
        match parse_chunk_blocking(
            path.clone(),
            0,
            Some(head_end),
            start_ts,
            emote_map.clone(),
            twitch_dir.clone(),
            twitch_fallback_index.clone(),
            fetch_unknown_emotes,
            source_partners.clone(),
            badge_dirs.clone(),
        )
        .await
        {
            Ok(older) => {
                fetches.extend(older.fetches);
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    // Heights are index-parallel to messages: prepend matching
                    // estimates, or every measured tail height would be
                    // re-attributed to the oldest rows and the virtualized
                    // offsets/scrollbar would scramble.
                    if !log.row_heights.is_empty() {
                        let n = older.messages.len();
                        log.row_heights
                            .splice(0..0, std::iter::repeat_n(CHAT_ROW_EST, n));
                    }
                    log.messages.splice(0..0, older.messages);
                    log.loading_older = false;
                    // Tail markers may target just-prepended older messages
                    // (e.g. a ban purging someone's whole history).
                    log.markers.extend(older.markers);
                    log.apply_markers();
                }
            }
            Err(_) => {
                if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
                    log.loading_older = false;
                }
            }
        }
        ctx.request_repaint();
    }
    // Phase 3: emoji downloads + in-place upgrade. Only one download pass runs
    // at a time (a concurrent tail reload just skips kicking off another).
    fetches.sort_by(|a, b| a.dest.cmp(&b.dest));
    fetches.dedup();
    if fetch_emoji && !fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&fetches, None).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Incremental tail reload for a live recording: parse only the bytes appended
/// since the last pass and push them onto the existing log. (The previous
/// implementation re-parsed the entire file every few seconds.)
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) async fn tail_chat(
    state: Arc<Mutex<ChatLoadState>>,
    loading: Arc<AtomicBool>,
    path: std::path::PathBuf,
    start_ts: i64,
    emote_map: Arc<HashMap<String, std::path::PathBuf>>,
    twitch_dir: Option<std::path::PathBuf>,
    twitch_fallback_index: Arc<HashMap<String, std::path::PathBuf>>,
    fetch_unknown_emotes: bool,
    fetch_emoji: bool,
    source_partners: Arc<HashMap<String, crate::models::CollabPartner>>,
    badge_dirs: Arc<TwitchBadgeDirs>,
    ctx: egui::Context,
) {
    let from = {
        match &*state.lock().unwrap() {
            ChatLoadState::Loaded(log) => Some(log.parsed_to),
            // Initial load still in flight — let it finish first.
            ChatLoadState::Loading => return,
            // The sidecar may have appeared since (opened seconds after the
            // recording started) or a transient read error cleared — retry the
            // full tail-first load instead of staying broken forever.
            ChatLoadState::NoFile | ChatLoadState::Error(_) => None,
        }
    };
    let Some(from) = from else {
        load_chat(
            state,
            loading,
            Some(path),
            start_ts,
            emote_map,
            twitch_dir,
            twitch_fallback_index,
            fetch_unknown_emotes,
            fetch_emoji,
            source_partners,
            badge_dirs,
            ctx,
        )
        .await;
        return;
    };
    let Ok(chunk) = parse_chunk_blocking(
        path,
        from,
        None,
        start_ts,
        emote_map,
        twitch_dir,
        twitch_fallback_index,
        fetch_unknown_emotes,
        source_partners,
        badge_dirs,
    )
    .await
    else {
        return;
    };
    // Always advance past parsed complete lines — a chunk of non-message lines
    // (tickers, moderation events) must not freeze the resume offset, or every
    // 3s pass re-reads an ever-growing suffix.
    if chunk.parsed_to > from || !chunk.messages.is_empty() {
        if let ChatLoadState::Loaded(log) = &mut *state.lock().unwrap() {
            // Overlapping reloads on a slow read could double-append; only the
            // pass that still matches the resume offset lands.
            if log.parsed_to == from {
                log.messages.extend(chunk.messages);
                log.parsed_to = chunk.parsed_to;
                // A live deletion/purge marker targets messages already shown
                // — re-apply so they strike through on the next frame. (No new
                // markers = nothing to do: appended messages postdate every
                // stored marker.)
                if !chunk.markers.is_empty() {
                    log.markers.extend(chunk.markers);
                    log.apply_markers();
                }
            }
        }
        ctx.request_repaint();
    }
    if fetch_emoji && !chunk.fetches.is_empty() && !loading.swap(true, Ordering::SeqCst) {
        download_emoji_images(&chunk.fetches, None).await;
        loading.store(false, Ordering::SeqCst);
        upgrade_pending_emotes(&state).await;
        ctx.request_repaint();
    }
}

/// Parse one line of a YouTube `.live_chat.json` file (a line can carry several
/// actions in the VOD-replay format), appending messages to `out` and any
/// moderation actions to `markers`.
///
/// `last_ts` carries the newest message timestamp seen so far, in stream-
/// relative seconds. Moderation actions in the live format have no timestamp of
/// their own (only messages do), so they're stamped at the position in the file
/// where they appear — which is exactly what a purge marker needs, since it
/// strikes everything said up to that point.
pub(in crate::ui) fn parse_yt_chat_line(
    line: &str,
    out: &mut Vec<ChatMessage>,
    markers: &mut Vec<MarkerAt>,
    fetches: &mut Vec<EmojiFetch>,
    last_ts: &mut f64,
) {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return,
    };
    let mut handle = |action: &serde_json::Value,
                      offset_ms: Option<i64>,
                      out: &mut Vec<ChatMessage>,
                      markers: &mut Vec<MarkerAt>| {
        if let Some(msg) = yt_action_to_msg(action, offset_ms, fetches) {
            *last_ts = msg.timestamp_secs;
            out.push(msg);
        } else if let Some(marker) = yt_action_to_marker(action) {
            let ts_secs = offset_ms.map(|ms| ms as f64 / 1000.0).unwrap_or(*last_ts);
            markers.push(MarkerAt { ts_secs, marker });
        }
    };
    if let Some(replay) = v.get("replayChatItemAction") {
        // VOD replay format: replayChatItemAction.{videoOffsetTimeMsec, actions[]}
        let offset_ms = replay
            .get("videoOffsetTimeMsec")
            .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
        if let Some(actions) = replay.get("actions").and_then(|a| a.as_array()) {
            for action in actions {
                handle(action, offset_ms, out, markers);
            }
        }
    } else {
        // Live format: the action sits directly at the top level of each line.
        handle(&v, None, out, markers);
    }
}

/// Turn one YouTube live-chat action into a replay marker, if it is one.
///
/// Which actions count — and why a by-author removal must not be called a
/// timeout or a ban — is [`crate::chat_scan::yt_moderation_action`]'s business.
/// Sharing that classifier with the archival scan is deliberate: the
/// strikethrough you see and the statistics that get recorded can then never
/// disagree about what happened.
pub(in crate::ui) fn yt_action_to_marker(action: &serde_json::Value) -> Option<ChatMarker> {
    use crate::chat_scan::YtModAction;
    match crate::chat_scan::yt_moderation_action(action)? {
        YtModAction::DeleteMessage { item_id } => {
            Some(ChatMarker::Delete { msg_id: item_id.to_string() })
        }
        YtModAction::PurgeAuthor { channel_id, reason } => Some(ChatMarker::Purge {
            key: channel_id.to_string(),
            // YouTube reports the removal but not what caused it, so the
            // fallback wording claims neither a timeout nor a ban.
            reason: reason.unwrap_or_else(|| "all messages removed by a moderator".to_string()),
        }),
    }
}

pub(in crate::ui) fn yt_action_to_msg(
    action: &serde_json::Value,
    offset_ms: Option<i64>,
    fetches: &mut Vec<EmojiFetch>,
) -> Option<ChatMessage> {
    let r = action.pointer("/addChatItemAction/item/liveChatTextMessageRenderer")?;
    let ts_secs = if let Some(ms) = offset_ms {
        ms as f64 / 1000.0
    } else {
        r["timestampUsec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .unwrap_or(0.0)
            / 1_000_000.0
    };
    let author = r
        .pointer("/authorName/simpleText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    // Identity, for moderation markers: the item id a single-message deletion
    // targets, and the author's stable channel id a by-author removal names.
    let msg_id = r["id"].as_str().unwrap_or("").to_string();
    let author_id = r["authorExternalChannelId"].as_str().unwrap_or("").to_string();
    // YouTube pre-tokenizes the body as `message.runs[]`: text runs are literal,
    // emoji runs carry either a standard unicode char (`emojiId`) or a custom
    // channel emoji (image-only). Build the display `segments` and the verbatim
    // search `text` in one pass.
    let mut text = String::new();
    let mut segments: Vec<ChatSegment> = Vec::new();
    if let Some(runs) = r["message"]["runs"].as_array() {
        for run in runs {
            if let Some(t) = run["text"].as_str() {
                text.push_str(t);
                // Text runs can themselves contain literal unicode emoji.
                segments.extend(emoji_split(t, fetches));
            } else if let Some(emoji) = run.get("emoji") {
                let shortcut = emoji["shortcuts"]
                    .as_array()
                    .and_then(|s| s.first())
                    .and_then(|e| e.as_str());
                let emoji_id = emoji["emojiId"].as_str();
                let label = shortcut.or(emoji_id).unwrap_or("[emoji]");
                text.push_str(label);
                if emoji["isCustomEmoji"].as_bool() == Some(true) {
                    // Custom channel emoji: image-only. Download YouTube's own PNG
                    // (largest thumbnail) into the cache; until present, fall back to
                    // the shortcut text.
                    let url = emoji
                        .pointer("/image/thumbnails")
                        .and_then(|t| t.as_array())
                        .and_then(|a| a.last())
                        .and_then(|t| t["url"].as_str());
                    let mut pending = None;
                    let file = emoji_id.zip(url).and_then(|(id, url)| {
                        let dest = crate::app_paths::asset_cache_dir()
                            .join("emotes")
                            .join("youtube")
                            .join(format!(
                                "{}.{}",
                                crate::downloader::sanitize_filename(id),
                                url_ext(url)
                            ));
                        if crate::iomon::fs::exists_sync(crate::iomon::Cat::AssetCache, &dest) {
                            Some(dest)
                        } else {
                            fetches.push(EmojiFetch {
                                dest: dest.clone(),
                                urls: vec![url.to_string()],
                            });
                            pending = Some(dest);
                            None
                        }
                    });
                    segments.push(ChatSegment::Emote {
                        name: label.to_string(),
                        file,
                        fallback_text: None,
                        pending,
                    });
                } else {
                    // Standard unicode emoji: `emojiId` is the actual char(s) → route
                    // through the shared Twemoji emoji pipeline for colour.
                    let glyph = emoji_id.or(shortcut).unwrap_or("[emoji]");
                    segments.extend(emoji_split(glyph, fetches));
                }
            }
        }
    }
    let badges: Vec<String> = r["authorBadges"]
        .as_array()
        .map(|bs| {
            bs.iter()
                .filter_map(|b| {
                    b.pointer("/liveChatAuthorBadgeRenderer/tooltip")
                        .and_then(|t| t.as_str())
                        .map(|t| t.split('(').next().unwrap_or(t).trim().to_string())
                })
                .collect()
        })
        .unwrap_or_default();
    Some(ChatMessage {
        timestamp_secs: ts_secs,
        // YouTube always stamps an absolute time, even when the replay also
        // carries a relative offset.
        ts_unix_ms: r["timestampUsec"]
            .as_str()
            .and_then(|s| s.parse::<f64>().ok())
            .map(|us| us / 1000.0)
            .unwrap_or(0.0),
        notice: None,
        author,
        text,
        segments,
        badges,
        badge_icons: Vec::new(),
        color_override: None,
        platform: ChatPlatform::YouTube,
        login: String::new(),
        msg_id,
        deleted: None,
        system: false,
        reply_to: String::new(),
        source_name: String::new(),
        user_id: String::new(),
        author_id,
        badge_info: String::new(),
    })
}

/// Parse a moderation **marker line** from a Twitch sidecar (written live by
/// the chat logger: `{"ts":…,"marker":"del"|"purge"|"clear"|"notice",…}`).
/// Returns the marker to apply (if any) and a visible system notice message
/// (purges, clears, and `notice` lines get one; single deletions don't — the
/// strikethrough is enough).
pub(in crate::ui) fn parse_twitch_marker_line(line: &str, start_ms: f64) -> Option<(Option<MarkerAt>, Option<ChatMessage>)> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let kind = v["marker"].as_str()?;
    let ts_secs = (v["ts"].as_f64().unwrap_or(0.0) - start_ms) / 1000.0;
    let ts_unix_ms = v["ts"].as_f64().unwrap_or(0.0);
    let notice = |text: String| ChatMessage {
        timestamp_secs: ts_secs,
        ts_unix_ms,
        notice: Some(Box::new(ChatNotice::System)),
        author: String::new(),
        segments: vec![ChatSegment::Text(text.clone())],
        text,
        badges: Vec::new(),
        badge_icons: Vec::new(),
        color_override: None,
        platform: ChatPlatform::Twitch,
        login: String::new(),
        msg_id: String::new(),
        deleted: None,
        system: true,
        reply_to: String::new(),
        source_name: String::new(),
        user_id: String::new(),
        author_id: String::new(),
        badge_info: String::new(),
    };
    match kind {
        "del" => {
            let id = v["id"].as_str().unwrap_or("");
            if id.is_empty() {
                return None;
            }
            let marker = ChatMarker::Delete { msg_id: id.to_string() };
            Some((Some(MarkerAt { ts_secs, marker }), None))
        }
        "purge" => {
            let login = v["login"].as_str().unwrap_or("").to_lowercase();
            if login.is_empty() {
                return None;
            }
            let reason = match v["secs"].as_i64() {
                Some(s) if s >= 3600 && s % 3600 == 0 => format!("timed out ({}h)", s / 3600),
                Some(s) if s >= 60 => format!("timed out ({}m)", s / 60),
                Some(s) => format!("timed out ({s}s)"),
                None => "banned".to_string(),
            };
            let n = notice(format!("{login} was {reason}"));
            Some((
                Some(MarkerAt { ts_secs, marker: ChatMarker::Purge { key: login, reason } }),
                Some(n),
            ))
        }
        "clear" => Some((
            Some(MarkerAt { ts_secs, marker: ChatMarker::Clear }),
            Some(notice("chat was cleared by moderators".into())),
        )),
        "notice" => {
            let text = v["text"].as_str().unwrap_or("").to_string();
            (!text.is_empty()).then(|| (None, Some(notice(text))))
        }
        // Sub / raid / announcement / watch-streak. `text` is Twitch's own
        // rendered `system-msg`; `body` is the user's own message when they
        // left one alongside it.
        "event" => {
            let headline = v["text"].as_str().unwrap_or("").to_string();
            if headline.is_empty() {
                return None;
            }
            let body = v["body"].as_str().unwrap_or("").to_string();
            let n = ChatNotice::from_event_kind(v["kind"].as_str().unwrap_or(""), headline)?;
            let author = v["name"].as_str().unwrap_or("").to_string();
            let mut m = notice(body.clone());
            m.notice = Some(Box::new(n));
            // NOT `system`: these get a coloured accent and an author, unlike
            // the muted ℹ room-event line, and `apply_markers` skips system
            // rows (a sub notice can legitimately be struck by a moderator).
            m.system = false;
            m.author = author;
            m.login = v["login"].as_str().unwrap_or("").to_lowercase();
            m.segments =
                if body.is_empty() { Vec::new() } else { vec![ChatSegment::Text(body)] };
            Some((None, Some(m)))
        }
        _ => None,
    }
}

/// Parse one line of a Twitch `.chat.jsonl` file. `start_ms` is the stream
/// start in unix milliseconds (timestamps become offsets from it).
#[allow(clippy::too_many_arguments)]
pub(in crate::ui) fn parse_twitch_chat_line(
    line: &str,
    start_ms: f64,
    emote_map: &HashMap<String, std::path::PathBuf>,
    twitch_dir: Option<&Path>,
    twitch_fallback_index: &HashMap<String, std::path::PathBuf>,
    fetch_unknown_emotes: bool,
    fetches: &mut Vec<EmojiFetch>,
    source_partners: &HashMap<String, crate::models::CollabPartner>,
    badge_dirs: &TwitchBadgeDirs,
) -> Option<ChatMessage> {
    let v: serde_json::Value = serde_json::from_str(line).ok()?;
    let ts_ms = v["ts"].as_f64().unwrap_or(0.0);
    let author = v["name"]
        .as_str()
        .or_else(|| v["login"].as_str())
        .unwrap_or("")
        .to_string();
    // Unwrap `/me` CTCP actions so the emote offsets (which index the inner
    // body) align and the raw control chars don't show in the replay/search.
    let text = strip_ctcp_action(v["text"].as_str().unwrap_or("")).to_string();
    let color_override = v["color"].as_str().and_then(parse_chat_hex_color);
    // Split raw badge tag "subscriber/12,moderator/1" into one entry per badge.
    let badges = v["badges"]
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.split(',').map(str::to_string).collect::<Vec<_>>())
        .unwrap_or_default();
    // Resolved once here (not per render frame) — see `resolve_twitch_badge_icon`'s doc.
    let badge_icons: Vec<Option<std::path::PathBuf>> =
        badges.iter().map(|b| resolve_twitch_badge_icon(b, badge_dirs)).collect();
    // `emotes` tag is absent on pre-feature logs → empty → first-party emotes
    // simply don't render (third-party word-matching still applies).
    let emotes_tag = v["emotes"].as_str().unwrap_or("");
    let segments = build_twitch_segments(
        &text,
        emotes_tag,
        emote_map,
        twitch_dir,
        twitch_fallback_index,
        fetch_unknown_emotes,
        fetches,
    );
    // Split literal unicode emoji out of the text segments into colour images.
    let segments = expand_emoji(segments, fetches);
    // Present only during an active Shared Chat session (Twitch tags every
    // message with it then, including ones typed locally). Resolved against
    // this take's recorded partners — a local message's own room id was
    // never recorded as a "partner" (the monitored channel itself is never
    // in that list, see `CollabPartner`'s doc), so it naturally falls
    // through to no indicator; only messages from an actual OTHER channel
    // resolve to a name.
    let source_room_id = v["source_room_id"].as_str().unwrap_or("");
    let source_name = if source_room_id.is_empty() {
        String::new()
    } else {
        source_partners.get(source_room_id).map(|p| p.name.clone()).unwrap_or_default()
    };
    // A message can be both a first message and a redemption; the redemption
    // is the more specific fact and carries its own header line, so it wins.
    // Absent from every log written before these fields existed, which simply
    // means no accent — exactly as before.
    let reward_id = v["reward_id"].as_str().unwrap_or("");
    let notice = if !reward_id.is_empty() {
        Some(Box::new(ChatNotice::Redemption {
            // IRC never names the reward; the title is resolved separately.
            reward: None,
            reward_id: reward_id.to_string(),
            cost: None,
        }))
    } else if v["msg_kind"].as_str() == Some("highlighted-message") {
        // The one reward identifiable without a lookup: Twitch gives it its
        // own msg-id rather than a custom-reward-id.
        Some(Box::new(ChatNotice::Redemption {
            reward: Some("Highlight My Message".to_string()),
            reward_id: String::new(),
            cost: None,
        }))
    } else if v["first"].as_bool().unwrap_or(false) {
        Some(Box::new(ChatNotice::FirstMessage))
    } else {
        None
    };
    Some(ChatMessage {
        timestamp_secs: (ts_ms - start_ms) / 1000.0,
        // The sidecar's `ts` is unix milliseconds; `start_ms` only converts it
        // to a stream-relative offset.
        ts_unix_ms: ts_ms,
        notice,
        author,
        text,
        segments,
        badges,
        badge_icons,
        color_override,
        platform: ChatPlatform::Twitch,
        login: v["login"].as_str().unwrap_or("").to_lowercase(),
        msg_id: v["id"].as_str().unwrap_or("").to_string(),
        deleted: None,
        system: false,
        reply_to: v["reply"].as_str().unwrap_or("").to_string(),
        source_name,
        user_id: v["user_id"].as_str().unwrap_or("").to_string(),
        // Twitch identifies a chatter by login in the moderation feed, so this
        // stays empty — see `ChatMessage::author_id`.
        author_id: String::new(),
        badge_info: v["badge_info"].as_str().unwrap_or("").to_string(),
    })
}
