//! Watch-state for the Backlog/Stream History views — see `stream_watch`
//! (schema v71) and `crate::models::stream_key`.

use std::collections::HashMap;

use super::*;

impl Store {
    /// Set (or clear) a broadcast's watch state, stamping `watch_state_at`.
    /// `key` is `crate::models::stream_key`/`StreamGroup::key`. Upserts, since
    /// most broadcasts have no row until first touched (see
    /// `stream_watch_states`'s "absent = unwatched" convention).
    pub fn set_stream_watch_state(&self, key: &str, monitor_id: i64, state: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO stream_watch (stream_key, monitor_id, watch_state, watch_state_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(stream_key) DO UPDATE SET
                watch_state=excluded.watch_state, watch_state_at=excluded.watch_state_at",
            params![key, monitor_id, state, now_unix()],
        )?;
        Ok(())
    }

    /// A single broadcast's watch state, when it has one — used by the
    /// auto-"started" playback hook (a single indexed lookup on click, not a
    /// full-table load). `None` means never touched (treat as `"unwatched"`).
    pub fn stream_watch_state(&self, key: &str) -> Result<Option<(String, Option<i64>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT watch_state, watch_state_at FROM stream_watch WHERE stream_key=?1",
                params![key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// All known watch states, keyed by broadcast key. A key absent from this
    /// map has never been touched — callers should treat that as
    /// `("unwatched", None)` (see `crate::ui::history::effective_watch_state`).
    pub fn stream_watch_states(&self) -> Result<HashMap<String, (String, Option<i64>)>> {
        let conn = self.db();
        let mut stmt =
            conn.prepare("SELECT stream_key, watch_state, watch_state_at FROM stream_watch")?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, String>(0)?, (r.get::<_, String>(1)?, r.get(2)?)))
            })?
            .collect::<rusqlite::Result<HashMap<_, _>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    #[test]
    fn set_stream_watch_state_upserts_and_stamps_at() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        assert!(store.stream_watch_state("s1:abc").unwrap().is_none());

        store.set_stream_watch_state("s1:abc", mid, "started").unwrap();
        let (state, at) = store.stream_watch_state("s1:abc").unwrap().unwrap();
        assert_eq!(state, "started");
        assert!(at.is_some());

        // A second call overwrites (upsert), not a duplicate row / conflict error.
        store.set_stream_watch_state("s1:abc", mid, "watched").unwrap();
        let (state, _) = store.stream_watch_state("s1:abc").unwrap().unwrap();
        assert_eq!(state, "watched");

        let all = store.stream_watch_states().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all.get("s1:abc").unwrap().0, "watched");
    }
}
