//! Structural Twitch ad-break detection: poll the live HLS manifest ourselves
//! (the same public `PlaybackAccessToken` + `usher.ttvnw.net` flow every
//! Twitch player — and streamlink itself — uses) and parse `EXT-X-DATERANGE`
//! ad tags directly, instead of relying solely on streamlink's own
//! `Detected advertisement break of N second(s)` log line.
//!
//! That line (`parse_ad_break_secs`, `process.rs`) only fires when the ad's
//! daterange tag *also* carries an `X-TV-TWITCH-AD-COMMERCIAL-ID`/
//! `X-TV-TWITCH-AD-ROLL-TYPE` attribute — a census of 155 real production
//! capture logs found it in exactly ZERO of them, even though streamlink's
//! unconditional ad-segment filtering (`Will skip ad segments`, present in
//! nearly all of them) means ad cuts are almost certainly still happening.
//! The raw daterange tag that actually drives that filter
//! (`CLASS="twitch-stitched-ad"`/`ID="stitched-ad-*"`) needs only
//! `START-DATE`/`DURATION`, so this fires far more often.
//!
//! Detections feed the SAME channel `spawn_ad_processor` (`process.rs`)
//! already drains — this module is a second *producer*, never a second
//! writer: the ffprobe-based cut-offset math, UI ad-tint, and
//! `insert_ad_break` write are all reused untouched.
//!
//! Read-only manifest text, nothing more: we already have full legitimate
//! stream access via streamlink. The persisted-query hash is undocumented
//! and could change without notice — failures degrade soft (backoff + a
//! `capture_alert`), never touching the main capture.

use super::*;

use chrono::{DateTime, Utc};

/// Settings key: `"0"` disables the live-manifest ad probe (default on).
pub const K_AD_PROBE: &str = "ad_probe";

pub(super) fn ad_probe_enabled(store: &Store) -> bool {
    store.get_setting(K_AD_PROBE).ok().flatten().as_deref() != Some("0")
}

/// How often the probe re-fetches the (small) media playlist. Comfortably
/// under a typical Twitch ad break's ~15-90s duration and under the live
/// playlist's rolling window, so at least one poll always overlaps any real
/// break.
const AD_PROBE_INTERVAL_SECS: u64 = 10;
/// Back off this long after a token/manifest fetch failure, so a bad network
/// blip or an upstream API change doesn't retry every 10s forever.
const AD_PROBE_FAIL_COOLDOWN_SECS: u64 = 300;
/// Consecutive failed cycles before surfacing a `capture_alert` — one blip
/// shouldn't page the user, a sustained outage should.
const AD_PROBE_DEGRADED_AFTER: u32 = 3;

/// Twitch's private (undocumented, but publicly used by every player and by
/// streamlink itself — verified against the installed streamlink 8.1.0
/// source, `plugins/twitch.py`) persisted-query hash for
/// `PlaybackAccessToken`.
const PLAYBACK_TOKEN_HASH: &str = "ed230aa1e33e07eebb8928504583da78a5173989fadfb1ac94be06a04f3cdbe9";

/// One Twitch ad daterange tag observed in the live manifest.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct AdTag {
    pub id: String,
    #[allow(dead_code)] // parsed for completeness/tests; the offset math reuses detection-time wall clock, same as the log-line path
    pub start: DateTime<Utc>,
    pub duration_secs: f64,
}

impl Supervisor {
    /// Spawn the per-take live-manifest ad probe. Exits (dropping its `tx`
    /// clone) once `done` is set, mirroring `tail_log`'s own stop condition —
    /// `spawn_ad_processor`'s consumer only ends once every sender is gone.
    pub(super) fn spawn_ad_probe(
        &self,
        login: String,
        monitor_id: i64,
        rec_id: i64,
        tx: mpsc::UnboundedSender<(i64, i64)>,
        done: Arc<AtomicBool>,
    ) -> tokio::task::JoinHandle<()> {
        let client = self.ctx.http_client();
        let store = self.store.clone();
        tokio::spawn(async move {
            run_ad_probe(client, store, login, monitor_id, rec_id, tx, done).await;
        })
    }
}

