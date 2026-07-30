//! History of automatic media disposals (schema v73 `disposal_record`) — backs
//! the Trash view. Every `disposal::dispose_media` call inserts one row via
//! [`Store::insert_disposal_record`]; the Trash view's Restore/Permanently
//! delete actions read one back (`get_disposal_record`), act on the file, then
//! flip its `state` (`set_disposal_record_state`).

use crate::disposal::{DisposalConfidence, DisposalMethod, DisposalRecordRow, DisposalRecordState};

use super::*;

/// One disposal record joined with enough of its recording/channel context to
/// render a Trash-view row without a second query per row.
#[derive(Clone)]
pub struct DisposalRecordDisplay {
    pub row: DisposalRecordRow,
    /// `None` when the recording (or its monitor/channel) no longer exists —
    /// old recordings are never deleted, so this is only a safety net.
    pub channel_id: Option<i64>,
    pub channel_name: String,
    pub take_started_at: Option<i64>,
}

impl Store {
    /// Log a completed disposal. Pure history — never updated in place except
    /// via `set_disposal_record_state`, and never deduped (a `rec_id` can
    /// legitimately gain several rows, e.g. its head disposed by post-join
    /// cleanup and its live capture disposed later by a VOD replace).
    pub fn insert_disposal_record(&self, row: &DisposalRecordRow) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO disposal_record(
                 rec_id, reason, method, original_path, trash_path, state,
                 disposed_at, updated_at, confidence)
             VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                row.rec_id,
                row.reason,
                row.method.as_str(),
                row.original_path,
                row.trash_path,
                row.state.as_str(),
                row.disposed_at,
                row.updated_at,
                row.confidence.as_str(),
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    fn row_from(r: &rusqlite::Row) -> rusqlite::Result<DisposalRecordRow> {
        let method: String = r.get(2)?;
        let state: String = r.get(5)?;
        let confidence: String = r.get(9)?;
        Ok(DisposalRecordRow {
            id: r.get(0)?,
            rec_id: r.get(1)?,
            reason: r.get(3)?,
            method: DisposalMethod::parse(&method).unwrap_or_default(),
            original_path: r.get(4)?,
            trash_path: r.get(6)?,
            state: DisposalRecordState::parse(&state).unwrap_or(DisposalRecordState::Permanent),
            disposed_at: r.get(7)?,
            updated_at: r.get(8)?,
            confidence: DisposalConfidence::parse(&confidence).unwrap_or(DisposalConfidence::Live),
        })
    }

    pub fn get_disposal_record(&self, id: i64) -> Result<Option<DisposalRecordRow>> {
        let conn = self.db();
        conn.query_row(
            "SELECT id, rec_id, method, reason, original_path, state, trash_path,
                    disposed_at, updated_at, confidence
             FROM disposal_record WHERE id=?1",
            params![id],
            Self::row_from,
        )
        .optional()
        .map_err(Into::into)
    }

    /// How many disposed files are still sitting in a trash folder
    /// (`state == SoftDeleted`) — i.e. still occupying the recordings drive and
    /// still restorable.
    ///
    /// Backs the top bar's 🗑 badge. A trash folder is only ever emptied by
    /// hand, so without a visible count it silently accumulates: the whole
    /// point of choosing "Trash folder" over the Recycle Bin is that the app
    /// knows what's in it, so it should say so.
    pub fn trashed_file_count(&self) -> Result<i64> {
        let conn = self.db();
        conn.query_row(
            "SELECT COUNT(*) FROM disposal_record WHERE state = ?1",
            params![DisposalRecordState::SoftDeleted.as_str()],
            |r| r.get(0),
        )
        .map_err(Into::into)
    }

    /// `(rec_id, reason)` pairs already logged — the historical-import scan
    /// (`disposal_backfill`) skips any candidate matching one of these, so
    /// re-running the scan never duplicates an entry.
    pub fn disposal_record_keys(&self) -> Result<std::collections::HashSet<(i64, String)>> {
        let conn = self.db();
        let mut stmt = conn.prepare("SELECT rec_id, reason FROM disposal_record")?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<std::collections::HashSet<(i64, String)>>>()?;
        Ok(rows)
    }

    /// Flip a record's state (restore / permanently-delete). `trash_path`:
    /// `Some(new)` overwrites it (restore clears it to `""`), `None` leaves it
    /// untouched (permanently-delete keeps the historical path for the log).
    pub fn set_disposal_record_state(
        &self,
        id: i64,
        state: DisposalRecordState,
        trash_path: Option<&str>,
        updated_at: i64,
    ) -> Result<()> {
        let conn = self.db();
        match trash_path {
            Some(p) => conn.execute(
                "UPDATE disposal_record SET state=?1, trash_path=?2, updated_at=?3 WHERE id=?4",
                params![state.as_str(), p, updated_at, id],
            )?,
            None => conn.execute(
                "UPDATE disposal_record SET state=?1, updated_at=?2 WHERE id=?3",
                params![state.as_str(), updated_at, id],
            )?,
        };
        Ok(())
    }

