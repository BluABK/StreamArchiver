//! Hype-train tuning: the weights/thresholds behind the chat-side inference
//! (`chat::EventTracker::note_contribution`), their persistence, and the
//! auto-tune primitives that calibrate them against ground truth (GQL-confirmed
//! trains, manual marks, and false-positive deletions).
//!
//! Twitch's real kickoff rules are streamer-configurable and opaque (the GQL
//! `config` field is auth-blocked), so the inference never tries to replicate
//! them — it flags "train-like bursts", and every confirmed/missed/false
//! sample nudges the thresholds toward what this user's channels actually do.

use crate::store::Store;

/// Settings key for the global [`HypeTuning`] JSON blob.
pub const K_HYPE_TUNING: &str = "hype_tuning";
/// Settings key for the per-channel overrides map
/// (`channel_id -> HypeOverride`, JSON).
pub const K_HYPE_OVERRIDES: &str = "hype_tuning_overrides";
/// Settings key for the "confirm trains via anonymous Twitch GQL" toggle
/// (default on; `"0"` = off).
pub const K_HYPE_GQL: &str = "hype_gql";

/// Global inference weights + thresholds, persisted as one JSON blob under
/// [`K_HYPE_TUNING`]. Every contribution the chat logger sees (sub / resub /
/// gift / bits / Hype Chat) is scored in points; a burst becomes an inferred
/// `hype_train` event when ALL enabled gates pass within `window_secs`.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HypeTuning {
    /// Sliding window the burst must fit in (Twitch's train timer is 5 min).
    pub window_secs: i64,
    /// Minimum summed points in the window (0 disables the points gate).
    pub min_points: i64,
    /// Minimum number of contributions in the window.
    pub min_events: i64,
    /// Minimum distinct contributors (a single whale isn't a train).
    pub min_actors: i64,
    /// Points per bit cheered.
    pub w_bits: i64,
    /// Points per tier-1 sub/resub (tier 2 counts double, tier 3 ×5).
    pub w_sub: i64,
    /// Points per gifted sub.
    pub w_gift: i64,
    /// Points per currency minor unit (cent) of a Hype Chat.
    pub w_dono: i64,
    /// Let confirmed/missed/false samples adjust the thresholds automatically.
    pub auto_tune: bool,
    /// Newest-first human-readable audit trail of auto-tune adjustments
    /// (capped at [`TUNE_LOG_CAP`]) — shown in Settings.
    pub log: Vec<String>,
}

impl Default for HypeTuning {
    fn default() -> Self {
        // Looser than the original hardcoded 5-events/3-actors inference (the
        // complaint that prompted this was under-detection); point weights
        // mirror Twitch's long-known conversion rates (1 pt/bit, 500 pts per
        // tier-1 sub or gift).
        Self {
            window_secs: 300,
            min_points: 1000,
            min_events: 3,
            min_actors: 2,
            w_bits: 1,
            w_sub: 500,
            w_gift: 500,
            w_dono: 1,
            auto_tune: true,
            log: Vec::new(),
        }
    }
}

/// Per-channel threshold overrides (sparse — `None` = use the global value).
/// Weights and the window stay global: they model Twitch's economy, not a
/// channel's size.
#[derive(Clone, Copy, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct HypeOverride {
    pub min_points: Option<i64>,
    pub min_events: Option<i64>,
    pub min_actors: Option<i64>,
}

impl HypeOverride {
    pub fn is_empty(&self) -> bool {
        self.min_points.is_none() && self.min_events.is_none() && self.min_actors.is_none()
    }
}

/// Auto-tune audit-log cap (oldest lines dropped past this).
const TUNE_LOG_CAP: usize = 20;