async fn run_ad_probe(
    client: reqwest::Client,
    store: Arc<Store>,
    login: String,
    monitor_id: i64,
    rec_id: i64,
    tx: mpsc::UnboundedSender<(i64, i64)>,
    done: Arc<AtomicBool>,
) {
    let mut seen: HashSet<String> = HashSet::new();
    let mut media_url: Option<String> = None;
    let mut consecutive_failures: u32 = 0;
    let mut alerted_degraded = false;

    while !done.load(Ordering::SeqCst) {
        if !ad_probe_enabled(&store) {
            return;
        }

        let cycle_result: anyhow::Result<()> = async {
            if media_url.is_none() {
                media_url = Some(resolve_media_playlist_url(&client, &login).await?);
            }
            let url = media_url.as_ref().expect("just set above");
            let text = match fetch_text(&client, url).await {
                Ok(t) => t,
                Err(e) => {
                    // The token may have expired or the CDN URL rotated —
                    // force a fresh resolve next cycle.
                    media_url = None;
                    return Err(e);
                }
            };
            for tag in parse_ad_dateranges(&text) {
                if tag.duration_secs > 0.0 && seen.insert(tag.id.clone()) {
                    let _ = tx.send((now_unix(), tag.duration_secs.round() as i64));
                }
            }
            Ok(())
        }
        .await;

        match cycle_result {
            Ok(()) => {
                consecutive_failures = 0;
                alerted_degraded = false;
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(monitor_id, rec_id, "ad probe ({login}): {e:#}");
                if consecutive_failures >= AD_PROBE_DEGRADED_AFTER && !alerted_degraded {
                    alerted_degraded = true;
                    file_ad_probe_degraded_alert(&store, monitor_id, rec_id, &login);
                }
            }
        }

        let backoff_secs =
            if consecutive_failures == 0 { AD_PROBE_INTERVAL_SECS } else { AD_PROBE_FAIL_COOLDOWN_SECS };
        for _ in 0..backoff_secs {
            if done.load(Ordering::SeqCst) {
                return;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
}

/// File (or grow) a warning alert so a probe that's been failing for a while
/// surfaces in the 🚨 Warnings window instead of going dark for months like
/// the detector this module supplements. Never touches the capture itself.
fn file_ad_probe_degraded_alert(store: &Store, monitor_id: i64, rec_id: i64, login: &str) {
    let alert = crate::store::NewCaptureAlert {
        kind: "ad_probe_degraded".to_string(),
        severity: "warning".to_string(),
        source: "ad_probe".to_string(),
        take_key: format!("ad_probe:rec{rec_id}"),
        monitor_id: Some(monitor_id),
        recording_id: Some(rec_id),
        video_id: None,
        channel: login.to_string(),
        count: 1,
        lost_segments: 0,
        last_line: format!(
            "Live ad-break manifest probe degraded for {login} — the playback-token/manifest \
             fetch has failed {AD_PROBE_DEGRADED_AFTER}+ cycles in a row. Ad-break accounting for \
             this take may be incomplete; the capture itself is unaffected."
        ),
    };
    match store.upsert_capture_alert(&alert) {
        Ok((id, was_new)) if was_new => info!(rec_id, "ad probe: filed degraded alert #{id} for {login}"),
        Ok(_) => {}
        Err(e) => warn!(rec_id, "ad probe: failed to file degraded alert: {e:#}"),
    }
}

async fn fetch_text(client: &reqwest::Client, url: &str) -> anyhow::Result<String> {
    let resp = client.get(url).send().await?.error_for_status()?;
    Ok(resp.text().await?)
}

/// Fetch a live playback token then resolve the cheapest media-playlist URL
/// to poll for ad dateranges.
async fn resolve_media_playlist_url(client: &reqwest::Client, login: &str) -> anyhow::Result<String> {
    let (sig, token) = fetch_playback_token(client, login).await?;
    let resp = client
        .get(format!("https://usher.ttvnw.net/api/channel/hls/{login}.m3u8"))
        .query(&[
            ("token", token.as_str()),
            ("sig", sig.as_str()),
            ("allow_source", "true"),
            ("allow_audio_only", "true"),
            ("p", &now_unix().to_string()),
        ])
        .send()
        .await?
        .error_for_status()?;
    let master = resp.text().await?;
    pick_cheapest_variant(&master).ok_or_else(|| anyhow::anyhow!("no variant in master playlist"))
}

/// Anonymous GQL `PlaybackAccessToken` query — the same one every Twitch
/// player (and streamlink internally) uses to get a live-stream `usher` URL.
/// Returns `(signature, token)`.
async fn fetch_playback_token(client: &reqwest::Client, login: &str) -> anyhow::Result<(String, String)> {
    let body = serde_json::json!({
        "operationName": "PlaybackAccessToken",
        "extensions": {
            "persistedQuery": { "version": 1, "sha256Hash": PLAYBACK_TOKEN_HASH },
        },
        "variables": {
            "isLive": true,
            "login": login,
            "isVod": false,
            "vodID": "",
            "playerType": "embed",
            "platform": "site",
        },
    });
    let resp = client
        .post("https://gql.twitch.tv/gql")
        .header("Client-Id", crate::recovery::GQL_CLIENT_ID)
        .json(&body)
        .send()
        .await?
        .error_for_status()?;
    let v: serde_json::Value = resp.json().await?;
    let tok = &v["data"]["streamPlaybackAccessToken"];
    let sig = tok["signature"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("no streamPlaybackAccessToken (channel offline or blocked?)"))?
        .to_string();
    let value = tok["value"].as_str().ok_or_else(|| anyhow::anyhow!("no playback token value"))?.to_string();
    Ok((sig, value))
}

/// Pick the cheapest rendition to poll from a Twitch master playlist: we only
/// need the `EXT-X-DATERANGE` ad tags a media playlist carries, never the
/// actual video/audio bytes, so the smallest stream is the free one. Twitch's
/// SSAI ad-stitching applies uniformly across every rendition of one
/// broadcast, so which rendition we poll doesn't change what ads we see.
pub(super) fn pick_cheapest_variant(master: &str) -> Option<String> {
    let mut best: Option<(i64, String)> = None;
    let lines: Vec<&str> = master.lines().collect();
    for (i, line) in lines.iter().enumerate() {
        let Some(attrs) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let Some(uri) = lines.get(i + 1).map(|s| s.trim()) else { continue };
        if uri.is_empty() || uri.starts_with('#') {
            continue;
        }
        if attrs.to_ascii_lowercase().contains("audio_only") {
            return Some(uri.to_string());
        }
        let bw = attrs
            .split(',')
            .find_map(|kv| kv.trim().strip_prefix("BANDWIDTH="))
            .and_then(|v| v.parse::<i64>().ok())
            .unwrap_or(i64::MAX);
        if best.as_ref().map(|(b, _)| bw < *b).unwrap_or(true) {
            best = Some((bw, uri.to_string()));
        }
    }
    best.map(|(_, uri)| uri)
}

/// Parse `EXT-X-DATERANGE` ad tags out of a Twitch media playlist. Matches
/// streamlink's own predicate (`_is_daterange_ad`, `twitch.py`) but needs only
/// `START-DATE`/`DURATION` — not the extra commercial-id/roll-type fields the
/// log-line detector (`parse_ad_break_secs`) requires — so it fires far more
/// often.
pub(super) fn parse_ad_dateranges(text: &str) -> Vec<AdTag> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("#EXT-X-DATERANGE:") else { continue };
        let attrs = parse_attr_list(rest);
        let is_ad = attrs.get("CLASS").map(|c| c == "twitch-stitched-ad").unwrap_or(false)
            || attrs.get("ID").map(|id| id.starts_with("stitched-ad-")).unwrap_or(false);
        if !is_ad {
            continue;
        }
        let Some(id) = attrs.get("ID").cloned() else { continue };
        let Some(start) = attrs.get("START-DATE").and_then(|s| DateTime::parse_from_rfc3339(s).ok()) else {
            continue;
        };
        let Some(duration_secs) = attrs.get("DURATION").and_then(|s| s.parse::<f64>().ok()) else { continue };
        out.push(AdTag { id, start: start.with_timezone(&Utc), duration_secs });
    }
    out
}

/// Attribute-list parser for one `#EXT-X-*:k=v,k="v,with,commas",...` tag —
/// comma-splits outside quotes so a quoted value can't be mistaken for extra
/// attributes.
fn parse_attr_list(rest: &str) -> HashMap<String, String> {
    let mut parts: Vec<&str> = Vec::new();
    let mut start = 0;
    let mut in_quotes = false;
    for (i, c) in rest.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ',' if !in_quotes => {
                parts.push(&rest[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    parts.push(&rest[start..]);

    let mut out = HashMap::new();
    for part in parts {
        if let Some((k, v)) = part.split_once('=') {
            out.insert(k.trim().to_string(), v.trim().trim_matches('"').to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ad_probe_enabled_default_on_and_opt_out() {
        let store = Store::open_in_memory().unwrap();
        assert!(ad_probe_enabled(&store));
        store.set_setting(K_AD_PROBE, "0").unwrap();
        assert!(!ad_probe_enabled(&store));
    }

    #[test]
    fn parse_ad_dateranges_minimal_tag_without_extra_fields() {
        // The OLD log-line detector needs an extra commercial-id/roll-type
        // attribute to fire at all; this parser needs only START-DATE and
        // DURATION, so it must still find this tag.
        let text = "#EXT-X-DATERANGE:ID=\"stitched-ad-abc\",CLASS=\"twitch-stitched-ad\",\
                     START-DATE=\"2026-07-22T18:00:00.000Z\",DURATION=30.000\n";
        let tags = parse_ad_dateranges(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].id, "stitched-ad-abc");
        assert_eq!(tags[0].duration_secs, 30.0);
        assert_eq!(tags[0].start.to_rfc3339(), "2026-07-22T18:00:00+00:00");
    }

    #[test]
    fn parse_ad_dateranges_full_tag_with_commercial_fields() {
        let text = "#EXT-X-DATERANGE:ID=\"stitched-ad-2026-07-22T18:00:00.000Z-0\",\
                     CLASS=\"twitch-stitched-ad\",START-DATE=\"2026-07-22T18:00:00.000Z\",\
                     DURATION=90.000,X-TV-TWITCH-AD-POD-FILLED-DURATION=\"90\",\
                     X-TV-TWITCH-AD-ROLL-TYPE=\"MIDROLL\"\n";
        let tags = parse_ad_dateranges(text);
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].duration_secs, 90.0);
    }

    #[test]
    fn parse_ad_dateranges_ignores_non_ad_dateranges() {
        let text = "#EXT-X-DATERANGE:ID=\"some-other-marker\",CLASS=\"some-other-class\",\
                     START-DATE=\"2026-07-22T18:05:00.000Z\",DURATION=5.000\n";
        assert!(parse_ad_dateranges(text).is_empty());
    }

    #[test]
    fn parse_ad_dateranges_matches_by_id_prefix_without_class() {
        // streamlink's own predicate: CLASS match OR id starts "stitched-ad-".
        let text =
            "#EXT-X-DATERANGE:ID=\"stitched-ad-xyz\",START-DATE=\"2026-07-22T18:10:00.000Z\",DURATION=15.000\n";
        assert_eq!(parse_ad_dateranges(text).len(), 1);
    }

    #[test]
    fn dedup_by_id_across_polls() {
        let text = "#EXT-X-DATERANGE:ID=\"stitched-ad-x\",CLASS=\"twitch-stitched-ad\",\
                     START-DATE=\"2026-07-22T18:00:00.000Z\",DURATION=30.000\n";
        let mut seen = HashSet::new();
        let first = parse_ad_dateranges(text);
        assert!(seen.insert(first[0].id.clone()));
        // A later "poll" of the same still-in-window playlist sees the same
        // tag again — the caller's HashSet must not double-count it.
        let second = parse_ad_dateranges(text);
        assert!(!seen.insert(second[0].id.clone()), "same tag must dedupe across polls");
    }

    #[test]
    fn pick_cheapest_variant_prefers_audio_only() {
        let master = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=6000000,RESOLUTION=1920x1080,VIDEO=\"1080p60\"
https://example.com/1080p60/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=160000,VIDEO=\"audio_only\"
https://example.com/audio_only/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=2000000,VIDEO=\"720p60\"
https://example.com/720p60/index.m3u8
";
        assert_eq!(pick_cheapest_variant(master).as_deref(), Some("https://example.com/audio_only/index.m3u8"));
    }

    #[test]
    fn pick_cheapest_variant_falls_back_to_lowest_bandwidth() {
        let master = "\
#EXTM3U
#EXT-X-STREAM-INF:BANDWIDTH=6000000,VIDEO=\"1080p60\"
https://example.com/1080p60/index.m3u8
#EXT-X-STREAM-INF:BANDWIDTH=800000,VIDEO=\"480p30\"
https://example.com/480p30/index.m3u8
";
        assert_eq!(pick_cheapest_variant(master).as_deref(), Some("https://example.com/480p30/index.m3u8"));
    }

    #[test]
    fn pick_cheapest_variant_none_when_no_streams() {
        assert_eq!(pick_cheapest_variant("#EXTM3U\n"), None);
    }
}
