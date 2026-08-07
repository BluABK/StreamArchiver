//! Simulcast dedup: when one channel is live on several platforms at once,
//! record only the preferred one.
//!
//! A channel here is a container of **instances** (monitors) — the same
//! streamer on Twitch *and* YouTube. When they simulcast, both instances go
//! live and both record, producing two copies of one broadcast at double the
//! disk and I/O, neither better than the other. The extra instances exist as
//! redundancy, not as duplicate archives.
//!
//! Two rules keep this from ever costing a broadcast:
//!
//! * **Exclusives still record.** If nothing is live on the preferred platform,
//!   whatever *is* live records as normal — a platform-exclusive stream is
//!   never skipped waiting for a channel that isn't broadcasting.
//! * **The others stay armed.** A held-back instance ("standing by") keeps
//!   being polled, and takes over if the preferred one is live but never gets a
//!   capture going — Auto off there, repeated errors, or a capture that died
//!   mid-stream. That's [`SETTLE_SECS_DEFAULT`]'s job.
//!
//! Distinct from [`crate::platform_pref`], which answers a *display* question
//! ("which instance's title/viewers drive the rolled-up channel row"). This one
//! decides what gets written to disk. They're deliberately separate settings:
//! you may well want Twitch's richer metadata on the row while recording
//! YouTube's cleaner video.
//!
//! Everything in here is a pure function over a snapshot ([`decide`]); the
//! supervisor gathers the snapshot and acts on the verdict. That's what makes
//! the policy testable without a `Supervisor`.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use crate::models::Platform;
use crate::store::Store;

// ---------- settings keys ----------

/// Global preferred recording platform (`off` = record every live instance).
pub const K_SIMULCAST_PREF: &str = "simulcast_pref";
/// Global ad-free override platform (`off` = no override).
pub const K_SIMULCAST_AD_FREE_PREF: &str = "simulcast_ad_free_pref";
/// Global settle window in seconds — see [`SETTLE_SECS_DEFAULT`].
pub const K_SIMULCAST_SETTLE_SECS: &str = "simulcast_settle_secs";
/// Per-channel scope-config map (`{channel_id -> SimulcastScope}`).
pub const K_CHANNEL_SIMULCAST_SCOPE: &str = "channel_simulcast_scope";
/// Per-monitor scope-config map (`{monitor_id -> SimulcastScope}`).
pub const K_MONITOR_SIMULCAST_SCOPE: &str = "monitor_simulcast_scope";

/// How long one broadcast stays "unsettled", in seconds.
///
/// One number, two jobs, because both are the same idea — *we're still working
/// out which source is best for this broadcast*:
///
/// * how long a standby instance waits for the preferred instance to actually
///   start capturing before taking over, and
/// * how young a non-preferred capture has to be for the preferred instance to
///   take it over ([`SimulcastDecision::Takeover`]).
///
/// Three minutes is about three poll attempts at the default cadence, and
/// outlasts a transient retry without stranding a genuinely broken instance.
pub const SETTLE_SECS_DEFAULT: i64 = 180;

// ---------- the setting ----------

/// Which platform a tier prefers, or `Off`.
///
/// `Off` means different things in the two fields it's used for, which is why
/// the label is passed in by the UI rather than baked in here: for `pref` it's
/// "record every live instance" (feature off), for `ad_free_pref` it's "no
/// ad-free override".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SimulcastPref {
    #[default]
    Off,
    Twitch,
    YouTube,
    Kick,
}

impl SimulcastPref {
    pub const ALL: [SimulcastPref; 4] =
        [SimulcastPref::Off, SimulcastPref::Twitch, SimulcastPref::YouTube, SimulcastPref::Kick];

