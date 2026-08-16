//! Finding clips in archived chat.
//!
//! This is the **only** discovery path for YouTube clips — YouTube has no API
//! to enumerate a channel's or a video's clips — and it doubles as discovery for
//! Twitch clips of channels you don't monitor, which the Helix sweep can never
//! see. Measured on this archive: 400 chat sidecars yielded 1,227 Twitch clip
//! links, 631 of them distinct.
//!
//! Extraction happens **at scan time**, folded into the sweep that already reads
//! every line of every sidecar exactly once (`chat_scan`). Querying the FTS
//! index instead would mean designing a query for URL fragments against a
//! tokenizer that splits them, and re-reading a corpus we are already holding.

use super::*;
use crate::chat_index::IndexedMessage;
use std::collections::HashMap;

/// A clip seen in chat, with where and when it was posted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HarvestedClip {
    pub clip: ClipRef,
    /// Earliest time it was posted in this log. Not the clip's own creation
    /// time — a clip is often shared long after it was made.
    pub first_seen_at: i64,
    /// How many times it was posted here. A clip spammed twenty times is one
    /// catalogue entry, not twenty.
    pub mentions: usize,
}

/// Pull every distinct clip URL out of one log's messages.
///
/// Deduped by `(platform, slug)`, keeping the earliest sighting — chat repeats
/// a good clip constantly and each repeat is the same artifact.
pub fn extract_clip_refs(messages: &[IndexedMessage]) -> Vec<HarvestedClip> {
    let mut found: HashMap<(Platform, String), HarvestedClip> = HashMap::new();
    for m in messages {
        for r in clip_refs_in(&m.text) {
            let key = (r.platform, r.slug.clone());
            found
                .entry(key)
                .and_modify(|h| {
                    h.mentions += 1;
                    h.first_seen_at = h.first_seen_at.min(m.at);
                    // A `twitch.tv/<login>/clip/` sighting reveals the
                    // broadcaster; a bare `clips.twitch.tv/` one does not. Keep
                    // whichever form told us more.
                    if h.clip.login.is_none() && r.login.is_some() {
                        h.clip.login = r.login.clone();
                    }
                })
                .or_insert(HarvestedClip {
                    clip: r,
                    first_seen_at: m.at,
                    mentions: 1,
                });
        }
    }
    let mut out: Vec<HarvestedClip> = found.into_values().collect();
    out.sort_by(|a, b| {
        a.first_seen_at
            .cmp(&b.first_seen_at)
            .then_with(|| a.clip.slug.cmp(&b.clip.slug))
    });
    out
}

/// Every clip URL in one message. A message can carry several.
fn clip_refs_in(text: &str) -> Vec<ClipRef> {
    let mut out = Vec::new();
    // Scan by whitespace rather than regex: chat is short, this runs per
    // message over the whole archive, and `parse_clip_url` already handles the
    // three URL shapes plus query/fragment tails.
    for token in text.split_whitespace() {
        // Trim the punctuation chat wraps links in — "(clip)" , "<url>", "url!"
        let t = token.trim_matches(|c: char| {
            matches!(c, '(' | ')' | '[' | ']' | '<' | '>' | '"' | '\'' | ',' | '!' | '?')
        });
        if let Some(r) = parse_clip_url(t)
            && !out.iter().any(|e: &ClipRef| e.slug == r.slug && e.platform == r.platform)
        {
            out.push(r);
        }
    }
    out
}

/// Persist harvested refs as catalogue rows.
///
/// These land with no metadata beyond the URL: a Twitch slug is hydrated on the
/// next sweep via `GET /helix/clips?id=`, and a YouTube one by the yt-dlp probe.
/// `channel_id`/`monitor_id` are filled in only when the broadcaster is one we
/// monitor — a clip of someone else's channel is still worth cataloguing, it
/// just has no local home and will not be downloaded.
pub fn record_harvest(
    store: &Store,
    harvested: &[HarvestedClip],
    logins_to_channel: &HashMap<String, (i64, i64)>,
    now: i64,
) -> usize {
    let mut n = 0;
    for h in harvested {
        // Never overwrite what a Helix sweep already knows — `upsert_clip`
        // protects the recovery keys, but there is no point rewriting a
        // fully-populated row with an empty one either.
        if matches!(
            store.clip_by_slug(h.clip.platform, &h.clip.slug),
            Ok(Some(_))
        ) {
            continue;
        }
        let (channel_id, monitor_id) = h
            .clip
            .login
            .as_ref()
            .and_then(|l| logins_to_channel.get(l))
            .map(|(c, m)| (Some(*c), Some(*m)))
            .unwrap_or((None, None));
        let c = Clip {
            platform: h.clip.platform,
            slug: h.clip.slug.clone(),
            channel_id,
            monitor_id,
            broadcaster_login: h.clip.login.clone().unwrap_or_default(),
            url: harvest_url(&h.clip),
            source: "chat".into(),
            first_seen_at: h.first_seen_at,
            ..Default::default()
        };
        if store.upsert_clip(&c, now).is_ok() {
            n += 1;
        }
    }
    n
}

