//! 7TV "paints" — the gradient usernames some chatters have.
//!
//! Fetched from 7TV's v4 GraphQL endpoint. The old `v3/cosmetics` route is
//! gone and v4 is undocumented, so the query below was derived by
//! introspecting the live schema (2026-08-08); every field is read
//! defensively and any failure leaves usernames rendering exactly as they do
//! today, which is why this is safe to depend on at all.
//!
//! What is NOT rendered, deliberately — see [`Paint::sample`]:
//! radial gradients (approximated as linear), image paints, drop shadows, and
//! animation. egui's `TextFormat` carries a flat colour per run and nothing
//! else, so a gradient is quantized into a handful of runs; anything needing
//! a real fill or a per-frame recolour is out of reach without a rendering
//! backend this app has never used.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// Whether gradient usernames render at all. Default on; only an explicit
/// `"0"` disables.
pub const K_RENDER_PAINTS: &str = "chat_render_7tv_paints";

/// How long a channel's cached paints stay fresh. Cosmetics change rarely and
/// a stale gradient is a cosmetic non-event.
pub const PAINTS_TTL_SECS: i64 = 24 * 60 * 60;

/// Twitch ids per GraphQL request. The endpoint scores query complexity, so
/// this stays modest rather than asking for hundreds of aliases at once.
const CHUNK: usize = 50;

/// One colour stop: position along the gradient (0..1) and an `#RRGGBBAA`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Stop {
    pub at: f32,
    pub rgba: [u8; 4],
}

/// A user's active paint, reduced to what can actually be drawn.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Paint {
    pub name: String,
    /// Sorted by position. A single-colour paint is two identical stops, so
    /// sampling needs no special case.
    pub stops: Vec<Stop>,
    /// Gradient angle in degrees, as 7TV gives it (0 = upward, growing
    /// clockwise). Only its horizontal component is representable — see
    /// [`Paint::sample`].
    pub angle: i32,
}

impl Paint {
    /// The colour at `t` (0..1) along the name's horizontal extent.
    ///
    /// **This is an approximation, and the interesting part is which way.**
    /// egui lays a `LayoutJob`'s runs out left to right, so only the
    /// gradient's horizontal component can be expressed: the angle is
    /// projected onto the x axis, which means a vertical gradient (angle 180,
    /// far and away the most common) collapses to a flat colour — its
    /// midpoint — rather than rendering wrong. That reads as "this person has
    /// a coloured name", which is most of the value, and never as a gradient
    /// running the wrong way.
    pub fn sample(&self, t: f32) -> [u8; 4] {
        if self.stops.is_empty() {
            return [255, 255, 255, 255];
        }
        // cos of the angle from vertical: ±1 for a horizontal gradient, 0 for
        // a vertical one. Negative flips the direction.
        let dir = (self.angle as f32 - 90.0).to_radians().cos();
        let t = 0.5 + (t - 0.5) * dir;
        let t = t.clamp(0.0, 1.0);
        let first = &self.stops[0];
        if t <= first.at {
            return first.rgba;
        }
        for w in self.stops.windows(2) {
            let (a, b) = (&w[0], &w[1]);
            if t <= b.at {
                let span = (b.at - a.at).max(f32::EPSILON);
                let k = ((t - a.at) / span).clamp(0.0, 1.0);
                let mix = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * k).round() as u8;
                return [
                    mix(a.rgba[0], b.rgba[0]),
                    mix(a.rgba[1], b.rgba[1]),
                    mix(a.rgba[2], b.rgba[2]),
                    mix(a.rgba[3], b.rgba[3]),
                ];
            }
        }
        self.stops[self.stops.len() - 1].rgba
    }
}

/// A channel's paints, cached to `paints.json` beside its emotes and badges.
///
/// `asked` deliberately includes users found to have NO paint. Most chatters
/// don't have one, so without it every reopen would re-ask 7TV about every
/// unpainted regular in the channel — the misses are the bulk of the answer
/// and the part most worth remembering.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PaintCache {
    pub fetched_at: i64,
    pub asked: Vec<String>,
    pub paints: HashMap<String, Paint>,
}

