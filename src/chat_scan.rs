//! Harvest chat-moderation actions out of **YouTube** chat sidecars.
//!
//! Twitch chat is captured by our own IRC client ([`crate::chat`]), which sees
//! `CLEARMSG`/`CLEARCHAT` as they happen and writes the matching `stream_event`
//! rows live. YouTube chat is captured by yt-dlp into a `.live_chat.json`
//! sidecar with no hook of ours anywhere in the loop, so its moderation actions
//! have to be read back out of the finished file — that's this module.
//!
//! Two deliberate asymmetries with the Twitch path:
//!
//! * **Only finished takes are scanned.** A live sidecar is still being
//!   appended to, and re-reading a growing file every minute would be wasteful
//!   and duplicate-prone. Nothing the user *sees* waits on this — the chat
//!   replay strikes deleted messages the moment it parses them, from the very
//!   same actions — only the recorded statistics land at the end.
//! * **A removal is recorded as `chat_purge`, never as `timeout` or `ban`.**
//!   YouTube's by-author removal says a moderator wiped everything that person
//!   said; it does not say whether they were muted for ten minutes or banned
//!   forever. Twitch tells us which, so it gets the specific kinds; YouTube
//!   gets an honest one that claims neither.
//!
//! The sweep also stamps Twitch takes as scanned without reading them (their
//! events were recorded live), which is what keeps the work queue draining.

use std::collections::HashMap;

use serde_json::Value;

use crate::chat_index::{IndexedMessage, UserKey};
use tracing::{debug, info, warn};

use crate::store::Store;

/// `app_settings` key holding the unix time of the last scan sweep.
const K_LAST_SWEEP: &str = "chat_scan_last_sweep";
/// Minimum gap between sweeps — this is archival bookkeeping, not anything the
/// user is waiting on.
const SWEEP_INTERVAL_SECS: i64 = 60;
/// How many takes one sweep will look at. Bounds the first run after this
/// feature ships, when every YouTube take ever recorded is unscanned.
const SCAN_BATCH: i64 = 5;
/// Cap on the per-file message index (id → author/text). A deletion names the
/// message it removed, so attributing one needs that message's row; this bounds
/// the memory a marathon chat can cost. Past the cap, deletions are still
/// counted — they just lose the excerpt and the chatter's name.
const MAX_INDEXED_MESSAGES: usize = 200_000;
/// Settings key: chat indexing on/off. The kill switch — off stops every
/// read and write the index does, immediately.
pub const K_INDEX_ENABLED: &str = "chat_index_enabled";
/// Settings key: how many takes one sweep indexes.
pub const K_INDEX_BATCH: &str = "chat_index_batch";
/// Default index batch: at one sweep a minute this drains a 1,200-file
/// backlog in about four hours, unattended, without ever being the loudest
/// thing on the disk.
const INDEX_BATCH_DEFAULT: i64 = 5;
/// Ceiling on the batch, including what the "Scan all" button may ask for —
/// a sweep still has to finish inside a scheduler tick.
pub const INDEX_BATCH_MAX: i64 = 200;
/// A single take costing more than this is worth a warning: at 2 s one file
/// is already an outlier (the measured median is well under 100 ms).
const SLOW_TAKE_WARN_MS: u128 = 2_000;
/// `app_settings` key: unix time of the last legacy-login resolve pass.
const K_LAST_RESOLVE: &str = "chat_index_last_resolve";
/// Legacy logins are historical — resolving them is never urgent, and each pass
/// spends a Helix call.
const RESOLVE_INTERVAL_SECS: i64 = 600;
/// Helix Get Users caps at 100 logins per request; one request per pass.
const RESOLVE_BATCH: i64 = 100;
/// Deleted-text excerpt length, matching the Twitch logger's.
const EXCERPT_CHARS: usize = 120;

