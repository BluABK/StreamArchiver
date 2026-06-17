//! Shared poll scheduler (Phase 2).
//!
//! Will run a single task driven by a `DelayQueue`/binary-heap that pops due
//! monitors, groups them by `(platform, detection_method)`, and fires *batched*
//! detection calls (e.g. one Twitch Helix `Get Streams` request covering up to
//! 100 channels). This avoids one-thread/one-process-per-channel, which is the
//! core of the low-idle-footprint requirement.
//!
//! Intentionally empty in Phase 1.