    pub fn as_str(self) -> &'static str {
        match self {
            SimulcastPref::Off => "off",
            SimulcastPref::Twitch => "twitch",
            SimulcastPref::YouTube => "youtube",
            SimulcastPref::Kick => "kick",
        }
    }

    pub fn parse(s: &str) -> Option<SimulcastPref> {
        match s.trim() {
            "off" | "" => Some(SimulcastPref::Off),
            "twitch" => Some(SimulcastPref::Twitch),
            "youtube" => Some(SimulcastPref::YouTube),
            "kick" => Some(SimulcastPref::Kick),
            _ => None,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SimulcastPref::Off => "Off",
            SimulcastPref::Twitch => "Twitch",
            SimulcastPref::YouTube => "YouTube",
            SimulcastPref::Kick => "Kick",
        }
    }

    /// The platform this names, or `None` for `Off`.
    pub fn platform(self) -> Option<Platform> {
        match self {
            SimulcastPref::Off => None,
            SimulcastPref::Twitch => Some(Platform::Twitch),
            SimulcastPref::YouTube => Some(Platform::YouTube),
            SimulcastPref::Kick => Some(Platform::Kick),
        }
    }
}

/// A channel's or instance's override of the global setting. Both fields are
/// `None` = inherit; an all-`None` scope is deleted rather than stored (see
/// [`Self::is_inherit`]).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SimulcastScope {
    #[serde(default)]
    pub pref: Option<SimulcastPref>,
    #[serde(default)]
    pub ad_free_pref: Option<SimulcastPref>,
}

impl SimulcastScope {
    pub fn is_inherit(&self) -> bool {
        self.pref.is_none() && self.ad_free_pref.is_none()
    }
}

// ---------- scope storage ----------

fn load_scope_map(store: &Store, key: &str) -> HashMap<String, SimulcastScope> {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .filter(|s| !s.trim().is_empty())
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_scope(store: &Store, key: &str, id: i64, cfg: &SimulcastScope) -> anyhow::Result<()> {
    let mut map = load_scope_map(store, key);
    if cfg.is_inherit() {
        map.remove(&id.to_string());
    } else {
        map.insert(id.to_string(), cfg.clone());
    }
    store.set_setting(key, &serde_json::to_string(&map)?)?;
    Ok(())
}

pub fn load_channel_simulcast_scope(store: &Store, channel_id: i64) -> SimulcastScope {
    load_scope_map(store, K_CHANNEL_SIMULCAST_SCOPE)
        .remove(&channel_id.to_string())
        .unwrap_or_default()
}

pub fn save_channel_simulcast_scope(
    store: &Store,
    channel_id: i64,
    cfg: &SimulcastScope,
) -> anyhow::Result<()> {
    save_scope(store, K_CHANNEL_SIMULCAST_SCOPE, channel_id, cfg)
}

pub fn load_monitor_simulcast_scope(store: &Store, monitor_id: i64) -> SimulcastScope {
    load_scope_map(store, K_MONITOR_SIMULCAST_SCOPE)
        .remove(&monitor_id.to_string())
        .unwrap_or_default()
}

pub fn save_monitor_simulcast_scope(
    store: &Store,
    monitor_id: i64,
    cfg: &SimulcastScope,
) -> anyhow::Result<()> {
    save_scope(store, K_MONITOR_SIMULCAST_SCOPE, monitor_id, cfg)
}

// ---------- global readers + effective resolution ----------

fn global_pref_key(store: &Store, key: &str) -> SimulcastPref {
    store
        .get_setting(key)
        .ok()
        .flatten()
        .and_then(|s| SimulcastPref::parse(&s))
        .unwrap_or_default()
}

pub fn global_pref(store: &Store) -> SimulcastPref {
    global_pref_key(store, K_SIMULCAST_PREF)
}

pub fn global_ad_free_pref(store: &Store) -> SimulcastPref {
    global_pref_key(store, K_SIMULCAST_AD_FREE_PREF)
}

/// The settle window in seconds. A non-positive or unparseable value falls back
/// to [`SETTLE_SECS_DEFAULT`] — a 0 there is far likelier to be an empty
/// settings field than a request to hand every broadcast to the first instance
/// that polls.
pub fn global_settle_secs(store: &Store) -> i64 {
    store
        .get_setting(K_SIMULCAST_SETTLE_SECS)
        .ok()
        .flatten()
        .and_then(|s| s.trim().parse::<i64>().ok())
        .filter(|v| *v > 0)
        .unwrap_or(SETTLE_SECS_DEFAULT)
}

/// Monitor override over channel override over the global default.
pub fn effective_pref_from(
    global: SimulcastPref,
    channel_scope: Option<&SimulcastScope>,
    monitor_scope: Option<&SimulcastScope>,
) -> SimulcastPref {
    monitor_scope
        .and_then(|s| s.pref)
        .or_else(|| channel_scope.and_then(|s| s.pref))
        .unwrap_or(global)
}

/// Same chain, resolved **independently** of [`effective_pref_from`] — a
/// channel can pick the everyday platform while one instance overrides only the
/// ad-free rule, or vice versa.
pub fn effective_ad_free_pref_from(
    global: SimulcastPref,
    channel_scope: Option<&SimulcastScope>,
    monitor_scope: Option<&SimulcastScope>,
) -> SimulcastPref {
    monitor_scope
        .and_then(|s| s.ad_free_pref)
        .or_else(|| channel_scope.and_then(|s| s.ad_free_pref))
        .unwrap_or(global)
}

/// One instance's fully-resolved policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SimulcastPolicy {
    /// The everyday preferred platform (`None` = dedup off for this instance).
    pub pref: Option<Platform>,
    /// Preferred platform *when an instance on it is ad-free* (`None` = no
    /// override).
    pub ad_free_pref: Option<Platform>,
}