/// One of YouTube's two moderator actions, under whichever of its two names the
/// sidecar happened to use.
///
/// YouTube expresses each action two ways — `mark…AsDeleted` (leaves a
/// tombstone carrying a `deletedStateMessage`) and `remove…` (drops the item) —
/// and which one arrives depends on whether the sidecar came from a live
/// continuation or a VOD replay. Both spellings are accepted rather than
/// guessing from the file.
pub enum YtModAction<'a> {
    /// One message removed, named by its item id.
    DeleteMessage { item_id: &'a str },
    /// Everything one author said was removed, named by their `UC…` channel id.
    /// `reason` is YouTube's own wording where it gave one.
    PurgeAuthor { channel_id: &'a str, reason: Option<String> },
}

/// Classify one action from a `.live_chat.json` line. `None` for everything
/// that isn't a moderator action (messages, membership items, superchats…).
pub fn yt_moderation_action(action: &Value) -> Option<YtModAction<'_>> {
    if let Some(id) = action
        .pointer("/markChatItemAsDeletedAction/targetItemId")
        .or_else(|| action.pointer("/removeChatItemAction/targetItemId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        return Some(YtModAction::DeleteMessage { item_id: id });
    }
    let channel_id = action
        .pointer("/markChatItemsByAuthorAsDeletedAction/externalChannelId")
        .or_else(|| action.pointer("/removeChatItemByAuthorAction/externalChannelId"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())?;
    let reason = action
        .pointer("/markChatItemsByAuthorAsDeletedAction/deletedStateMessage/runs")
        .and_then(Value::as_array)
        .map(|runs| {
            runs.iter().filter_map(|r| r["text"].as_str()).collect::<Vec<_>>().join("").trim().to_string()
        })
        .filter(|s| !s.is_empty());
    Some(YtModAction::PurgeAuthor { channel_id, reason })
}

/// One moderation event recovered from a sidecar, in `stream_event` shape.
#[derive(Debug, PartialEq, Eq)]
pub struct ScannedEvent {
    pub at: i64,
    /// `msg_deleted` or `chat_purge`.
    pub kind: &'static str,
    /// The chatter it happened to, by display name — empty when the sidecar
    /// never carried a message from them (so their name was never seen).
    pub actor: String,
    /// Their `UC…` channel id.
    pub target: String,
    pub detail: String,
}

/// True for the sidecars this module can read — yt-dlp names YouTube's chat
/// dump after the subtitle track it really is (`{stem}.live_chat.json`).
pub fn is_youtube_sidecar(path: &str) -> bool {
    path.to_ascii_lowercase().ends_with("live_chat.json")
}

fn excerpt(s: &str, max: usize) -> String {
    let t = s.trim();
    if t.chars().count() <= max {
        return t.to_string();
    }
    t.chars().take(max).collect::<String>() + "…"
}

/// What a caller wants out of a sidecar read.
///
/// A take can need moderation harvesting, index rows, or both, and the two
/// queues drain at different rates — but the file should only ever be opened
/// once for a given take. The sweep asks for whatever is still outstanding and
/// pays for one read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ScanWants {
    /// Moderation actions for `stream_event` (YouTube only — Twitch records
    /// its own live).
    pub events: bool,
    /// Messages and identities for [`crate::chat_index`].
    pub messages: bool,
}

impl ScanWants {
    pub fn nothing(self) -> bool {
        !self.events && !self.messages
    }
}

/// One sidecar's contents, in whichever shapes were asked for.
#[derive(Debug, Default)]
pub struct SidecarScan {
    pub events: Vec<ScannedEvent>,
    pub messages: Vec<IndexedMessage>,
    /// Bytes read, for the throughput figure in the sweep's log line.
    pub bytes: u64,
}

/// Read one **Twitch** sidecar for the chat index.
///
/// Twitch moderation is recorded live by our own IRC client, so there is
/// nothing to harvest here — only messages. Lines carrying a `marker` field are
/// the replay's deletion/purge/notice annotations rather than anything a person
/// said, and are skipped.
///
/// `user_id` only appears in logs written from 2026-08-05 onward; older lines
/// fall back to a login key. See [`UserKey`].
pub fn scan_twitch_sidecar(path: &std::path::Path) -> anyhow::Result<SidecarScan> {
    use std::io::BufRead;
    let f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
    let reader = std::io::BufReader::with_capacity(256 * 1024, f);
    let mut out = SidecarScan { bytes, ..Default::default() };
    for line in reader.lines() {
        let Ok(line) = line else { break }; // unreadable tail: keep what we have
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        if let Some(m) = twitch_message(&v) {
            out.messages.push(m);
        }
    }
    Ok(out)
}

/// One indexable message from a Twitch sidecar line, or `None` for the marker
/// lines and anything missing the fields an identity needs.
fn twitch_message(v: &Value) -> Option<IndexedMessage> {
    if v.get("marker").is_some() {
        return None; // replay annotation, not a chat message
    }
    let text = v.get("text").and_then(Value::as_str)?;
    let login = v.get("login").and_then(Value::as_str).unwrap_or("");
    let user_id = v.get("user_id").and_then(Value::as_str).unwrap_or("");
    let key = UserKey::new("twitch", user_id, login)?;
    let display = v.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(login);
    // The sidecar stamps milliseconds; the index works in seconds like every
    // other timestamp in the app.
    let at = v.get("ts").and_then(Value::as_i64).unwrap_or(0) / 1000;
    Some(IndexedMessage {
        key,
        login: login.to_string(),
        display: display.to_string(),
        at,
        text: text.to_string(),
    })
}

/// Read one YouTube sidecar, returning the moderation actions in it (oldest
/// first) and/or its messages, per `wants`.
///
/// `started_at` anchors the VOD-replay format's stream-relative offsets; the
/// live format's own `timestampUsec` is absolute and used as-is. An action with
/// neither (the live format doesn't timestamp moderator actions) is stamped at
/// the last message before it, which is where it happened.
pub fn scan_youtube_sidecar(
    path: &std::path::Path,
    started_at: i64,
    wants: ScanWants,
) -> anyhow::Result<SidecarScan> {
    use std::io::BufRead;
    let f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let bytes = f.metadata().map(|m| m.len()).unwrap_or(0);
    let reader = std::io::BufReader::with_capacity(256 * 1024, f);

    // Author display names, and the message index deletions are resolved
    // through. Both are per-file and dropped when the scan returns.
    let mut names: HashMap<String, String> = HashMap::new();
    let mut messages: HashMap<String, (String, String)> = HashMap::new(); // item id -> (author id, text)
    let mut out = SidecarScan { bytes, ..Default::default() };
    let mut last_at = started_at;

    for line in reader.lines() {
        let Ok(line) = line else { break }; // unreadable tail: keep what we have
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(&line) else { continue };
        // VOD replay wraps a batch of actions with one video offset; live lines
        // are a bare action.
        let (actions, offset_ms): (Vec<&Value>, Option<i64>) = match v.get("replayChatItemAction") {
            Some(replay) => {
                let offset = replay
                    .get("videoOffsetTimeMsec")
                    .and_then(|x| x.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| x.as_i64()));
                (replay.get("actions").and_then(Value::as_array).map(|a| a.iter().collect()).unwrap_or_default(), offset)
            }
            None => (vec![&v], None),
        };
        for action in actions {
            if let Some(r) = action.pointer("/addChatItemAction/item/liveChatTextMessageRenderer") {
                let at = match offset_ms {
                    Some(ms) => started_at + ms / 1000,
                    None => r["timestampUsec"]
                        .as_str()
                        .and_then(|s| s.parse::<i64>().ok())
                        .map(|us| us / 1_000_000)
                        .unwrap_or(last_at),
                };
                last_at = at;
                let author_id = r["authorExternalChannelId"].as_str().unwrap_or("");
                if author_id.is_empty() {
                    continue;
                }
                let name = r.pointer("/authorName/simpleText").and_then(Value::as_str);
                if let Some(name) = name {
                    names.insert(author_id.to_string(), name.to_string());
                }
                let text: Option<String> = (wants.messages
                    || messages.len() < MAX_INDEXED_MESSAGES)
                    .then(|| {
                        r["message"]["runs"]
                            .as_array()
                            .map(|runs| {
                                runs.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join("")
                            })
                            .unwrap_or_default()
                    });
                if wants.messages
                    && let Some(text) = text.as_deref()
                    // YouTube has no login concept — the channel id is the only
                    // stable handle, and it is always present here.
                    && let Some(key) = UserKey::new("youtube", author_id, "")
                {
                    out.messages.push(IndexedMessage {
                        key,
                        login: String::new(),
                        display: name.unwrap_or_default().to_string(),
                        at,
                        text: text.to_string(),
                    });
                }
                if wants.events
                    && messages.len() < MAX_INDEXED_MESSAGES
                    && let Some(id) = r["id"].as_str().filter(|s| !s.is_empty())
                {
                    let text = text.unwrap_or_default();
                    messages.insert(id.to_string(), (author_id.to_string(), excerpt(&text, EXCERPT_CHARS)));
                }
                continue;
            }
            if !wants.events {
                continue;
            }
            let at = match offset_ms {
                Some(ms) => started_at + ms / 1000,
                None => last_at,
            };
            match yt_moderation_action(action) {
                Some(YtModAction::DeleteMessage { item_id }) => {
                    let (author_id, text) = messages
                        .get(item_id)
                        .map(|(a, t)| (a.clone(), t.clone()))
                        .unwrap_or_default();
                    out.events.push(ScannedEvent {
                        at,
                        kind: "msg_deleted",
                        actor: names.get(&author_id).cloned().unwrap_or_default(),
                        target: author_id,
                        detail: text,
                    });
                }
                Some(YtModAction::PurgeAuthor { channel_id, reason }) => {
                    out.events.push(ScannedEvent {
                        at,
                        kind: "chat_purge",
                        actor: names.get(channel_id).cloned().unwrap_or_default(),
                        target: channel_id.to_string(),
                        detail: reason.unwrap_or_default(),
                    });
                }
                None => {}
            }
        }
    }
    Ok(out)
}