impl PaintCache {
    /// Read a channel's cache. Empty (not an error) when absent, unreadable,
    /// or past [`PAINTS_TTL_SECS`] — paints are decoration and a stale or
    /// missing file must never be load-bearing.
    pub fn load(name: &str, account: &str) -> PaintCache {
        for dir in crate::assets::asset_read_dirs(name, crate::models::Platform::Twitch, account) {
            let Ok(s) = crate::iomon::fs::read_to_string_sync(
                crate::iomon::Cat::AssetCache,
                dir.join("paints.json"),
            ) else {
                continue;
            };
            if let Ok(c) = serde_json::from_str::<PaintCache>(&s)
                && crate::models::now_unix() - c.fetched_at < PAINTS_TTL_SECS
            {
                return c;
            }
        }
        PaintCache::default()
    }

    pub fn save(&self, name: &str, account: &str) {
        let dir = crate::assets::channel_asset_dir(name, crate::models::Platform::Twitch, account);
        if crate::iomon::fs::create_dir_all_sync(crate::iomon::Cat::AssetCache, &dir).is_err() {
            return;
        }
        if let Ok(json) = serde_json::to_vec_pretty(self) {
            let _ = crate::iomon::fs::write_sync(
                crate::iomon::Cat::AssetCache,
                dir.join("paints.json"),
                json,
            );
        }
    }
}

fn parse_hex(s: &str) -> Option<[u8; 4]> {
    let h = s.trim_start_matches('#');
    if h.len() != 8 && h.len() != 6 {
        return None;
    }
    let b = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    Some([b(0)?, b(2)?, b(4)?, if h.len() == 8 { b(6)? } else { 255 }])
}

/// Reduce one `activePaint` node to a drawable [`Paint`].
///
/// Takes the first layer that yields usable stops. Multi-layer paints stack
/// translucent fills, which a per-run flat colour cannot express; the base
/// layer is the one that carries the identity.
pub fn parse_paint(node: &serde_json::Value) -> Option<Paint> {
    let name = node["name"].as_str().unwrap_or_default().to_string();
    for layer in node["data"]["layers"].as_array()? {
        let ty = &layer["ty"];
        let mut angle = 90; // horizontal by default
        let stops: Vec<Stop> = match ty["__typename"].as_str().unwrap_or_default() {
            "PaintLayerTypeSingleColor" => {
                let c = parse_hex(ty["color"]["hex"].as_str().unwrap_or_default())?;
                vec![Stop { at: 0.0, rgba: c }, Stop { at: 1.0, rgba: c }]
            }
            "PaintLayerTypeLinearGradient" | "PaintLayerTypeRadialGradient" => {
                // A radial paint has no angle and no left-to-right reading;
                // treating it as a horizontal sweep of the same stops is the
                // closest a run-coloured layout gets.
                angle = ty["angle"].as_i64().unwrap_or(90) as i32;
                let mut s: Vec<Stop> = ty["stops"]
                    .as_array()?
                    .iter()
                    .filter_map(|st| {
                        Some(Stop {
                            at: st["at"].as_f64()? as f32,
                            rgba: parse_hex(st["color"]["hex"].as_str()?)?,
                        })
                    })
                    .collect();
                s.sort_by(|a, b| a.at.total_cmp(&b.at));
                s
            }
            // Image paints need a real fill; nothing to sample.
            _ => continue,
        };
        if !stops.is_empty() {
            return Some(Paint { name, stops, angle });
        }
    }
    None
}

/// The first message in a GraphQL response's top-level `errors` array, if
/// any — 7TV (like Twitch's own GQL) answers a genuine query error with an
/// ordinary HTTP 200 and `"data": null`, so this is the only way to tell
/// "nobody in this batch has a paint" apart from "the query itself failed".
fn gql_error(v: &serde_json::Value) -> Option<&str> {
    v["errors"].as_array().filter(|e| !e.is_empty())?[0]["message"].as_str().or(Some("?"))
}