/// A one-shot snapshot of every tier, so resolving a whole channel's worth of
/// instances costs three `get_setting` calls and two JSON parses instead of
/// five store hits per instance.
///
/// Deciding needs **every sibling's** policy, not just the candidate's (see the
/// fail-open rule in [`decide`]), which is exactly why this exists. Same shape
/// as [`crate::platform_pref::PlatformPrefCtx`].
pub struct SimulcastCtx {
    global_pref: SimulcastPref,
    global_ad_free_pref: SimulcastPref,
    channel_scopes: HashMap<String, SimulcastScope>,
    monitor_scopes: HashMap<String, SimulcastScope>,
    pub settle_secs: i64,
}

impl SimulcastCtx {
    pub fn load(store: &Store) -> SimulcastCtx {
        SimulcastCtx {
            global_pref: global_pref(store),
            global_ad_free_pref: global_ad_free_pref(store),
            channel_scopes: load_scope_map(store, K_CHANNEL_SIMULCAST_SCOPE),
            monitor_scopes: load_scope_map(store, K_MONITOR_SIMULCAST_SCOPE),
            settle_secs: global_settle_secs(store),
        }
    }

    pub fn policy_for(&self, channel_id: i64, monitor_id: i64) -> SimulcastPolicy {
        let ch = self.channel_scopes.get(&channel_id.to_string());
        let mon = self.monitor_scopes.get(&monitor_id.to_string());
        SimulcastPolicy {
            pref: effective_pref_from(self.global_pref, ch, mon).platform(),
            ad_free_pref: effective_ad_free_pref_from(self.global_ad_free_pref, ch, mon).platform(),
        }
    }
}

/// Prefix stamped on a standby take's
/// [`not_recorded_reason`](crate::models::Recording::not_recorded_reason).
pub const SKIP_REASON_PREFIX: &str = "simulcast:";

/// Whether a `not_recorded` take was skipped by simulcast dedup — i.e. a
/// sibling instance recorded this broadcast, so nothing was actually missed.
///
/// The VOD-recovery paths ask this before treating a 👁 row as a gap worth
/// downloading; without it, declining a duplicate at capture time would just
/// re-fetch it hours later.
pub fn is_simulcast_skip(not_recorded_reason: &str) -> bool {
    not_recorded_reason.starts_with(SKIP_REASON_PREFIX)
}

// ---------- the decision ----------