/// One take the sweep has decided to read, and why.
struct Due {
    rec_id: i64,
    monitor_id: i64,
    channel_id: i64,
    stream_id: String,
    started_at: i64,
    chat_path: String,
    youtube: bool,
    wants: ScanWants,
}

/// Mine the chat sidecars of finished takes that haven't been read yet, in
/// small batches: moderation actions into `stream_event`, messages and
/// identities into [`crate::chat_index`].
///
/// Self-throttled to [`SWEEP_INTERVAL_SECS`] — call it from the scheduler tick
/// beside the other sweeps and let it decide. Every take it looks at is stamped
/// whether or not it yielded anything, including Twitch takes (whose moderation
/// events were already recorded live) and files that have since been deleted —
/// the stamp means "we've been here", not "we found something", and without
/// that the queue would never drain.
///
/// The two jobs share one read. Their queues are independent (moderation only
/// ever wanted YouTube files, and its backlog drained a day before indexing
/// existed), but a take that owes both must not be opened twice.
pub async fn maybe_sweep_chat_scan(
    store: &Store,
    index: Option<&std::sync::Arc<crate::chat_index::ChatIndex>>,
    now: i64,
) {
    let last = store
        .get_setting(K_LAST_SWEEP)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < SWEEP_INTERVAL_SECS {
        return;
    }
    let _ = store.set_setting(K_LAST_SWEEP, &now.to_string());

    let index = index.filter(|_| index_enabled(store));
    let due = match collect_due(store, index, now) {
        Ok(v) if v.is_empty() => return,
        Ok(v) => v,
        Err(e) => {
            warn!("chat scan: query failed: {e:#}");
            return;
        }
    };

    let sweep_started = std::time::Instant::now();
    let (mut read, mut skipped, mut indexed_msgs, mut found_events) = (0u32, 0u32, 0i64, 0usize);
    for t in due {
        // A capture in flight on the same drive outranks archival bookkeeping:
        // this is the load pattern that has knocked the USB enclosure off the
        // bus before, and nothing here is time-critical.
        let path = std::path::PathBuf::from(&t.chat_path);
        let _pass = crate::io_gate::local_pass("chat index", &path).await;

        let started_at = t.started_at;
        let wants = t.wants;
        let youtube = t.youtube;
        // Parsing a marathon chat log is seconds of CPU and megabytes of I/O:
        // off the scheduler's thread it goes.
        let t_parse = std::time::Instant::now();
        let scanned = tokio::task::spawn_blocking(move || {
            if youtube {
                scan_youtube_sidecar(&path, started_at, wants)
            } else {
                scan_twitch_sidecar(&path)
            }
        })
        .await;
        let parse_ms = t_parse.elapsed().as_millis();

        let scan = match scanned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                // Missing/unreadable file: stamp it anyway. Retrying forever on
                // a sidecar that will never come back would wedge the queue.
                //
                // A sidecar that simply isn't there is the common case while
                // the backlog of old takes drains (moved, pruned, or written
                // before chat logging existed) — routine, and at WARN it
                // buried real problems in the log. Anything else still warns.
                let missing = e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                let status = if missing {
                    debug!(rec_id = t.rec_id, path = %t.chat_path, "chat scan: sidecar is gone");
                    crate::chat_index::status::MISSING
                } else {
                    warn!(rec_id = t.rec_id, path = %t.chat_path, "chat scan: unreadable: {e:#}");
                    crate::chat_index::status::FAILED
                };
                if t.wants.events {
                    let _ = store.set_recording_chat_scanned(t.rec_id, now);
                }
                if let Some(idx) = index.filter(|_| t.wants.messages)
                    && let Err(e) = idx.stamp_take(t.rec_id, &t.chat_path, status, now)
                {
                    warn!(rec_id = t.rec_id, "chat index: stamping failed: {e:#}");
                }
                skipped += 1;
                continue;
            }
            Err(e) => {
                warn!(rec_id = t.rec_id, "chat scan: task failed: {e:#}");
                continue; // not stamped — a panicked/cancelled task gets retried
            }
        };
        read += 1;

        if t.wants.events {
            let found = scan.events.len();
            found_events += found;
            for e in &scan.events {
                if let Err(err) = store.record_stream_event(
                    t.monitor_id,
                    e.at,
                    &t.stream_id,
                    e.kind,
                    &e.actor,
                    &e.target,
                    0,
                    "",
                    &e.detail,
                ) {
                    warn!(rec_id = t.rec_id, "chat scan: recording event failed: {err:#}");
                }
            }
            let _ = store.set_recording_chat_scanned(t.rec_id, now);
            if found > 0 {
                info!(rec_id = t.rec_id, found, "chat scan: recorded YouTube moderation events");
            }
        }

        if let Some(idx) = index.filter(|_| t.wants.messages) {
            let parsed = crate::chat_index::ParsedSidecar {
                messages: scan.messages,
                bytes: scan.bytes,
            };
            let mb = parsed.bytes as f64 / (1024.0 * 1024.0);
            let t_write = std::time::Instant::now();
            let take = crate::chat_index::TakeRef {
                rec_id: t.rec_id,
                monitor_id: t.monitor_id,
                channel_id: t.channel_id,
                chat_path: &t.chat_path,
            };
            match idx.write_take(&take, &parsed, parse_ms, now) {
                Ok((msgs, users)) => {
                    indexed_msgs += msgs;
                    let write_ms = t_write.elapsed().as_millis();
                    let total_ms = parse_ms + write_ms;
                    // One line per take, carrying everything needed to find a
                    // pathological file without re-running anything.
                    let rate = if total_ms > 0 { mb / (total_ms as f64 / 1000.0) } else { 0.0 };
                    info!(
                        rec_id = t.rec_id,
                        platform = if t.youtube { "youtube" } else { "twitch" },
                        mb = format!("{mb:.1}"),
                        msgs,
                        users,
                        parse_ms = parse_ms as u64,
                        write_ms = write_ms as u64,
                        total_ms = total_ms as u64,
                        mb_per_s = format!("{rate:.1}"),
                        "chat index: indexed take"
                    );
                    if total_ms >= SLOW_TAKE_WARN_MS {
                        warn!(
                            rec_id = t.rec_id,
                            total_ms = total_ms as u64,
                            mb = format!("{mb:.1}"),
                            msgs,
                            "chat index: take took longer than expected — \
                             check the chat index lane in the I/O tab"
                        );
                    }
                }
                Err(e) => {
                    warn!(rec_id = t.rec_id, "chat index: write failed: {e:#}");
                    let _ = idx.stamp_take(
                        t.rec_id,
                        &t.chat_path,
                        crate::chat_index::status::FAILED,
                        now,
                    );
                }
            }
        }
    }

    if read > 0 || skipped > 0 {
        debug!(
            read,
            skipped,
            events = found_events,
            messages = indexed_msgs,
            sweep_ms = sweep_started.elapsed().as_millis() as u64,
            "chat scan: sweep finished"
        );
    }
}

