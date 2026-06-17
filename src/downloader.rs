//! Download supervisor + per-tool adapters (Phase 3).
//!
//! Will own a bounded concurrency semaphore, spawn streamlink/yt-dlp/ffmpeg via
//! `tokio::process` with piped stdout/stderr, tail logs, classify completion
//! (exit + file finalized + stderr patterns), apply exponential backoff, kill
//! whole process trees (Win32 Job Object), and remux TS -> MKV. Default output
//! container is MKV; never MP4.
//!
//! Intentionally empty in Phase 1.