/// Everything [`decide`] knows about one instance of a channel. Assembled by
/// the supervisor from the monitor row plus its own in-flight state; nothing in
/// here reads the clock or the database, so the policy stays testable.
#[derive(Clone, Debug)]
pub struct InstanceState {
    pub monitor_id: i64,
    pub platform: Platform,
    /// A capture is running (the supervisor's `active` set).
    pub capturing: bool,
    /// Capture ended, still muxing/promoting — it still holds this broadcast.
    pub finalizing: bool,
    /// `monitor.last_state == "live"`.
    pub live_state: bool,
    /// `monitor.last_live_since`.
    pub live_since: Option<i64>,
    /// When this instance's last take ended (`last_recording_ended`).
    pub last_take_ended: Option<i64>,
    /// The open take's `started_at`, when `capturing`.
    pub take_started_at: Option<i64>,
    /// Master dormancy switch (channel AND instance).
    pub automation_on: bool,
    /// Auto-record flag (channel AND instance).
    pub auto_record_on: bool,
    /// Detection method is `Disabled` — never polled automatically.
    pub detection_disabled: bool,
    /// A manual Stop hold is suppressing restarts here.
    pub stop_held: bool,
    /// Captures from this instance have no ad-break cuts: the manual `ad_free`
    /// flag, or a detected Twitch subscription.
    pub ad_free: bool,
    pub policy: SimulcastPolicy,
}

impl InstanceState {
    /// Whether this instance can be counted on to hold this broadcast.
    ///
    /// Deliberately stricter than the UI's liveness test
    /// (`active || last_state == "live"`): the scheduler skips dormant and
    /// `Disabled`-detection monitors entirely, so their `last_state` can sit at
    /// a stale `"live"` indefinitely. Trusting that here would strand a live
    /// sibling forever behind an instance that is never going to record.
    fn eligible_live(&self) -> bool {
        self.capturing
            || self.finalizing
            || (self.live_state && self.automation_on && !self.detection_disabled)
    }

    /// The moment this instance last had a chance to get a capture going: it
    /// went live, or its previous take ended (whichever is later).
    ///
    /// The `last_take_ended` half is what stops a routine reconnect from
    /// handing the broadcast away — a winner whose capture just dropped gets a
    /// fresh settle window rather than being written off.
    fn last_chance_at(&self) -> i64 {
        self.live_since.unwrap_or(i64::MIN).max(self.last_take_ended.unwrap_or(i64::MIN))
    }
}

/// What the supervisor should do with a start request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SimulcastDecision {
    /// Record normally.
    Record,
    /// Don't record: another instance of this channel has this broadcast.
    /// Stays armed — the next poll re-decides.
    Standby { winner: i64, winner_platform: Platform },
    /// Record, and stop these captures first: this instance is the preferred
    /// source and they're duplicates of it, young enough to be worth switching.
    Takeover { stop: Vec<i64> },
}