/// Pick this cycle's batch: takes owing a moderation scan, an index read, or
/// both. Newest first for indexing (a fresh index is most useful for the
/// streams the user just watched), oldest first for moderation (its existing
/// FIFO order, so a long-drained queue stays drained).
fn collect_due(
    store: &Store,
    index: Option<&std::sync::Arc<crate::chat_index::ChatIndex>>,
    _now: i64,
) -> anyhow::Result<Vec<Due>> {
    let mut due: Vec<Due> = Vec::new();
    for t in store.recordings_needing_chat_scan(SCAN_BATCH)? {
        due.push(Due {
            rec_id: t.rec_id,
            monitor_id: t.monitor_id,
            channel_id: 0, // filled in below when indexing also wants this take
            stream_id: t.stream_id,
            started_at: t.started_at,
            youtube: is_youtube_sidecar(&t.chat_path),
            chat_path: t.chat_path,
            wants: ScanWants { events: true, messages: false },
        });
    }
    // Twitch takes have nothing to mine — their moderation events were written
    // live. Stamp them here rather than opening the file for nothing; indexing
    // may still want them, which the pass below decides on its own.
    due.retain(|d| {
        if d.youtube {
            return true;
        }
        let _ = store.set_recording_chat_scanned(d.rec_id, crate::models::now_unix());
        false
    });

    let Some(idx) = index else { return Ok(due) };
    let already = idx.indexed_rec_ids()?;
    let mut budget = index_batch(store);
    for c in store.chat_index_candidates()? {
        if budget == 0 {
            break;
        }
        if already.contains(&c.rec_id) {
            continue;
        }
        budget -= 1;
        if let Some(d) = due.iter_mut().find(|d| d.rec_id == c.rec_id) {
            // Already queued for a moderation scan: one read, both jobs.
            d.wants.messages = true;
            d.channel_id = c.channel_id;
            continue;
        }
        due.push(Due {
            rec_id: c.rec_id,
            monitor_id: c.monitor_id,
            channel_id: c.channel_id,
            stream_id: String::new(),
            started_at: c.started_at,
            youtube: is_youtube_sidecar(&c.chat_path),
            chat_path: c.chat_path,
            wants: ScanWants { events: false, messages: true },
        });
    }
    due.retain(|d| !d.wants.nothing());
    Ok(due)
}

