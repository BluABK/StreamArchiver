//! Live-detection adapters (Phase 2).
//!
//! Each platform/method gets an adapter implementing a `Detector` trait that
//! takes a batch of monitors and returns their live state. Planned adapters:
//! `TwitchHelix`, `Scrape` (YouTube `/live`, Kick JSON), `GenericProbe`,
//! `CliSelfPoll`, and later `TwitchEventSub`.
//!
//! Intentionally empty in Phase 1.