/// The whole policy, as one pure function.
///
/// `all` is every instance of the candidate's channel, including the candidate
/// itself (the ad-free rule may need to inspect it). `settle_secs` is
/// [`SimulcastCtx::settle_secs`].
pub fn decide(
    all: &[InstanceState],
    candidate_id: i64,
    now: i64,
    settle_secs: i64,
) -> SimulcastDecision {
    let Some(me) = all.iter().find(|s| s.monitor_id == candidate_id) else {
        return SimulcastDecision::Record;
    };
    let Some(pref) = effective_platform(&me.policy, all) else {
        return SimulcastDecision::Record; // dedup off for this instance
    };

    // This instance IS the preferred source. Polls are independently phased, so
    // a non-preferred sibling beating it to the broadcast is routine rather
    // than exceptional — hence two outcomes:
    //
    // * while the broadcast is still settling, take over: stop the duplicate
    //   and record here, since almost nothing has been captured yet and both
    //   Twitch head-backfill and YouTube live-from-start can recover the start.
    // * once that window has passed, the sibling's capture is the intact copy
    //   of this broadcast. Starting a second one now would produce exactly the
    //   duplicate this feature exists to prevent, with the *later* start —
    //   so stand by instead, still armed if that capture dies.
    if me.platform == pref {
        let mut young: Vec<i64> = Vec::new();
        let mut established: Option<&InstanceState> = None;
        for s in all.iter().filter(|s| {
            s.monitor_id != me.monitor_id && s.platform != pref && (s.capturing || s.finalizing)
        }) {
            if s.capturing && s.take_started_at.is_some_and(|t| t > now - settle_secs) {
                young.push(s.monitor_id);
            } else {
                established = Some(s);
            }
        }
        return match (young.is_empty(), established) {
            (false, _) => SimulcastDecision::Takeover { stop: young },
            (true, Some(s)) => {
                SimulcastDecision::Standby { winner: s.monitor_id, winner_platform: s.platform }
            }
            (true, None) => SimulcastDecision::Record,
        };
    }

    // Who would hold this broadcast instead? Earliest-live wins, matching the
    // channel-row rollup's tie-break.
    let winner = all
        .iter()
        .filter(|s| s.platform == pref && s.eligible_live())
        .min_by_key(|s| s.live_since.unwrap_or(i64::MAX));
    // Nobody: this is a platform exclusive (or the preferred instance is
    // offline/dormant). Record it — that's the never-lose-a-broadcast rule, and
    // it needs no branch of its own.
    let Some(winner) = winner else {
        return SimulcastDecision::Record;
    };

    // Fail open on a disagreement: if the winner's own resolved preference
    // isn't this platform, two instances could each stand by for the other and
    // the broadcast would be lost. An archiver errs on capturing.
    if effective_platform(&winner.policy, all) != Some(pref) {
        return SimulcastDecision::Record;
    }

    // Live but structurally unable to record here, and not already capturing:
    // waiting would be waiting forever.
    if !winner.capturing && !winner.finalizing && (!winner.auto_record_on || winner.stop_held) {
        return SimulcastDecision::Record;
    }

    let standby =
        SimulcastDecision::Standby { winner: winner.monitor_id, winner_platform: winner.platform };
    if winner.capturing || winner.finalizing {
        return standby; // it has the broadcast — stand by indefinitely
    }
    // Live, allowed to record, but nothing is running. Give it the settle
    // window to get going; after that, take the broadcast.
    if now.saturating_sub(winner.last_chance_at()) >= settle_secs {
        SimulcastDecision::Record
    } else {
        standby
    }
}

/// The platform a policy resolves to right now: the ad-free override when an
/// eligible-live instance on that platform really is ad-free, else the everyday
/// preference.
///
/// The status consulted is that of the instance **on the ad-free platform** —
/// the whole point is "prefer Twitch *when Twitch has no ad breaks for me*".
/// Requiring it to be eligible-live matters: a stale ad-free flag on an offline
/// instance must not redirect the preference onto a channel that isn't
/// broadcasting, stranding the one that is.
fn effective_platform(policy: &SimulcastPolicy, all: &[InstanceState]) -> Option<Platform> {
    if let Some(afp) = policy.ad_free_pref
        && all.iter().any(|s| s.platform == afp && s.ad_free && s.eligible_live())
    {
        return Some(afp);
    }
    policy.pref
}

#[cfg(test)]
mod tests {
    use super::*;

    const SETTLE: i64 = 180;
    const NOW: i64 = 10_000;

    fn state(id: i64, platform: Platform, pref: Option<Platform>) -> InstanceState {
        InstanceState {
            monitor_id: id,
            platform,
            capturing: false,
            finalizing: false,
            live_state: false,
            live_since: None,
            last_take_ended: None,
            take_started_at: None,
            automation_on: true,
            auto_record_on: true,
            detection_disabled: false,
            stop_held: false,
            ad_free: false,
            policy: SimulcastPolicy { pref, ad_free_pref: None },
        }
    }

    /// Live `secs` ago, nothing running.
    fn live(mut s: InstanceState, secs: i64) -> InstanceState {
        s.live_state = true;
        s.live_since = Some(NOW - secs);
        s
    }

    /// Live and capturing, take started `secs` ago.
    fn capturing(mut s: InstanceState, secs: i64) -> InstanceState {
        s.live_state = true;
        s.live_since = Some(NOW - secs);
        s.capturing = true;
        s.take_started_at = Some(NOW - secs);
        s
    }