/// Fetch the active paints for a batch of Twitch user ids.
///
/// Returns only the users who HAVE one; the caller records everything it asked
/// about so misses aren't re-requested. An error means we couldn't ask —
/// callers change nothing, exactly as with the Twitch GQL checks.
pub async fn fetch_paints(
    http: &reqwest::Client,
    twitch_ids: &[String],
) -> anyhow::Result<HashMap<String, Paint>> {
    const FIELDS: &str = "style { activePaint { id name data { layers { opacity ty { \
        __typename \
        ... on PaintLayerTypeSingleColor { color { hex } } \
        ... on PaintLayerTypeLinearGradient { angle repeating stops { at color { hex } } } \
        ... on PaintLayerTypeRadialGradient { repeating stops { at color { hex } } } \
        } } } } }";
    let mut out = HashMap::new();
    for chunk in twitch_ids.chunks(CHUNK) {
        // Twitch ids are numeric, so they're safe to embed; aliases can't
        // start with a digit, hence the `u` prefix.
        let subs: String = chunk
            .iter()
            .enumerate()
            .map(|(i, id)| {
                format!("u{i}: userByConnection(platform: TWITCH, platformId: \"{id}\") {{ {FIELDS} }} ")
            })
            .collect();
        let body = serde_json::json!({ "query": format!("{{ users {{ {subs}}} }}") });
        let resp = http.post("https://7tv.io/v4/gql").json(&body).send().await?;
        if !resp.status().is_success() {
            anyhow::bail!("7tv gql {}", resp.status());
        }
        let v: serde_json::Value = resp.json().await?;
        // A GraphQL error is still an HTTP 200 with `data: null` — anything
        // in `chunk` would then read back as "asked, no paint" and get
        // negative-cached for `PAINTS_TTL_SECS` (24h) by the caller, even
        // though nobody in this batch was actually checked. Bailing here
        // instead means the caller's `asked` set is untouched and the next
        // sweep retries the same chunk.
        if let Some(e) = gql_error(&v) {
            anyhow::bail!("7tv gql: {e}");
        }
        for (i, id) in chunk.iter().enumerate() {
            let node = &v["data"]["users"][format!("u{i}")]["style"]["activePaint"];
            if let Some(p) = parse_paint(node) {
                out.insert(id.clone(), p);
            }
        }
    }
    Ok(out)
}

