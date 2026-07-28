//! Recording groups: free-form cross-cutting tags spanning any number of
//! *takes* (`recording_group`/`recording_group_member`) — e.g. "Numi
//! Subathon 2025". No "primary" concept (unlike `channel_groups`): a take's
//! place in the tree is always its channel/instance, unaffected by this —
//! it's purely a filterable label, surfaced via the Streams grid's group
//! filter alongside channel groups.

use super::*;

impl Store {
    /// Create a new, empty group. Returns its id.
    pub fn create_recording_group(&self, name: &str) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO recording_group(name, created_at) VALUES(?1, ?2)",
            params![name, now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn rename_recording_group(&self, id: i64, name: &str) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE recording_group SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
    }

    /// Delete a group. Membership rows cascade (FK) — no other table
    /// references a recording group, so there's nothing else to clean up
    /// (unlike `delete_channel_group`, which also clears `primary_group_id`).
    pub fn delete_recording_group(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM recording_group WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// All groups, alphabetical.
    pub fn list_recording_groups(&self) -> Result<Vec<crate::models::RecordingGroup>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at FROM recording_group ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::RecordingGroup { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every recording (take) id that's a member of this group. A "Stream"
    /// (broadcast) counts as in the group when ANY of its takes are — see
    /// `add_recordings_to_group`, which always adds every take of a
    /// selected stream together, so this is never a partial match in
    /// practice. Used by the Streams grid's group filter.
    pub fn recording_ids_in_group(&self, group_id: i64) -> Result<std::collections::HashSet<i64>> {
        let conn = self.db();
        let mut stmt =
            conn.prepare("SELECT recording_id FROM recording_group_member WHERE group_id = ?1")?;
        let rows = stmt
            .query_map(params![group_id], |r| r.get(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
        Ok(rows)
    }

    /// Add every id in `recording_ids` (a stream's full take list, or any
    /// other set) to `group_id` — idempotent per id.
    pub fn add_recordings_to_group(&self, recording_ids: &[i64], group_id: i64) -> Result<()> {
        let conn = self.db();
        for &rid in recording_ids {
            conn.execute(
                "INSERT OR IGNORE INTO recording_group_member(recording_id, group_id) VALUES(?1, ?2)",
                params![rid, group_id],
            )?;
        }
        Ok(())
    }

    /// Remove every id in `recording_ids` from `group_id` — idempotent, and
    /// a no-op for ids that aren't members.
    pub fn remove_recordings_from_group(&self, recording_ids: &[i64], group_id: i64) -> Result<()> {
        let conn = self.db();
        for &rid in recording_ids {
            conn.execute(
                "DELETE FROM recording_group_member WHERE recording_id = ?1 AND group_id = ?2",
                params![rid, group_id],
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_roundtrip_and_bulk_add() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = crate::store::test_util::sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let r1 = store.insert_recording(mid, 1_000, "C:/rec/1.mkv", Some(1_000), false, Some("s1"), None, "", "").unwrap();
        let r2 = store.insert_recording(mid, 2_000, "C:/rec/2.mkv", Some(2_000), false, Some("s1"), None, "", "").unwrap();
        let other = store.insert_recording(mid, 3_000, "C:/rec/3.mkv", Some(3_000), false, Some("s2"), None, "", "").unwrap();

        let g = store.create_recording_group("Numi Subathon 2025").unwrap();
        assert!(store.list_recording_groups().unwrap().iter().any(|x| x.id == g));
        assert!(store.recording_ids_in_group(g).unwrap().is_empty());

        // Adding a "stream" (both its takes) at once.
        store.add_recordings_to_group(&[r1, r2], g).unwrap();
        let members = store.recording_ids_in_group(g).unwrap();
        assert!(members.contains(&r1) && members.contains(&r2) && !members.contains(&other));

        // Removing one take doesn't touch the other.
        store.remove_recordings_from_group(&[r1], g).unwrap();
        let members = store.recording_ids_in_group(g).unwrap();
        assert!(!members.contains(&r1) && members.contains(&r2));

        // Rename + delete.
        store.rename_recording_group(g, "Numi Subathon (renamed)").unwrap();
        assert_eq!(store.list_recording_groups().unwrap()[0].name, "Numi Subathon (renamed)");
        store.delete_recording_group(g).unwrap();
        assert!(store.recording_ids_in_group(g).unwrap().is_empty());
        assert!(store.list_recording_groups().unwrap().is_empty());
    }

    #[test]
    fn deleting_recording_cascades_membership() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = crate::store::test_util::sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();
        let r1 = store.insert_recording(mid, 1_000, "C:/rec/1.mkv", Some(1_000), false, Some("s1"), None, "", "").unwrap();
        store.finish_recording(r1, 2_000, 500, Some(0), "completed", "C:/rec/1.mkv", "").unwrap();
        let g = store.create_recording_group("Group").unwrap();
        store.add_recordings_to_group(&[r1], g).unwrap();
        assert!(store.recording_ids_in_group(g).unwrap().contains(&r1));

        store.delete_recording(r1).unwrap();
        assert!(store.recording_ids_in_group(g).unwrap().is_empty());
    }
}