    /// The usual pair: a YouTube-preferred channel with a Twitch and a YouTube
    /// instance (ids 1 and 2).
    fn pair() -> (InstanceState, InstanceState) {
        (
            state(1, Platform::Twitch, Some(Platform::YouTube)),
            state(2, Platform::YouTube, Some(Platform::YouTube)),
        )
    }

    #[test]
    fn feature_off_always_records() {
        let (mut tw, mut yt) = pair();
        tw.policy.pref = None;
        yt.policy.pref = None;
        let all = vec![live(tw, 10), capturing(yt, 10)];
        assert_eq!(decide(&all, 1, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn the_preferred_instance_defers_to_an_established_capture_instead_of_duplicating_it() {
        let (tw, yt) = pair();
        // Twitch has been capturing for ages: too late to switch, and starting
        // a second capture now would be the very duplicate we're avoiding —
        // with the worse (later) start of the two.
        let all = vec![capturing(tw, 5_000), live(yt, 10)];
        assert_eq!(
            decide(&all, 2, NOW, SETTLE),
            SimulcastDecision::Standby { winner: 1, winner_platform: Platform::Twitch }
        );
        // With nothing else running it just records.
        let (tw, yt) = pair();
        assert_eq!(decide(&[tw, live(yt, 10)], 2, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn non_preferred_stands_by_while_the_preferred_captures() {
        let (tw, yt) = pair();
        let all = vec![live(tw, 10), capturing(yt, 10)];
        assert_eq!(
            decide(&all, 1, NOW, SETTLE),
            SimulcastDecision::Standby { winner: 2, winner_platform: Platform::YouTube }
        );
    }

    #[test]
    fn platform_exclusive_records() {
        // The never-lose-a-broadcast rule: preferred platform simply isn't live.
        let (tw, yt) = pair();
        let all = vec![live(tw, 10), yt];
        assert_eq!(decide(&all, 1, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn a_dormant_or_detection_disabled_winner_never_strands_the_candidate() {
        // Both keep a stale last_state = "live" forever, since neither is polled.
        let (tw, yt) = pair();
        let mut dormant = live(yt.clone(), 10);
        dormant.automation_on = false;
        assert_eq!(decide(&[live(tw.clone(), 10), dormant], 1, NOW, SETTLE), SimulcastDecision::Record);

        let mut disabled = live(yt, 10);
        disabled.detection_disabled = true;
        assert_eq!(decide(&[live(tw, 10), disabled], 1, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn failover_opens_after_the_settle_window() {
        let (tw, yt) = pair();
        // Preferred went live but never started capturing.
        let waiting = decide(&[live(tw.clone(), 100), live(yt.clone(), 100)], 1, NOW, SETTLE);
        assert!(matches!(waiting, SimulcastDecision::Standby { .. }), "still inside the window");
        let expired = decide(&[live(tw, 200), live(yt, 200)], 1, NOW, SETTLE);
        assert_eq!(expired, SimulcastDecision::Record, "window elapsed — take over");
    }

    #[test]
    fn failover_re_graces_after_the_winners_take_ends() {
        let (tw, yt) = pair();
        // Live for hours, so live_since alone would have expired long ago —
        // but its capture only just dropped, so it gets a fresh window.
        let mut just_dropped = live(yt.clone(), 9_000);
        just_dropped.last_take_ended = Some(NOW - 30);
        assert!(matches!(
            decide(&[live(tw.clone(), 9_000), just_dropped], 1, NOW, SETTLE),
            SimulcastDecision::Standby { .. }
        ));

        let mut long_dead = live(yt, 9_000);
        long_dead.last_take_ended = Some(NOW - 900);
        assert_eq!(
            decide(&[live(tw, 9_000), long_dead], 1, NOW, SETTLE),
            SimulcastDecision::Record,
            "gave up on it a while ago"
        );
    }

    #[test]
    fn a_winner_that_cannot_record_is_taken_over_immediately() {
        let (tw, yt) = pair();
        // Auto-off there: no waiting, it was never going to capture.
        let mut auto_off = live(yt.clone(), 5);
        auto_off.auto_record_on = false;
        assert_eq!(decide(&[live(tw.clone(), 5), auto_off], 1, NOW, SETTLE), SimulcastDecision::Record);

        let mut held = live(yt, 5);
        held.stop_held = true;
        assert_eq!(decide(&[live(tw, 5), held], 1, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn ad_free_override_flips_the_preference() {
        let (mut tw, mut yt) = pair();
        // Normally YouTube, but Twitch when we're subscribed there.
        tw.policy.ad_free_pref = Some(Platform::Twitch);
        yt.policy.ad_free_pref = Some(Platform::Twitch);
        let mut sub = live(tw.clone(), 10);
        sub.ad_free = true;
        let all = vec![sub, capturing(yt.clone(), 10)];
        // Twitch is now the preferred one: it takes the young YouTube capture
        // over rather than deferring to it…
        assert_eq!(decide(&all, 1, NOW, SETTLE), SimulcastDecision::Takeover { stop: vec![2] });
        // …and YouTube (the everyday preference) defers to it.
        assert_eq!(
            decide(&all, 2, NOW, SETTLE),
            SimulcastDecision::Standby { winner: 1, winner_platform: Platform::Twitch }
        );

        // Without the sub, the everyday preference stands.
        let all = vec![live(tw, 10), capturing(yt, 10)];
        assert!(matches!(decide(&all, 1, NOW, SETTLE), SimulcastDecision::Standby { .. }));
    }

    #[test]
    fn ad_free_override_ignores_an_offline_ad_free_instance() {
        let (mut tw, mut yt) = pair();
        tw.policy.ad_free_pref = Some(Platform::Twitch);
        yt.policy.ad_free_pref = Some(Platform::Twitch);
        // Subscribed on Twitch, but Twitch isn't broadcasting: YouTube must not
        // be redirected onto a channel that isn't live.
        tw.ad_free = true;
        let all = vec![tw, capturing(yt, 10)];
        assert_eq!(decide(&all, 2, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn mismatched_instance_overrides_fail_open() {
        // Each instance prefers the other's platform — without the fail-open
        // rule both would stand by and the broadcast would be lost entirely.
        let mut tw = state(1, Platform::Twitch, Some(Platform::YouTube));
        let mut yt = state(2, Platform::YouTube, Some(Platform::Twitch));
        tw = live(tw, 10);
        yt = live(yt, 10);
        let all = vec![tw, yt];
        assert_eq!(decide(&all, 1, NOW, SETTLE), SimulcastDecision::Record);
        assert_eq!(decide(&all, 2, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn takeover_stops_only_young_non_preferred_captures() {
        let (tw, yt) = pair();
        // Twitch beat YouTube to the punch 30s ago: still settling, so switch.
        let all = vec![capturing(tw.clone(), 30), live(yt.clone(), 5)];
        assert_eq!(decide(&all, 2, NOW, SETTLE), SimulcastDecision::Takeover { stop: vec![1] });

        // An hour in, the Twitch capture is the intact one — leave it alone and
        // don't start a second copy either.
        let all = vec![capturing(tw, 3_600), live(yt, 3_600)];
        assert_eq!(
            decide(&all, 2, NOW, SETTLE),
            SimulcastDecision::Standby { winner: 1, winner_platform: Platform::Twitch }
        );
    }

    #[test]
    fn two_instances_on_the_preferred_platform_both_record() {
        // Documented limitation: dedup is across platforms, not within one.
        let a = capturing(state(1, Platform::YouTube, Some(Platform::YouTube)), 30);
        let b = live(state(2, Platform::YouTube, Some(Platform::YouTube)), 5);
        assert_eq!(decide(&[a, b], 2, NOW, SETTLE), SimulcastDecision::Record);
    }

    #[test]
    fn pref_strings_roundtrip_and_reject_garbage() {
        for p in SimulcastPref::ALL {
            assert_eq!(SimulcastPref::parse(p.as_str()), Some(p));
        }
        assert_eq!(SimulcastPref::parse(""), Some(SimulcastPref::Off), "unset reads as off");
        assert_eq!(SimulcastPref::parse("nonsense"), None);
        assert_eq!(SimulcastPref::Twitch.platform(), Some(Platform::Twitch));
        assert_eq!(SimulcastPref::Off.platform(), None);
    }

    #[test]
    fn precedence_monitor_over_channel_over_global_per_field() {
        let ch = SimulcastScope {
            pref: Some(SimulcastPref::YouTube),
            ad_free_pref: Some(SimulcastPref::Twitch),
        };
        let mon = SimulcastScope { pref: Some(SimulcastPref::Kick), ad_free_pref: None };
        // Instance wins where it has an opinion…
        assert_eq!(
            effective_pref_from(SimulcastPref::Off, Some(&ch), Some(&mon)),
            SimulcastPref::Kick
        );
        // …and the fields resolve independently: the untouched one still
        // follows the channel, not the instance's silence-as-off.
        assert_eq!(
            effective_ad_free_pref_from(SimulcastPref::Off, Some(&ch), Some(&mon)),
            SimulcastPref::Twitch
        );
        // Nothing set anywhere: the global stands.
        assert_eq!(
            effective_pref_from(SimulcastPref::YouTube, None, None),
            SimulcastPref::YouTube
        );
    }

    #[test]
    fn scope_json_roundtrips_and_empty_means_inherit() {
        let scope = SimulcastScope {
            pref: Some(SimulcastPref::Twitch),
            ad_free_pref: Some(SimulcastPref::Off),
        };
        let json = serde_json::to_string(&scope).unwrap();
        assert_eq!(serde_json::from_str::<SimulcastScope>(&json).unwrap(), scope);
        // An older blob with neither field present still loads.
        let empty: SimulcastScope = serde_json::from_str("{}").unwrap();
        assert!(empty.is_inherit());
        assert!(!scope.is_inherit());
    }

    #[test]
    fn channel_and_monitor_scopes_roundtrip_and_clear() {
        let store = Store::open_in_memory().unwrap();
        let scope = SimulcastScope {
            pref: Some(SimulcastPref::YouTube),
            ad_free_pref: Some(SimulcastPref::Twitch),
        };
        save_channel_simulcast_scope(&store, 7, &scope).unwrap();
        save_monitor_simulcast_scope(&store, 9, &scope).unwrap();
        assert_eq!(load_channel_simulcast_scope(&store, 7), scope);
        assert_eq!(load_monitor_simulcast_scope(&store, 9), scope);
        // An untouched id inherits.
        assert!(load_channel_simulcast_scope(&store, 8).is_inherit());

        // Clearing removes the entry rather than storing an inert one.
        save_channel_simulcast_scope(&store, 7, &SimulcastScope::default()).unwrap();
        assert!(load_channel_simulcast_scope(&store, 7).is_inherit());
        let raw = store.get_setting(K_CHANNEL_SIMULCAST_SCOPE).unwrap().unwrap_or_default();
        assert!(!raw.contains("\"7\""), "cleared scope left behind: {raw}");
    }

    #[test]
    fn ctx_resolves_every_tier_and_the_settle_window() {
        let store = Store::open_in_memory().unwrap();
        store.set_setting(K_SIMULCAST_PREF, "youtube").unwrap();
        save_monitor_simulcast_scope(
            &store,
            5,
            &SimulcastScope { pref: Some(SimulcastPref::Off), ad_free_pref: None },
        )
        .unwrap();

        let ctx = SimulcastCtx::load(&store);
        assert_eq!(ctx.settle_secs, SETTLE_SECS_DEFAULT, "unset falls back to the default");
        assert_eq!(ctx.policy_for(1, 4).pref, Some(Platform::YouTube));
        // The per-instance exemption: always record this one.
        assert_eq!(ctx.policy_for(1, 5).pref, None);

        store.set_setting(K_SIMULCAST_SETTLE_SECS, "0").unwrap();
        assert_eq!(
            SimulcastCtx::load(&store).settle_secs,
            SETTLE_SECS_DEFAULT,
            "a blank/zero field is not a request to skip settling"
        );
    }
}