/// Whether gradient usernames render. Default on.
pub fn render_paints(store: &crate::store::Store) -> bool {
    store.get_setting(K_RENDER_PAINTS).ok().flatten().as_deref() != Some("0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The real shape off 7TV's v4 endpoint (LaynaLazar's "Bloody Mary",
    /// 2026-08-08).
    #[test]
    fn parses_a_real_linear_gradient_paint() {
        let node: serde_json::Value = serde_json::from_str(
            r##"{"id":"01H","name":"Bloody Mary","data":{"layers":[{"opacity":1.0,"ty":{
                "__typename":"PaintLayerTypeLinearGradient","angle":180,"repeating":false,
                "stops":[{"at":0.2,"color":{"hex":"#FFD1ECFF"}},
                         {"at":0.54,"color":{"hex":"#E45E5EFF"}},
                         {"at":0.76,"color":{"hex":"#D4022CFF"}},
                         {"at":1.0,"color":{"hex":"#8A0037FF"}}]}}]}}"##,
        )
        .unwrap();
        let p = parse_paint(&node).expect("parses");
        assert_eq!(p.name, "Bloody Mary");
        assert_eq!(p.angle, 180);
        assert_eq!(p.stops.len(), 4);
        assert_eq!(p.stops[0], Stop { at: 0.2, rgba: [0xFF, 0xD1, 0xEC, 0xFF] });
        assert_eq!(p.stops[3].rgba, [0x8A, 0x00, 0x37, 0xFF]);
    }

    #[test]
    fn a_single_colour_paint_becomes_two_identical_stops() {
        let node: serde_json::Value = serde_json::from_str(
            r##"{"name":"Flat","data":{"layers":[{"ty":{
                "__typename":"PaintLayerTypeSingleColor","color":{"hex":"#12345678"}}}]}}"##,
        )
        .unwrap();
        let p = parse_paint(&node).unwrap();
        assert_eq!(p.stops.len(), 2);
        // Sampling anywhere gives the same colour, with no special case.
        assert_eq!(p.sample(0.0), [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(p.sample(0.5), [0x12, 0x34, 0x56, 0x78]);
        assert_eq!(p.sample(1.0), [0x12, 0x34, 0x56, 0x78]);
    }

    /// Anything we can't draw yields no paint at all rather than a wrong one:
    /// the name then renders in its normal colour, which is the correct
    /// fallback.
    #[test]
    fn undrawable_and_malformed_paints_yield_nothing() {
        let img: serde_json::Value = serde_json::from_str(
            r##"{"name":"Pic","data":{"layers":[{"ty":{"__typename":"PaintLayerTypeImage"}}]}}"##,
        )
        .unwrap();
        assert_eq!(parse_paint(&img), None);
        assert_eq!(parse_paint(&serde_json::Value::Null), None);
        assert_eq!(parse_paint(&serde_json::json!({})), None);
        assert_eq!(parse_paint(&serde_json::json!({"data": {"layers": []}})), None);
        // A gradient whose stops are all unparseable is not a gradient.
        let bad: serde_json::Value = serde_json::from_str(
            r##"{"name":"x","data":{"layers":[{"ty":{"__typename":"PaintLayerTypeLinearGradient",
               "angle":90,"stops":[{"at":0.0,"color":{"hex":"nope"}}]}}]}}"##,
        )
        .unwrap();
        assert_eq!(parse_paint(&bad), None);
    }

    /// 7TV answers a genuine query error with an ordinary HTTP 200 and
    /// `"data": null` — the real shape returned for a malformed query
    /// (verified live against `7tv.io/v4/gql`, 2026-08-10). Without checking
    /// for this, every id in that batch reads back as "no paint" and gets
    /// negative-cached for `PAINTS_TTL_SECS`, silently hiding a real paint
    /// for up to a day.
    #[test]
    fn gql_error_finds_a_top_level_graphql_error() {
        let v: serde_json::Value = serde_json::from_str(
            r##"{"data":null,"errors":[{"message":"Unknown field \"idz\" on type \"Paint\".",
                "locations":[{"line":1,"column":91}]}]}"##,
        )
        .unwrap();
        assert_eq!(gql_error(&v), Some("Unknown field \"idz\" on type \"Paint\"."));
    }

    #[test]
    fn gql_error_is_none_for_an_ordinary_successful_response() {
        let v: serde_json::Value =
            serde_json::from_str(r##"{"data":{"users":{"u0":null}}}"##).unwrap();
        assert_eq!(gql_error(&v), None);
        // An empty `errors` array (some servers always include the key) is
        // not an error either.
        let v: serde_json::Value =
            serde_json::from_str(r##"{"data":{},"errors":[]}"##).unwrap();
        assert_eq!(gql_error(&v), None);
    }

    #[test]
    fn hex_parsing_handles_both_lengths_and_rejects_junk() {
        assert_eq!(parse_hex("#FFD1ECFF"), Some([0xFF, 0xD1, 0xEC, 0xFF]));
        assert_eq!(parse_hex("112233"), Some([0x11, 0x22, 0x33, 0xFF]));
        assert_eq!(parse_hex("#xyzxyz"), None);
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#FFF"), None);
    }

    /// Only the horizontal component of the angle survives — a vertical
    /// gradient collapses to its midpoint rather than rendering sideways.
    #[test]
    fn sampling_projects_the_angle_onto_the_horizontal() {
        let p = Paint {
            name: "g".into(),
            angle: 90, // fully horizontal, left to right
            stops: vec![
                Stop { at: 0.0, rgba: [0, 0, 0, 255] },
                Stop { at: 1.0, rgba: [100, 100, 100, 255] },
            ],
        };
        assert_eq!(p.sample(0.0), [0, 0, 0, 255]);
        assert_eq!(p.sample(0.5), [50, 50, 50, 255]);
        assert_eq!(p.sample(1.0), [100, 100, 100, 255]);

        // 270° is the same axis, reversed.
        let rev = Paint { angle: 270, ..p.clone() };
        assert_eq!(rev.sample(0.0), [100, 100, 100, 255]);
        assert_eq!(rev.sample(1.0), [0, 0, 0, 255]);

        // 180° is vertical: every position samples the middle, so the name
        // reads as one colour rather than a gradient pointing the wrong way.
        let vert = Paint { angle: 180, ..p };
        assert_eq!(vert.sample(0.0), vert.sample(1.0));
        assert_eq!(vert.sample(0.0), [50, 50, 50, 255]);
    }

    #[test]
    fn sampling_clamps_outside_the_stop_range() {
        let p = Paint {
            name: "g".into(),
            angle: 90,
            stops: vec![
                Stop { at: 0.4, rgba: [10, 10, 10, 255] },
                Stop { at: 0.6, rgba: [20, 20, 20, 255] },
            ],
        };
        assert_eq!(p.sample(0.0), [10, 10, 10, 255], "before the first stop");
        assert_eq!(p.sample(1.0), [20, 20, 20, 255], "after the last");
        assert_eq!(p.sample(0.5), [15, 15, 15, 255]);
    }
}