/// Is chat indexing switched on? Default yes — but a single setting turns off
/// every read and write below, which is the first thing to reach for if the
/// index is ever suspected of costing more than it's worth.
pub fn index_enabled(store: &Store) -> bool {
    store
        .get_setting(K_INDEX_ENABLED)
        .ok()
        .flatten()
        .map(|v| v.trim() != "0" && !v.trim().eq_ignore_ascii_case("false"))
        .unwrap_or(true)
}

/// How many takes one sweep will index. Configurable so a user in a hurry can
/// raise it (and so the "Scan all" button can raise it for one cycle).
pub fn index_batch(store: &Store) -> i64 {
    store
        .get_setting(K_INDEX_BATCH)
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(INDEX_BATCH_DEFAULT)
        .min(INDEX_BATCH_MAX)
}

/// Index one take's chat log right now, jumping the sweep's queue.
///
/// The manual path behind "index this channel's chat logs". It reuses the same
/// parse and the same write as the background sweep — a second implementation
/// would be a second set of bugs — and still passes through the disk gate, so
/// a user in a hurry still cannot outrank a running capture.
///
/// Returns `(messages, chatters)`.
pub async fn index_take_now(
    index: &std::sync::Arc<crate::chat_index::ChatIndex>,
    target: &crate::store::ChatIndexTarget,
    now: i64,
) -> anyhow::Result<(i64, i64)> {
    let path = std::path::PathBuf::from(&target.chat_path);
    let _pass = crate::io_gate::local_pass("chat index", &path).await;
    let started_at = target.started_at;
    let youtube = is_youtube_sidecar(&target.chat_path);
    let t_parse = std::time::Instant::now();
    let scan = tokio::task::spawn_blocking(move || {
        if youtube {
            scan_youtube_sidecar(&path, started_at, ScanWants { events: false, messages: true })
        } else {
            scan_twitch_sidecar(&path)
        }
    })
    .await??;
    let parse_ms = t_parse.elapsed().as_millis();
    let parsed =
        crate::chat_index::ParsedSidecar { messages: scan.messages, bytes: scan.bytes };
    let take = crate::chat_index::TakeRef {
        rec_id: target.rec_id,
        monitor_id: target.monitor_id,
        channel_id: target.channel_id,
        chat_path: &target.chat_path,
    };
    let out = index.write_take(&take, &parsed, parse_ms, now)?;
    debug!(
        rec_id = target.rec_id,
        msgs = out.0,
        users = out.1,
        parse_ms = parse_ms as u64,
        "chat index: indexed take on request"
    );
    Ok(out)
}

