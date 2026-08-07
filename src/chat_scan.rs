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
use tracing::{info, warn};

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

/// Read one YouTube sidecar and return every moderation action in it, oldest
/// first.
///
/// `started_at` anchors the VOD-replay format's stream-relative offsets; the
/// live format's own `timestampUsec` is absolute and used as-is. An action with
/// neither (the live format doesn't timestamp moderator actions) is stamped at
/// the last message before it, which is where it happened.
pub fn scan_youtube_sidecar(path: &std::path::Path, started_at: i64) -> anyhow::Result<Vec<ScannedEvent>> {
    use std::io::BufRead;
    let f = crate::iomon::fs::open_sync(crate::iomon::Cat::ChatSidecar, path)?;
    let reader = std::io::BufReader::new(f);

    // Author display names, and the message index deletions are resolved
    // through. Both are per-file and dropped when the scan returns.
    let mut names: HashMap<String, String> = HashMap::new();
    let mut messages: HashMap<String, (String, String)> = HashMap::new(); // item id -> (author id, text)
    let mut out: Vec<ScannedEvent> = Vec::new();
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
                if let Some(name) = r.pointer("/authorName/simpleText").and_then(Value::as_str) {
                    names.insert(author_id.to_string(), name.to_string());
                }
                if messages.len() < MAX_INDEXED_MESSAGES
                    && let Some(id) = r["id"].as_str().filter(|s| !s.is_empty())
                {
                    let text: String = r["message"]["runs"]
                        .as_array()
                        .map(|runs| runs.iter().filter_map(|x| x["text"].as_str()).collect::<Vec<_>>().join(""))
                        .unwrap_or_default();
                    messages.insert(id.to_string(), (author_id.to_string(), excerpt(&text, EXCERPT_CHARS)));
                }
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
                    out.push(ScannedEvent {
                        at,
                        kind: "msg_deleted",
                        actor: names.get(&author_id).cloned().unwrap_or_default(),
                        target: author_id,
                        detail: text,
                    });
                }
                Some(YtModAction::PurgeAuthor { channel_id, reason }) => {
                    out.push(ScannedEvent {
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

/// Mine the chat sidecars of finished takes that haven't been read yet, in
/// small batches, and record what they contain as `stream_event` rows.
///
/// Self-throttled to [`SWEEP_INTERVAL_SECS`] — call it from the scheduler tick
/// beside the other sweeps and let it decide. Every take it looks at is stamped
/// [scanned](Store::set_recording_chat_scanned) whether or not it yielded
/// anything, including Twitch takes (whose events were already recorded live)
/// and files that have since been deleted — the stamp means "we've been here",
/// not "we found something", and without that the queue would never drain.
pub async fn maybe_sweep_chat_scan(store: &Store, now: i64) {
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

    let due = match store.recordings_needing_chat_scan(SCAN_BATCH) {
        Ok(v) if v.is_empty() => return,
        Ok(v) => v,
        Err(e) => {
            warn!("chat scan: query failed: {e:#}");
            return;
        }
    };
    for t in due {
        // Twitch: nothing to mine, its moderation events were written live.
        if !is_youtube_sidecar(&t.chat_path) {
            let _ = store.set_recording_chat_scanned(t.rec_id, now);
            continue;
        }
        let path = std::path::PathBuf::from(&t.chat_path);
        let started_at = t.started_at;
        // Parsing a marathon chat log is seconds of CPU and megabytes of I/O:
        // off the scheduler's thread it goes.
        let scanned =
            tokio::task::spawn_blocking(move || scan_youtube_sidecar(&path, started_at)).await;
        let events = match scanned {
            Ok(Ok(v)) => v,
            Ok(Err(e)) => {
                // Missing/unreadable file: stamp it anyway. Retrying forever on
                // a sidecar that will never come back would wedge the queue.
                warn!(rec_id = t.rec_id, path = %t.chat_path, "chat scan: unreadable: {e:#}");
                let _ = store.set_recording_chat_scanned(t.rec_id, now);
                continue;
            }
            Err(e) => {
                warn!(rec_id = t.rec_id, "chat scan: task failed: {e:#}");
                continue; // not stamped — a panicked/cancelled task gets retried
            }
        };
        let found = events.len();
        for e in events {
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
        let got = scan_youtube_sidecar(&p, 1_000).unwrap();
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
        let got = scan_youtube_sidecar(&p, 4_000).unwrap();
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
        let got = scan_youtube_sidecar(&p, 700).unwrap();
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
        assert!(scan_youtube_sidecar(&p, 0).unwrap().is_empty());
    }

    #[test]
    fn only_youtube_sidecars_are_scannable() {
        assert!(is_youtube_sidecar("C:/x/a.live_chat.json"));
        assert!(!is_youtube_sidecar("C:/x/a.chat.jsonl"));
    }
}
