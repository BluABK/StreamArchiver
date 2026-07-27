//! Channel groups: named collections of channels (`channel_group`), with a
//! many-to-many membership table (`channel_group_member`) and a per-channel
//! `primary_group_id` column on `channel` for default-view clustering — see
//! `models::Channel::primary_group_id`'s doc comment for the primary/member
//! distinction.

use super::*;

impl Store {
    /// Create a new, empty group. Returns its id.
    pub fn create_channel_group(&self, name: &str) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO channel_group(name, created_at) VALUES(?1, ?2)",
            params![name, now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn rename_channel_group(&self, id: i64, name: &str) -> Result<()> {
        let conn = self.db();
        conn.execute("UPDATE channel_group SET name = ?2 WHERE id = ?1", params![id, name])?;
        Ok(())
    }

    /// Delete a group. Membership rows cascade (FK). Any channel that had
    /// this group as its *primary* falls back to ungrouped — there's no FK
    /// on `primary_group_id` (see the v77 migration's doc comment), so that
    /// needs a manual clear here.
    pub fn delete_channel_group(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET primary_group_id = NULL WHERE primary_group_id = ?1",
            params![id],
        )?;
        conn.execute("DELETE FROM channel_group WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// All groups, alphabetical.
    pub fn list_channel_groups(&self) -> Result<Vec<crate::models::ChannelGroup>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at FROM channel_group ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(crate::models::ChannelGroup { id: r.get(0)?, name: r.get(1)?, created_at: r.get(2)? })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Every channel id that's a *member* of this group (primary or
    /// secondary alike — membership is membership; see the module doc
    /// comment). Used by the Streams grid's group filter.
    pub fn channel_ids_in_group(&self, group_id: i64) -> Result<std::collections::HashSet<i64>> {
        let conn = self.db();
        let mut stmt =
            conn.prepare("SELECT channel_id FROM channel_group_member WHERE group_id = ?1")?;
        let rows = stmt
            .query_map(params![group_id], |r| r.get(0))?
            .collect::<rusqlite::Result<std::collections::HashSet<i64>>>()?;
        Ok(rows)
    }

    /// Every group this channel is a member of (primary or secondary),
    /// alphabetical — seeds the channel form's secondary-group checklist
    /// (which also includes the primary, since primary implies membership).
    pub fn channel_groups_for_channel(&self, channel_id: i64) -> Result<Vec<i64>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT m.group_id FROM channel_group_member m
             JOIN channel_group g ON g.id = m.group_id
             WHERE m.channel_id = ?1
             ORDER BY g.name COLLATE NOCASE, g.id",
        )?;
        let rows = stmt
            .query_map(params![channel_id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Add or remove `channel_id` as a member of `group_id`. Removing a
    /// channel's *primary* group also clears `primary_group_id` — it can't
    /// stay primary once it's not even a member.
    pub fn set_channel_group_member(&self, channel_id: i64, group_id: i64, member: bool) -> Result<()> {
        let conn = self.db();
        if member {
            conn.execute(
                "INSERT OR IGNORE INTO channel_group_member(channel_id, group_id) VALUES(?1, ?2)",
                params![channel_id, group_id],
            )?;
        } else {
            conn.execute(
                "DELETE FROM channel_group_member WHERE channel_id = ?1 AND group_id = ?2",
                params![channel_id, group_id],
            )?;
            conn.execute(
                "UPDATE channel SET primary_group_id = NULL
                 WHERE id = ?1 AND primary_group_id = ?2",
                params![channel_id, group_id],
            )?;
        }
        Ok(())
    }

    /// Set (or clear, with `None`) `channel_id`'s primary group. Setting one
    /// also ensures a membership row exists — a channel is always at least a
    /// member of its own primary group.
    pub fn set_channel_primary_group(&self, channel_id: i64, group_id: Option<i64>) -> Result<()> {
        let conn = self.db();
        if let Some(gid) = group_id {
            conn.execute(
                "INSERT OR IGNORE INTO channel_group_member(channel_id, group_id) VALUES(?1, ?2)",
                params![channel_id, gid],
            )?;
        }
        conn.execute(
            "UPDATE channel SET primary_group_id = ?2 WHERE id = ?1",
            params![channel_id, group_id],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn membership_and_primary_roundtrip() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let g1 = store.create_channel_group("VTubers").unwrap();
        let g2 = store.create_channel_group("Music").unwrap();

        assert!(store.list_channel_groups().unwrap().iter().any(|g| g.id == g1 && g.name == "VTubers"));
        assert!(store.channel_groups_for_channel(cid).unwrap().is_empty());

        // Setting primary implies membership.
        store.set_channel_primary_group(cid, Some(g1)).unwrap();
        assert_eq!(store.list_channels().unwrap()[0].primary_group_id, Some(g1));
        assert_eq!(store.channel_groups_for_channel(cid).unwrap(), vec![g1]);
        assert!(store.channel_ids_in_group(g1).unwrap().contains(&cid));

        // A channel can also be a plain (secondary) member of another group.
        // "Music" < "VTubers" alphabetically, so it sorts first.
        store.set_channel_group_member(cid, g2, true).unwrap();
        assert_eq!(store.channel_groups_for_channel(cid).unwrap(), vec![g2, g1]);
        assert!(store.channel_ids_in_group(g2).unwrap().contains(&cid));
        // Primary is unaffected by an unrelated secondary membership change.
        assert_eq!(store.list_channels().unwrap()[0].primary_group_id, Some(g1));

        // Removing the PRIMARY group as a membership clears primary too.
        store.set_channel_group_member(cid, g1, false).unwrap();
        assert_eq!(store.list_channels().unwrap()[0].primary_group_id, None);
        assert_eq!(store.channel_groups_for_channel(cid).unwrap(), vec![g2]);

        // Deleting a group clears it as anyone's primary and drops membership.
        store.set_channel_primary_group(cid, Some(g2)).unwrap();
        store.delete_channel_group(g2).unwrap();
        assert_eq!(store.list_channels().unwrap()[0].primary_group_id, None);
        assert!(store.channel_groups_for_channel(cid).unwrap().is_empty());
        assert!(store.list_channel_groups().unwrap().iter().all(|g| g.id != g2));
    }

    #[test]
    fn deleting_channel_cascades_membership() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let g = store.create_channel_group("Group").unwrap();
        store.set_channel_group_member(cid, g, true).unwrap();
        assert!(store.channel_ids_in_group(g).unwrap().contains(&cid));

        store.delete_channel(cid).unwrap();
        assert!(store.channel_ids_in_group(g).unwrap().is_empty());
    }
}