/// Index the most recent `limit` chat logs of one channel, newest first,
/// skipping any already done.
///
/// Returns `(takes read, messages indexed, takes that could not be read)`.
pub async fn index_channel_now(
    store: &Store,
    index: &std::sync::Arc<crate::chat_index::ChatIndex>,
    channel_id: i64,
    limit: usize,
    now: i64,
) -> (u32, i64, u32) {
    let already = index.indexed_rec_ids().unwrap_or_default();
    let targets: Vec<_> = store
        .chat_index_candidates()
        .unwrap_or_default()
        .into_iter()
        .filter(|c| c.channel_id == channel_id && !already.contains(&c.rec_id))
        .take(limit)
        .collect();
    let (mut done, mut msgs, mut failed) = (0u32, 0i64, 0u32);
    for t in &targets {
        match index_take_now(index, t, now).await {
            Ok((m, _)) => {
                done += 1;
                msgs += m;
            }
            Err(e) => {
                failed += 1;
                let missing = e
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound);
                let status = if missing {
                    crate::chat_index::status::MISSING
                } else {
                    warn!(rec_id = t.rec_id, "chat index: on-request read failed: {e:#}");
                    crate::chat_index::status::FAILED
                };
                let _ = index.stamp_take(t.rec_id, &t.chat_path, status, now);
            }
        }
    }
    info!(channel_id, done, msgs, failed, "chat index: indexed a channel on request");
    (done, msgs, failed)
}