/// Load the global tuning blob (defaults when unset/corrupt).
pub fn load_tuning(store: &Store) -> HypeTuning {
    store
        .get_setting(K_HYPE_TUNING)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Persist the global tuning blob (best-effort).
pub fn save_tuning(store: &Store, t: &HypeTuning) {
    if let Ok(json) = serde_json::to_string(t) {
        let _ = store.set_setting(K_HYPE_TUNING, &json);
    }
}

/// Load the per-channel overrides map.
pub fn load_overrides(store: &Store) -> std::collections::HashMap<i64, HypeOverride> {
    store
        .get_setting(K_HYPE_OVERRIDES)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Set (or clear, when empty) one channel's override.
pub fn save_override(store: &Store, channel_id: i64, ov: HypeOverride) {
    let mut map = load_overrides(store);
    if ov.is_empty() {
        map.remove(&channel_id);
    } else {
        map.insert(channel_id, ov);
    }
    if let Ok(json) = serde_json::to_string(&map) {
        let _ = store.set_setting(K_HYPE_OVERRIDES, &json);
    }
}

/// Global tuning with `channel_id`'s override applied (the values the
/// inference should actually run with for that channel).
pub fn load_effective(store: &Store, channel_id: i64) -> HypeTuning {
    let mut t = load_tuning(store);
    if let Some(ov) = load_overrides(store).get(&channel_id) {
        if let Some(v) = ov.min_points {
            t.min_points = v;
        }
        if let Some(v) = ov.min_events {
            t.min_events = v;
        }
        if let Some(v) = ov.min_actors {
            t.min_actors = v;
        }
    }
    t
}

/// Whether GQL train confirmation is enabled (default on).
pub fn gql_enabled(store: &Store) -> bool {
    store
        .get_setting(K_HYPE_GQL)
        .ok()
        .flatten()
        .map(|v| v != "0")
        .unwrap_or(true)
}

/// Score one contribution in hype points. `kind` is a `stream_event` kind
/// (only the contribution kinds score; everything else is 0), `amount` its
/// stored amount (bits, gift-batch size, dono minor units), `tier` the Twitch
/// sub plan (`1000`/`2000`/`3000`/`Prime`).
pub fn contribution_points(kind: &str, amount: i64, tier: &str, t: &HypeTuning) -> i64 {
    match kind {
        "sub" | "resub" => t.w_sub * tier_multiplier(tier),
        "subgift" => t.w_gift * amount.max(1),
        "bits" => t.w_bits * amount.max(0),
        "dono" => t.w_dono * amount.max(0),
        _ => 0,
    }
}

/// Tier-2 subs count double, tier-3 ×5 (Twitch's own point ratios); Prime and
/// unknown plans count as tier 1.
fn tier_multiplier(tier: &str) -> i64 {
    match tier {
        "2000" => 2,
        "3000" => 5,
        _ => 1,
    }
}

/// Score the STORED contributions for `monitor_id` in `[from, to)`:
/// `(points, events, distinct actors)`. Used to retro-analyze the run-up to a
/// confirmed/manual train and to size false-positive bursts.
pub fn observed_burst(
    store: &Store,
    monitor_id: i64,
    from: i64,
    to: i64,
    t: &HypeTuning,
) -> (i64, i64, i64) {
    let rows = store
        .contribution_events_range(monitor_id, from, to)
        .unwrap_or_default();
    let mut points = 0i64;
    let mut actors = std::collections::HashSet::new();
    for (kind, actor, amount, tier) in &rows {
        points += contribution_points(kind, *amount, tier, t);
        if !actor.is_empty() {
            actors.insert(actor.to_lowercase());
        }
    }
    (points, rows.len() as i64, actors.len() as i64)
}

/// A real train the inference missed: loosen only the gate(s) that were the
/// blocker, down toward what was actually observed (never below the floors —
/// 200 pts / 2 events / 1 actor). No-op with auto-tune off or no observed
/// contributions. `why` names the ground-truth source for the log
/// ("confirmed train" / "manual mark").
pub fn loosen_for_missed(store: &Store, observed: (i64, i64, i64), why: &str) {
    let mut t = load_tuning(store);
    if !t.auto_tune {
        return;
    }
    let (pts, events, actors) = observed;
    if events == 0 {
        return; // nothing was stored — no signal to tune from
    }
    let mut changes = Vec::new();
    if t.min_points > 0 && pts < t.min_points {
        let new = (pts * 9 / 10).max(200);
        changes.push(format!("min points {} → {new}", t.min_points));
        t.min_points = new;
    }
    if events < t.min_events {
        let new = events.max(2);
        changes.push(format!("min events {} → {new}", t.min_events));
        t.min_events = new;
    }
    if actors < t.min_actors {
        let new = actors.max(1);
        changes.push(format!("min actors {} → {new}", t.min_actors));
        t.min_actors = new;
    }
    if changes.is_empty() {
        return; // gates already pass — the miss was timing, not thresholds
    }
    push_log(
        &mut t,
        &format!(
            "loosened after {why} the inference missed ({pts} pts / {events} events / {actors} chatters): {}",
            changes.join(", ")
        ),
    );
    save_tuning(store, &t);
}

/// An inferred burst turned out NOT to be a train (GQL says no train ran /
/// the user deleted it): tighten just past that burst's size, capped at
/// 10 000 pts / 10 events so one enormous non-train can't disable the
/// inference. No-op with auto-tune off.
pub fn tighten_for_false(store: &Store, observed_points: i64, observed_events: i64, why: &str) {
    let mut t = load_tuning(store);
    if !t.auto_tune {
        return;
    }
    let mut changes = Vec::new();
    if t.min_points > 0 && observed_points >= t.min_points {
        let new = (observed_points * 11 / 10).min(10_000).max(t.min_points);
        if new != t.min_points {
            changes.push(format!("min points {} → {new}", t.min_points));
            t.min_points = new;
        }
    }
    if observed_events >= t.min_events {
        let new = (observed_events + 1).min(10);
        if new != t.min_events {
            changes.push(format!("min events {} → {new}", t.min_events));
            t.min_events = new;
        }
    }
    if changes.is_empty() {
        return;
    }
    push_log(
        &mut t,
        &format!(
            "tightened after {why} ({observed_points} pts / {observed_events} events): {}",
            changes.join(", ")
        ),
    );
    save_tuning(store, &t);
}

/// Prepend a timestamped line to the audit log, dropping past the cap.
fn push_log(t: &mut HypeTuning, line: &str) {
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M");
    t.log.insert(0, format!("{ts} — {line}"));
    t.log.truncate(TUNE_LOG_CAP);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem_store() -> Store {
        Store::open_in_memory().expect("store")
    }

    #[test]
    fn tuning_roundtrip_and_defaults() {
        let store = mem_store();
        let d = load_tuning(&store);
        assert_eq!(d, HypeTuning::default());
        let mut t = d.clone();
        t.min_points = 750;
        t.w_bits = 2;
        save_tuning(&store, &t);
        assert_eq!(load_tuning(&store), t);
        // Corrupt blob falls back to defaults.
        store.set_setting(K_HYPE_TUNING, "{nope").unwrap();
        assert_eq!(load_tuning(&store), HypeTuning::default());
    }

    #[test]
    fn override_merge() {
        let store = mem_store();
        save_override(
            &store,
            7,
            HypeOverride { min_points: Some(400), min_events: None, min_actors: Some(1) },
        );
        let eff = load_effective(&store, 7);
        assert_eq!(eff.min_points, 400);
        assert_eq!(eff.min_events, HypeTuning::default().min_events);
        assert_eq!(eff.min_actors, 1);
        // Other channels stay global.
        assert_eq!(load_effective(&store, 8), HypeTuning::default());
        // Clearing removes the map entry.
        save_override(&store, 7, HypeOverride::default());
        assert!(load_overrides(&store).is_empty());
    }

    #[test]
    fn points_and_tiers() {
        let t = HypeTuning::default();
        assert_eq!(contribution_points("sub", 0, "1000", &t), 500);
        assert_eq!(contribution_points("resub", 14, "2000", &t), 1000);
        assert_eq!(contribution_points("sub", 0, "3000", &t), 2500);
        assert_eq!(contribution_points("sub", 0, "Prime", &t), 500);
        assert_eq!(contribution_points("subgift", 5, "1000", &t), 2500);
        assert_eq!(contribution_points("bits", 300, "", &t), 300);
        assert_eq!(contribution_points("dono", 500, "USD", &t), 500);
        assert_eq!(contribution_points("first_chat", 1, "", &t), 0);
    }

    #[test]
    fn loosen_only_blocking_gates_with_floors() {
        let store = mem_store();
        // Observed burst: 150 pts, 2 events, 2 actors vs defaults 1000/3/2.
        loosen_for_missed(&store, (150, 2, 2), "manual mark");
        let t = load_tuning(&store);
        assert_eq!(t.min_points, 200); // 135 clamped up to the floor
        assert_eq!(t.min_events, 2);
        assert_eq!(t.min_actors, 2); // was not a blocker — untouched
        assert_eq!(t.log.len(), 1);
        assert!(t.log[0].contains("manual mark"), "{}", t.log[0]);
    }

    #[test]
    fn tighten_caps_and_autotune_off() {
        let store = mem_store();
        tighten_for_false(&store, 50_000, 30, "deletion");
        let t = load_tuning(&store);
        assert_eq!(t.min_points, 10_000);
        assert_eq!(t.min_events, 10);
        // With auto-tune off both primitives are inert.
        let mut off = t.clone();
        off.auto_tune = false;
        save_tuning(&store, &off);
        loosen_for_missed(&store, (0, 1, 1), "confirmed train");
        tighten_for_false(&store, 99_999, 99, "deletion");
        assert_eq!(load_tuning(&store), off);
    }
}