/// Rebuild a canonical URL from a ref, so the row is openable and downloadable
/// before anything hydrates it.
fn harvest_url(r: &ClipRef) -> String {
    match r.platform {
        Platform::YouTube => format!("https://www.youtube.com/clip/{}", r.slug),
        _ => match &r.login {
            Some(l) => format!("https://www.twitch.tv/{l}/clip/{}", r.slug),
            None => format!("https://clips.twitch.tv/{}", r.slug),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_index::UserKey;

    fn msg(at: i64, text: &str) -> IndexedMessage {
        IndexedMessage {
            key: UserKey::new("twitch", "1", "someone").unwrap(),
            login: "someone".into(),
            display: "Someone".into(),
            at,
            text: text.into(),
        }
    }

    #[test]
    fn extracts_all_three_url_shapes_from_chat() {
        let msgs = vec![
            msg(10, "lmao https://clips.twitch.tv/FunnyThing-abc"),
            msg(20, "https://www.twitch.tv/laynalazar/clip/OtherThing-def is better"),
            msg(30, "https://www.youtube.com/clip/UgkxABC123"),
        ];
        let got = extract_clip_refs(&msgs);
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].clip.slug, "FunnyThing-abc");
        assert_eq!(got[1].clip.login.as_deref(), Some("laynalazar"));
        assert_eq!(got[2].clip.platform, Platform::YouTube);
    }

    #[test]
    fn the_same_clip_spammed_twenty_times_is_one_entry() {
        // Chat repeats a good clip constantly; each repeat is the same artifact.
        let msgs: Vec<_> = (0..20)
            .map(|i| msg(100 + i, "https://clips.twitch.tv/Same-abc"))
            .collect();
        let got = extract_clip_refs(&msgs);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].mentions, 20);
        assert_eq!(got[0].first_seen_at, 100, "earliest sighting wins");
    }

    #[test]
    fn a_later_sighting_can_reveal_the_broadcaster_the_first_one_hid() {
        // clips.twitch.tv/<slug> carries no login; twitch.tv/<login>/clip/ does.
        let msgs = vec![
            msg(10, "https://clips.twitch.tv/Same-abc"),
            msg(20, "https://www.twitch.tv/laynalazar/clip/Same-abc"),
        ];
        let got = extract_clip_refs(&msgs);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].clip.login.as_deref(), Some("laynalazar"));
        assert_eq!(got[0].first_seen_at, 10);
    }

    #[test]
    fn links_survive_the_punctuation_chat_wraps_them_in() {
        for t in [
            "(https://clips.twitch.tv/Wrapped-abc)",
            "<https://clips.twitch.tv/Wrapped-abc>",
            "look: https://clips.twitch.tv/Wrapped-abc!",
            "\"https://clips.twitch.tv/Wrapped-abc\",",
        ] {
            let got = extract_clip_refs(&[msg(1, t)]);
            assert_eq!(got.len(), 1, "{t}");
            assert_eq!(got[0].clip.slug, "Wrapped-abc", "{t}");
        }
    }

    #[test]
    fn several_clips_in_one_message_are_all_found() {
        let got = extract_clip_refs(&[msg(
            1,
            "https://clips.twitch.tv/One-a and https://clips.twitch.tv/Two-b",
        )]);
        assert_eq!(got.len(), 2);
    }

    #[test]
    fn ordinary_chat_yields_nothing() {
        let msgs = vec![
            msg(1, "hello twitch.tv is a website"),
            msg(2, "https://www.twitch.tv/laynalazar"),
            msg(3, "https://www.youtube.com/watch?v=abc"),
            msg(4, "KEKW"),
        ];
        assert!(extract_clip_refs(&msgs).is_empty());
    }

    #[test]
    fn a_harvested_url_is_openable_before_anything_hydrates_it() {
        let tw = ClipRef {
            platform: Platform::Twitch,
            slug: "Abc-1".into(),
            login: Some("layna".into()),
        };
        assert_eq!(harvest_url(&tw), "https://www.twitch.tv/layna/clip/Abc-1");
        let bare = ClipRef { login: None, ..tw.clone() };
        assert_eq!(harvest_url(&bare), "https://clips.twitch.tv/Abc-1");
        let yt = ClipRef {
            platform: Platform::YouTube,
            slug: "Ugk1".into(),
            login: None,
        };
        assert_eq!(harvest_url(&yt), "https://www.youtube.com/clip/Ugk1");
    }

    #[test]
    fn harvest_records_foreign_clips_without_a_channel_and_never_clobbers_helix() {
        let store = Store::open_in_memory().unwrap();
        let mut logins = HashMap::new();
        logins.insert("laynalazar".to_string(), (7i64, 9i64));

        let harvested = extract_clip_refs(&[
            msg(10, "https://www.twitch.tv/laynalazar/clip/Mine-a"),
            msg(20, "https://www.twitch.tv/someoneelse/clip/Theirs-b"),
        ]);
        let n = record_harvest(&store, &harvested, &logins, 100);
        assert_eq!(n, 2);

        let mine = store.clip_by_slug(Platform::Twitch, "Mine-a").unwrap().unwrap();
        assert_eq!(mine.channel_id, Some(7));
        assert_eq!(mine.monitor_id, Some(9));
        assert_eq!(mine.source, "chat");

        // A clip of a channel we don't monitor is still catalogued — it just
        // has no local home, so it will never be downloaded.
        let theirs = store.clip_by_slug(Platform::Twitch, "Theirs-b").unwrap().unwrap();
        assert_eq!(theirs.channel_id, None);
        assert_eq!(theirs.broadcaster_login, "someoneelse");

        // Re-harvesting must not overwrite a row Helix has since populated.
        store
            .upsert_clip(
                &Clip {
                    platform: Platform::Twitch,
                    slug: "Mine-a".into(),
                    vod_id: "555".into(),
                    vod_offset_secs: Some(42),
                    title: "Real title".into(),
                    ..Default::default()
                },
                200,
            )
            .unwrap();
        assert_eq!(record_harvest(&store, &harvested, &logins, 300), 0);
        let after = store.clip_by_slug(Platform::Twitch, "Mine-a").unwrap().unwrap();
        assert_eq!(after.title, "Real title");
        assert_eq!(after.vod_offset_secs, Some(42));
    }
}