/// Fold login-keyed Twitch chatters into their real account ids.
///
/// Sidecars written before 2026-08-05 carry no `user-id` at all — about two
/// thirds of the Twitch archive — so those chatters are indexed under their
/// login. Helix Get Users maps a login to an id 100 at a time, and a hit merges
/// the two identities (see
/// [`ChatIndex::resolve_login`](crate::chat_index::ChatIndex::resolve_login)).
///
/// Busiest logins first: they are the ones a user is most likely to look up,
/// and the ones whose split history is most visible.
///
/// **This is name matching, and the UI says so.** Helix answers with whoever
/// holds that login *today*, so a chatter who has since renamed will be folded
/// into the wrong account. That is stated on the identity rather than hidden,
/// because the alternative — leaving two thirds of the archive unattributable —
/// is worse, and because a merge that announces itself can be reasoned about.
pub async fn maybe_resolve_logins(ctx: &std::sync::Arc<crate::detectors::DetectContext>, now: i64) {
    let store = &ctx.store;
    let last = store
        .get_setting(K_LAST_RESOLVE)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .unwrap_or(0);
    if now - last < RESOLVE_INTERVAL_SECS {
        return;
    }
    let Some(idx) = crate::chat_index::shared().filter(|_| index_enabled(store)) else { return };
    let _ = store.set_setting(K_LAST_RESOLVE, &now.to_string());

    let pending = match idx.unresolved_logins(RESOLVE_BATCH) {
        Ok(v) if v.is_empty() => return,
        Ok(v) => v,
        Err(e) => {
            warn!("chat index: unresolved-login query failed: {e:#}");
            return;
        }
    };
    let logins: Vec<String> = pending.iter().map(|(_, l)| l.to_lowercase()).collect();
    let t = std::time::Instant::now();
    let Some(found) = ctx.twitch_ids_for_logins(&logins).await else {
        debug!(n = logins.len(), "chat index: Helix lookup failed, retrying next sweep");
        return;
    };
    let (mut merged, mut unknown) = (0u32, 0u32);
    for (user_id, login) in &pending {
        let id = found.get(&login.to_lowercase()).map(String::as_str);
        if id.is_none() {
            unknown += 1;
        }
        match idx.resolve_login(*user_id, id) {
            Ok(true) => merged += 1,
            Ok(false) => {}
            Err(e) => warn!(login = %login, "chat index: merge failed: {e:#}"),
        }
    }
    info!(
        asked = pending.len(),
        merged,
        unknown,
        ms = t.elapsed().as_millis() as u64,
        "chat index: resolved legacy logins"
    );
}

#[cfg(test)]
mod tests {
    // Test-only: throwaway sidecar files iomon has no need to attribute.
    #![allow(clippy::disallowed_methods)]
    use super::*;

    /// Write a throwaway sidecar and hand back its path. A directory per case
    /// keeps parallel test threads from colliding.
    fn write(case: &str, name: &str, lines: &[&str]) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("sa_chat_scan_{}_{case}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(name);
        std::fs::write(&p, lines.join("\n")).unwrap();
        p
    }

    /// Just the moderation actions — what the sweep asks for on a take that
    /// only owes a moderation scan.
    const EVENTS: ScanWants = ScanWants { events: true, messages: false };
    /// Just the index rows.
    const MESSAGES: ScanWants = ScanWants { events: false, messages: true };
    /// Both, from one read — the case that exists because a take can owe both
    /// and must only be opened once.
    const BOTH: ScanWants = ScanWants { events: true, messages: true };