    /// Every logged disposal, newest first, joined with its recording's
    /// channel/take context for the Trash view. A recording that's since been
    /// deleted from the DB (shouldn't normally happen) still shows up with
    /// `channel_id: None` rather than silently vanishing from the history.
    pub fn list_disposal_records(&self) -> Result<Vec<DisposalRecordDisplay>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT d.id, d.rec_id, d.method, d.reason, d.original_path, d.state,
                    d.trash_path, d.disposed_at, d.updated_at, d.confidence,
                    c.id, c.name, r.started_at
             FROM disposal_record d
             LEFT JOIN recording r ON r.id = d.rec_id
             LEFT JOIN monitor m ON m.id = r.monitor_id
             LEFT JOIN channel c ON c.id = m.channel_id
             ORDER BY d.disposed_at DESC",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(DisposalRecordDisplay {
                    row: Self::row_from(r)?,
                    channel_id: r.get(10)?,
                    channel_name: r.get::<_, Option<String>>(11)?.unwrap_or_default(),
                    take_started_at: r.get(12)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(rec_id: i64, state: DisposalRecordState) -> DisposalRecordRow {
        DisposalRecordRow {
            id: 0,
            rec_id,
            reason: "post-join cleanup: head".into(),
            method: DisposalMethod::Trash,
            original_path: r"A:\streams\Ch\x.head.mkv".into(),
            trash_path: r"A:\streams\.sa-trash\x.head.mkv".into(),
            state,
            disposed_at: 1_000,
            updated_at: 1_000,
            confidence: DisposalConfidence::Live,
        }
    }

    #[test]
    fn insert_get_and_state_round_trip() {
        let store = Store::open_in_memory().unwrap();
        let id = store.insert_disposal_record(&sample(1, DisposalRecordState::SoftDeleted)).unwrap();
        let row = store.get_disposal_record(id).unwrap().unwrap();
        assert_eq!(row.rec_id, 1);
        assert_eq!(row.state, DisposalRecordState::SoftDeleted);
        assert_eq!(row.method, DisposalMethod::Trash);

        store.set_disposal_record_state(id, DisposalRecordState::Restored, Some(""), 2_000).unwrap();
        let row = store.get_disposal_record(id).unwrap().unwrap();
        assert_eq!(row.state, DisposalRecordState::Restored);
        assert_eq!(row.trash_path, "");
        assert_eq!(row.updated_at, 2_000);

        assert!(store.get_disposal_record(id + 1).unwrap().is_none());
    }

    #[test]
    fn set_state_without_trash_path_leaves_it_untouched() {
        let store = Store::open_in_memory().unwrap();
        let id = store.insert_disposal_record(&sample(1, DisposalRecordState::SoftDeleted)).unwrap();
        store.set_disposal_record_state(id, DisposalRecordState::Permanent, None, 2_000).unwrap();
        let row = store.get_disposal_record(id).unwrap().unwrap();
        assert_eq!(row.state, DisposalRecordState::Permanent);
        assert_eq!(row.trash_path, r"A:\streams\.sa-trash\x.head.mkv");
    }

    #[test]
    fn list_orders_newest_first_and_survives_a_missing_recording() {
        let store = Store::open_in_memory().unwrap();
        let mut a = sample(999, DisposalRecordState::SoftDeleted); // no such recording row
        a.disposed_at = 1_000;
        let mut b = sample(999, DisposalRecordState::Permanent);
        b.disposed_at = 2_000;
        store.insert_disposal_record(&a).unwrap();
        store.insert_disposal_record(&b).unwrap();

        let list = store.list_disposal_records().unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].row.disposed_at, 2_000); // newest first
        assert_eq!(list[1].row.disposed_at, 1_000);
        assert!(list[0].channel_id.is_none());
        assert_eq!(list[0].channel_name, "");
    }

    #[test]
    fn confidence_round_trips_and_defaults_live() {
        let store = Store::open_in_memory().unwrap();
        let mut historical = sample(5, DisposalRecordState::Permanent);
        historical.confidence = DisposalConfidence::HistoricalGuess;
        let id = store.insert_disposal_record(&historical).unwrap();
        assert_eq!(store.get_disposal_record(id).unwrap().unwrap().confidence, DisposalConfidence::HistoricalGuess);

        let live_id = store.insert_disposal_record(&sample(6, DisposalRecordState::Permanent)).unwrap();
        assert_eq!(store.get_disposal_record(live_id).unwrap().unwrap().confidence, DisposalConfidence::Live);
    }

    /// The 🗑 badge counts only what's still IN a trash folder — a recycled or
    /// permanently-deleted file is gone and costs nothing, so it must not keep
    /// the badge lit forever.
    #[test]
    fn trashed_file_count_counts_only_soft_deleted() {
        let store = Store::open_in_memory().unwrap();
        assert_eq!(store.trashed_file_count().unwrap(), 0);

        store.insert_disposal_record(&sample(1, DisposalRecordState::SoftDeleted)).unwrap();
        store.insert_disposal_record(&sample(2, DisposalRecordState::SoftDeleted)).unwrap();
        store.insert_disposal_record(&sample(3, DisposalRecordState::Permanent)).unwrap();
        store.insert_disposal_record(&sample(4, DisposalRecordState::Restored)).unwrap();
        assert_eq!(store.trashed_file_count().unwrap(), 2);

        // Emptying the trash (permanently deleting a row) clears it down.
        let id = store.insert_disposal_record(&sample(5, DisposalRecordState::SoftDeleted)).unwrap();
        assert_eq!(store.trashed_file_count().unwrap(), 3);
        store
            .set_disposal_record_state(id, DisposalRecordState::Permanent, None, 99)
            .unwrap();
        assert_eq!(store.trashed_file_count().unwrap(), 2);
    }

    #[test]
    fn disposal_record_keys_reflects_existing_rows() {
        let store = Store::open_in_memory().unwrap();
        store.insert_disposal_record(&sample(1, DisposalRecordState::Permanent)).unwrap();
        let keys = store.disposal_record_keys().unwrap();
        assert!(keys.contains(&(1, "post-join cleanup: head".to_string())));
        assert!(!keys.contains(&(2, "post-join cleanup: head".to_string())));
    }
}
