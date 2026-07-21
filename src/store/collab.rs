//! Twitch "Stream Together" collab-session history (schema v58).
//!
//! One row per observed collab session per monitor — `shared_chat` rows mirror
//! Helix `GET /shared_chat/session` (keyed by Twitch's `session_id`), `title`
//! rows are `@mention`-derived (keyed by the broadcast's `stream_id`).
//! Participant names are denormalized JSON captured at observation time.
//! `ended_at IS NULL` = still active; the poll routine ends sessions that
//! disappear. Set changes are ALSO logged to `monitor_stream_change`
//! (`kind = "collab"`) by the caller so the 📝 history popup shows them.

use super::*;
use crate::models::{CollabPartner, CollabSessionRow, collab_partner_names};

impl Store {
    /// Insert or refresh the open collab session identified by
    /// (`monitor_id`, `source`, key) — key is `session_id` for `shared_chat`
    /// rows and `stream_id` for `title` rows. Partners are UNIONED into an
    /// existing open row (people joining mid-session accumulate; a partner
    /// dropping out doesn't rewrite history), `host_id`/`stream_id` are
    /// filled in when they arrive late, and `last_seen_at` is bumped.
    ///
    /// Returns `Some((old_names, new_names))` when the stored partner set
    /// changed (an insert returns `("", names)`), for the caller's
    /// `monitor_stream_change` logging; `None` when nothing changed.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_collab_session(
        &self,
        monitor_id: i64,
        source: &str,
        session_id: &str,
        stream_id: &str,
        host_id: &str,
        partners: &[CollabPartner],
        session_created_at: i64,
        now: i64,
    ) -> Result<Option<(String, String)>> {
        let (key_col, key_val) = if source == "shared_chat" {
            ("session_id", session_id)
        } else {
            ("stream_id", stream_id)
        };
        let conn = self.db();
        let open: Option<(i64, String)> = conn
            .query_row(
                &format!(
                    "SELECT id, participants FROM collab_session
                     WHERE monitor_id = ?1 AND source = ?2 AND {key_col} = ?3
                       AND ended_at IS NULL
                     ORDER BY id DESC LIMIT 1"
                ),
                params![monitor_id, source, key_val],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;

        match open {
            None => {
                if partners.is_empty() {
                    return Ok(None);
                }
                let first_seen = if session_created_at > 0 { session_created_at } else { now };
                conn.execute(
                    "INSERT INTO collab_session(monitor_id, source, session_id, stream_id,
                        host_id, participants, first_seen_at, last_seen_at)
                     VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        monitor_id,
                        source,
                        session_id,
                        stream_id,
                        host_id,
                        serde_json::to_string(partners).unwrap_or_else(|_| "[]".into()),
                        first_seen,
                        now,
                    ],
                )?;
                Ok(Some((String::new(), collab_partner_names(partners))))
            }
            Some((row_id, old_json)) => {
                let mut merged: Vec<CollabPartner> =
                    serde_json::from_str(&old_json).unwrap_or_default();
                let old_names = collab_partner_names(&merged);
                for p in partners {
                    let key = |q: &CollabPartner| {
                        if q.id.is_empty() {
                            format!("~{}", q.name.to_lowercase())
                        } else {
                            q.id.clone()
                        }
                    };
                    if !merged.iter().any(|q| key(q) == key(p)) {
                        merged.push(p.clone());
                    }
                }
                let new_names = collab_partner_names(&merged);
                conn.execute(
                    "UPDATE collab_session
                     SET participants = ?2, last_seen_at = ?3,
                         host_id = CASE WHEN ?4 = '' THEN host_id ELSE ?4 END,
                         stream_id = CASE WHEN ?5 = '' THEN stream_id ELSE ?5 END
                     WHERE id = ?1",
                    params![
                        row_id,
                        serde_json::to_string(&merged).unwrap_or_else(|_| "[]".into()),
                        now,
                        host_id,
                        stream_id,
                    ],
                )?;
                Ok((old_names != new_names).then_some((old_names, new_names)))
            }
        }
    }

    /// Stamp `ended_at` on every open collab session of `monitor_id` whose key
    /// is NOT in the keep lists (`shared_chat` rows keyed by session id,
    /// `title` rows by stream id). Called on every poll — with empty keeps
    /// when the channel is offline or not collabing. Returns each closed
    /// session's partner-names string, for the caller's `monitor_stream_change`
    /// "collab ended" logging.
    pub fn end_open_collab_sessions(
        &self,
        monitor_id: i64,
        keep_session_ids: &[&str],
        keep_stream_ids: &[&str],
        now: i64,
    ) -> Result<Vec<String>> {
        let conn = self.db();
        let open: Vec<(i64, String, String, String, String)> = conn
            .prepare(
                "SELECT id, source, session_id, stream_id, participants FROM collab_session
                 WHERE monitor_id = ?1 AND ended_at IS NULL",
            )?
            .query_map(params![monitor_id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut closed = Vec::new();
        for (id, source, session_id, stream_id, participants) in open {
            let keep = if source == "shared_chat" {
                keep_session_ids.contains(&session_id.as_str())
            } else {
                keep_stream_ids.contains(&stream_id.as_str())
            };
            if !keep {
                conn.execute(
                    "UPDATE collab_session SET ended_at = ?2 WHERE id = ?1",
                    params![id, now],
                )?;
                let partners: Vec<CollabPartner> =
                    serde_json::from_str(&participants).unwrap_or_default();
                closed.push(collab_partner_names(&partners));
            }
        }
        Ok(closed)
    }

    /// Collab history for a channel (all its instances), newest first — the
    /// 🤝 Collab-history popup. `limit` caps the result (0 = unlimited).
    pub fn collab_sessions_for_channel(
        &self,
        channel_id: i64,
        limit: usize,
    ) -> Result<Vec<CollabSessionRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT cs.id, cs.monitor_id, cs.source, cs.session_id, cs.stream_id,
                    cs.host_id, cs.participants, cs.first_seen_at, cs.last_seen_at, cs.ended_at
             FROM collab_session cs
             JOIN monitor m ON m.id = cs.monitor_id
             WHERE m.channel_id = ?1
             ORDER BY cs.first_seen_at DESC, cs.id DESC",
        )?;
        let mut rows = stmt
            .query_map(params![channel_id], Self::map_collab_session)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if limit > 0 {
            rows.truncate(limit);
        }
        Ok(rows)
    }

    /// Aggregate partner overview for the Stats tab: one entry per partner
    /// (case-insensitive by name, most recent display casing kept) with the
    /// session count and the most recent sighting. Sorted by count desc.
    pub fn collab_partner_overview(&self) -> Result<Vec<(String, i64, i64)>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT participants, last_seen_at FROM collab_session
             ORDER BY last_seen_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut agg: std::collections::HashMap<String, (String, i64, i64)> =
            std::collections::HashMap::new();
        for (json, last_seen) in rows {
            let partners: Vec<CollabPartner> = serde_json::from_str(&json).unwrap_or_default();
            for p in partners {
                if p.name.is_empty() {
                    continue;
                }
                // Ascending scan → the last write per key carries the most
                // recent display casing and last_seen.
                let e = agg
                    .entry(p.name.to_lowercase())
                    .or_insert_with(|| (p.name.clone(), 0, 0));
                e.0 = p.name.clone();
                e.1 += 1;
                e.2 = e.2.max(last_seen);
            }
        }
        let mut out: Vec<(String, i64, i64)> = agg.into_values().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.to_lowercase().cmp(&b.0.to_lowercase())));
        Ok(out)
    }

    /// `(monitor_id, stream_id) → comma-joined partner names` for every stored
    /// session with a stream id — lets the grid's stream/take rows show which
    /// collab a past broadcast was, from one cheap preloaded map.
    pub fn collab_names_by_stream(
        &self,
    ) -> Result<std::collections::HashMap<(i64, String), String>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT monitor_id, stream_id, participants FROM collab_session
             WHERE stream_id <> ''
             ORDER BY last_seen_at ASC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut out: std::collections::HashMap<(i64, String), String> =
            std::collections::HashMap::new();
        for (mid, sid, json) in rows {
            let partners: Vec<CollabPartner> = serde_json::from_str(&json).unwrap_or_default();
            let names = collab_partner_names(&partners);
            if names.is_empty() {
                continue;
            }
            // Merge shared-chat + title rows of the same broadcast.
            let entry = out.entry((mid, sid)).or_default();
            if entry.is_empty() {
                *entry = names;
            } else {
                for n in names.split(", ") {
                    if !entry.split(", ").any(|e| e.eq_ignore_ascii_case(n)) {
                        entry.push_str(", ");
                        entry.push_str(n);
                    }
                }
            }
        }
        Ok(out)
    }

    fn map_collab_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<CollabSessionRow> {
        Ok(CollabSessionRow {
            id: r.get(0)?,
            monitor_id: r.get(1)?,
            source: r.get(2)?,
            session_id: r.get(3)?,
            stream_id: r.get(4)?,
            host_id: r.get(5)?,
            partners: serde_json::from_str(&r.get::<_, String>(6)?).unwrap_or_default(),
            first_seen_at: r.get(7)?,
            last_seen_at: r.get(8)?,
            ended_at: r.get(9)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    fn partner(id: &str, name: &str, from_title: bool) -> CollabPartner {
        CollabPartner {
            id: id.into(),
            login: name.to_lowercase(),
            name: name.into(),
            from_title,
        }
    }

    #[test]
    fn collab_session_upsert_union_end_and_overview() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Numi").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Insert: reports ("", names).
        let shylily = partner("100", "Shylily", false);
        let diff = store
            .upsert_collab_session(mid, "shared_chat", "sess-1", "st-1", "650", &[shylily.clone()], 1000, 1010)
            .unwrap();
        assert_eq!(diff, Some((String::new(), "Shylily".into())));

        // Same set again: no change reported.
        let diff = store
            .upsert_collab_session(mid, "shared_chat", "sess-1", "st-1", "650", &[shylily.clone()], 1000, 1020)
            .unwrap();
        assert_eq!(diff, None);

        // A third streamer joins: union, change reported.
        let ironmouse = partner("200", "Ironmouse", false);
        let diff = store
            .upsert_collab_session(mid, "shared_chat", "sess-1", "st-1", "650", &[shylily.clone(), ironmouse], 1000, 1030)
            .unwrap();
        assert_eq!(diff, Some(("Shylily".into(), "Shylily, Ironmouse".into())));

        // Title-mention session for the same broadcast, keyed by stream_id.
        let mention = partner("", "Zentreya", true);
        store
            .upsert_collab_session(mid, "title", "", "st-1", "", &[mention], 0, 1040)
            .unwrap();

        // Both open; ending with only the shared-chat key keeps it and closes
        // the title row.
        let closed = store
            .end_open_collab_sessions(mid, &["sess-1"], &[], 1050)
            .unwrap();
        assert_eq!(closed, vec!["@Zentreya".to_string()]);
        let sessions = store.collab_sessions_for_channel(cid, 0).unwrap();
        assert_eq!(sessions.len(), 2);
        let open: Vec<_> = sessions.iter().filter(|s| s.ended_at.is_none()).collect();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].source, "shared_chat");
        // first_seen_at honors the session's created_at, not first observation.
        assert_eq!(open[0].first_seen_at, 1000);

        // Channel offline: everything closes.
        let closed = store.end_open_collab_sessions(mid, &[], &[], 1060).unwrap();
        assert_eq!(closed, vec!["Shylily, Ironmouse".to_string()]);

        // Overview aggregates across sessions; the stream map merges sources.
        let overview = store.collab_partner_overview().unwrap();
        let lily = overview.iter().find(|(n, _, _)| n == "Shylily").unwrap();
        assert_eq!((lily.1, lily.2), (1, 1030), "one session, last touched at 1030");
        let by_stream = store.collab_names_by_stream().unwrap();
        assert_eq!(
            by_stream.get(&(mid, "st-1".into())).unwrap(),
            "Shylily, Ironmouse, @Zentreya"
        );
    }
}