    #[test]
    fn scans_replay_format_resolving_names_and_excerpts() {
        let p = write(
            "a",
            "a.live_chat.json",
            &[
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"5000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"msg1","authorExternalChannelId":"UCspam","authorName":{"simpleText":"Spammer"},"message":{"runs":[{"text":"buy followers at example.com"}]}}}}}]}}"#,
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"9000","actions":[{"markChatItemAsDeletedAction":{"targetItemId":"msg1","deletedStateMessage":{"runs":[{"text":"Message deleted by moderator"}]}}}]}}"#,
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"12000","actions":[{"markChatItemsByAuthorAsDeletedAction":{"externalChannelId":"UCspam","deletedStateMessage":{"runs":[{"text":"Message deleted by moderator"}]}}}]}}"#,
            ],
        );
        let got = scan_youtube_sidecar(&p, 1_000, EVENTS).unwrap().events;
        assert_eq!(got.len(), 2);
        assert_eq!(
            got[0],
            ScannedEvent {
                at: 1_009,
                kind: "msg_deleted",
                actor: "Spammer".into(),
                target: "UCspam".into(),
                detail: "buy followers at example.com".into(),
            }
        );
        assert_eq!(got[1].kind, "chat_purge");
        assert_eq!((got[1].at, got[1].actor.as_str()), (1_012, "Spammer"));
    }

    #[test]
    fn live_format_stamps_untimed_actions_at_the_last_message() {
        let p = write(
            "b",
            "b.live_chat.json",
            &[
                r#"{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m1","authorExternalChannelId":"UCa","authorName":{"simpleText":"Ann"},"timestampUsec":"5000000000","message":{"runs":[{"text":"hi"}]}}}}}"#,
                r#"{"removeChatItemByAuthorAction":{"externalChannelId":"UCa"}}"#,
            ],
        );
        let got = scan_youtube_sidecar(&p, 4_000, EVENTS).unwrap().events;
        assert_eq!(got.len(), 1);
        // Stamped at the message before it, not at the capture start.
        assert_eq!((got[0].at, got[0].kind), (5_000, "chat_purge"));
        // No `deletedStateMessage` on the `remove…` spelling: no invented reason.
        assert!(got[0].detail.is_empty());
    }

    #[test]
    fn deletion_of_an_unseen_message_is_still_counted() {
        let p = write(
            "c",
            "c.live_chat.json",
            &[r#"{"markChatItemAsDeletedAction":{"targetItemId":"ghost"}}"#],
        );
        let got = scan_youtube_sidecar(&p, 700, EVENTS).unwrap().events;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].kind, "msg_deleted");
        // Nothing known about who said it — recorded blank, never guessed.
        assert!(got[0].actor.is_empty() && got[0].target.is_empty());
    }

    #[test]
    fn ordinary_chat_produces_nothing() {
        let p = write(
            "d",
            "d.live_chat.json",
            &[
                r#"{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m1","authorExternalChannelId":"UCa","authorName":{"simpleText":"Ann"},"message":{"runs":[{"text":"hello"}]}}}}}"#,
                r#"{"addChatItemAction":{"item":{"liveChatPaidMessageRenderer":{"id":"m2"}}}}"#,
                "not json at all",
            ],
        );
        assert!(scan_youtube_sidecar(&p, 0, EVENTS).unwrap().events.is_empty());
    }

    #[test]
    fn only_youtube_sidecars_are_scannable() {
        assert!(is_youtube_sidecar("C:/x/a.live_chat.json"));
        assert!(!is_youtube_sidecar("C:/x/a.chat.jsonl"));
    }

    #[test]
    fn youtube_messages_are_keyed_on_the_channel_id() {
        let p = write(
            "e",
            "e.live_chat.json",
            &[
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"5000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m1","authorExternalChannelId":"UCann","authorName":{"simpleText":"Ann"},"message":{"runs":[{"text":"hello "},{"text":"world"}]}}}}}]}}"#,
            ],
        );
        let got = scan_youtube_sidecar(&p, 1_000, MESSAGES).unwrap();
        assert_eq!(got.messages.len(), 1);
        let m = &got.messages[0];
        assert_eq!((m.key.platform.as_str(), m.key.key.as_str()), ("youtube", "UCann"));
        assert!(
            !crate::chat_index::key_is_name_matched(&m.key.key),
            "YouTube always has a stable id"
        );
        // Runs are joined, not just the first one.
        assert_eq!((m.display.as_str(), m.text.as_str(), m.at), ("Ann", "hello world", 1_005));
    }

    #[test]
    fn one_read_can_serve_both_jobs() {
        let p = write(
            "f",
            "f.live_chat.json",
            &[
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"1000","actions":[{"addChatItemAction":{"item":{"liveChatTextMessageRenderer":{"id":"m1","authorExternalChannelId":"UCspam","authorName":{"simpleText":"Spammer"},"message":{"runs":[{"text":"spam"}]}}}}}]}}"#,
                r#"{"replayChatItemAction":{"videoOffsetTimeMsec":"2000","actions":[{"markChatItemAsDeletedAction":{"targetItemId":"m1"}}]}}"#,
            ],
        );
        let got = scan_youtube_sidecar(&p, 0, BOTH).unwrap();
        assert_eq!(got.events.len(), 1, "moderation action still harvested");
        assert_eq!(got.messages.len(), 1, "and the message indexed, from the same read");
        // The deletion still resolves through the message index to a name.
        assert_eq!(got.events[0].actor, "Spammer");
    }

    #[test]
    fn twitch_messages_prefer_the_user_id_and_skip_markers() {
        let p = write(
            "g",
            "g.chat.jsonl",
            &[
                r#"{"ts":1700000000000,"login":"ann","name":"Ann","text":"hello","user_id":"42"}"#,
                // A replay marker, not something anyone said.
                r#"{"ts":1700000001000,"marker":"del","id":"abc"}"#,
                // Pre-2026-08-05 line: no user_id at all.
                r#"{"ts":1700000002000,"login":"Bob","name":"Bob","text":"hi"}"#,
            ],
        );
        let got = scan_twitch_sidecar(&p).unwrap();
        assert_eq!(got.messages.len(), 2, "the marker line is not a message");
        assert_eq!(got.messages[0].key.key, "42");
        assert!(!crate::chat_index::key_is_name_matched(&got.messages[0].key.key));
        // Milliseconds in the log, seconds in the index.
        assert_eq!(got.messages[0].at, 1_700_000_000);
        // The legacy line falls back to a lowercased login key.
        assert_eq!(got.messages[1].key.key, "login:bob");
        assert!(crate::chat_index::key_is_name_matched(&got.messages[1].key.key));
        assert_eq!(got.messages[1].display, "Bob");
        assert!(got.bytes > 0, "byte count feeds the throughput log line");
    }

    #[test]
    fn a_twitch_line_with_no_identity_is_dropped() {
        let p = write(
            "h",
            "h.chat.jsonl",
            &[r#"{"ts":1700000000000,"login":"","name":"","text":"ghost"}"#],
        );
        assert!(scan_twitch_sidecar(&p).unwrap().messages.is_empty());
    }
}
