//! Channels, containers, and monitor (stream instance) CRUD/state.

use super::*;

impl Store {
    /// Whether a periodic background job (see `events::TOGGLEABLE_JOBS`) is enabled.
    /// Default `true`; only an explicit `"0"` disables it.
    pub fn job_enabled(&self, key: &str) -> bool {
        self.get_setting(key)
            .ok()
            .flatten()
            .map(|v| v != "0")
            .unwrap_or(true)
    }

    // ----- channels -----

    pub fn find_channel_by_url(&self, url: &str) -> Result<Option<Channel>> {
        let conn = self.db();
        let ch = conn
            .query_row(
                "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled, \
                 automation_enabled FROM channel WHERE url = ?1",
                params![url],
                Self::map_channel,
            )
            .optional()?;
        Ok(ch)
    }

    pub fn insert_channel(&self, name: &str, url: &str, platform: Platform) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO channel(name, url, platform, created_at) VALUES(?1, ?2, ?3, ?4)",
            params![name, url, platform.as_str(), now_unix()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    /// Get an existing channel by URL or create it.
    pub fn upsert_channel(&self, name: &str, url: &str, platform: Platform) -> Result<i64> {
        if let Some(existing) = self.find_channel_by_url(url)? {
            return Ok(existing.id);
        }
        self.insert_channel(name, url, platform)
    }

    pub fn delete_channel(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM channel WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// All channel containers (including ones with no instances yet), ordered to
    /// match the monitor list (name, then id).
    pub fn list_channels(&self) -> Result<Vec<Channel>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, name, url, platform, created_at, color, preferred_platform, enabled, \
             automation_enabled FROM channel
             ORDER BY name COLLATE NOCASE, id",
        )?;
        let rows = stmt
            .query_map([], Self::map_channel)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Create a new empty channel container (no URL/platform of its own; its
    /// instances carry the source URLs). Always inserts a new row. `enabled`/
    /// `automation_enabled` default to the schema default (both `true`).
    pub fn create_container(&self, name: &str) -> Result<i64> {
        self.insert_channel(name, "", Platform::Generic)
    }

    /// Create a new channel container, seeding its Auto (`enabled`) and
    /// Enabled (`automation_enabled`) switches instead of leaving them at the
    /// schema default. Meant for "add stream" flows that create a channel
    /// alongside its first instance: without this, a brand-new channel always
    /// starts Auto=on/Enabled=on regardless of what the instance was
    /// configured with, leaving a channel/instance mismatch the grid ANDs
    /// together (confusing even though not functionally broken) the moment
    /// it's created.
    pub fn create_container_with_flags(
        &self,
        name: &str,
        enabled: bool,
        automation_enabled: bool,
    ) -> Result<i64> {
        let id = self.create_container(name)?;
        self.set_channel_enabled(id, enabled)?;
        self.set_channel_automation_enabled(id, automation_enabled)?;
        Ok(id)
    }

    /// Rename a channel container.
    pub fn rename_channel(&self, id: i64, name: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        Ok(())
    }

    /// Set (or clear) the custom hex color for a channel container.
    /// Pass an empty string to revert to the automatic palette color.
    pub fn set_channel_color(&self, id: i64, color: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET color = ?2 WHERE id = ?1",
            params![id, color],
        )?;
        Ok(())
    }

    /// Set (or clear) the preferred asset source for a channel container — the
    /// platform (and optionally account) whose profile pic / banner represents
    /// it, stored as `platform[:account]` text. `None` reverts to auto (the
    /// first instance that has a fetched icon).
    pub fn set_channel_preferred_asset(
        &self,
        id: i64,
        source: Option<&crate::models::PreferredAssetSource>,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET preferred_platform = ?2 WHERE id = ?1",
            params![id, source.map(|s| s.to_db()).unwrap_or_default()],
        )?;
        Ok(())
    }

    /// Enable/disable a channel container's own flag. Does NOT touch individual
    /// instance (monitor) enabled states — those are independent.
    pub fn set_channel_enabled(&self, channel_id: i64, enabled: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET enabled = ?2 WHERE id = ?1",
            params![channel_id, enabled as i64],
        )?;
        Ok(())
    }

    /// Master automation switch for a whole channel (all its instances). Off =
    /// fully dormant. Independent from `enabled` (the Auto-record flag).
    pub fn set_channel_automation_enabled(&self, channel_id: i64, on: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE channel SET automation_enabled = ?2 WHERE id = ?1",
            params![channel_id, on as i64],
        )?;
        Ok(())
    }

    // ----- monitors -----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_monitor(&self, m: &Monitor) -> Result<i64> {
        let conn = self.db();
        conn.execute(
            "INSERT INTO monitor(channel_id, url, enabled, tool, detection_method, poll_interval_secs,
                quality, output_dir, filename_template, container, capture_from_start, auth_kind,
                auth_value, extra_args, max_concurrent, last_state, ad_free, audio_tracks, subtitle_tracks,
                chat_log, fetch_thumbnail, fetch_chat_assets, dual_capture, thumbnail_in_toast,
                sabr_codec_pref, sabr_codec_custom, automation_enabled)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
            params![
                m.channel_id,
                m.url,
                m.enabled as i64,
                m.tool.as_str(),
                m.detection_method.as_str(),
                m.poll_interval_secs,
                m.quality,
                m.output_dir,
                m.filename_template,
                m.container.as_str(),
                m.capture_from_start as i64,
                m.auth_kind.as_str(),
                m.auth_value,
                m.extra_args,
                m.max_concurrent,
                m.last_state,
                m.ad_free as i64,
                m.audio_tracks,
                m.subtitle_tracks,
                m.chat_log as i64,
                m.fetch_thumbnail as i64,
                m.fetch_chat_assets as i64,
                m.dual_capture as i64,
                m.thumbnail_in_toast as i64,
                m.sabr_codec_pref.id(),
                m.sabr_codec_custom,
                m.automation_enabled as i64,
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn update_monitor(&self, m: &Monitor) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET url=?2, enabled=?3, tool=?4, detection_method=?5, poll_interval_secs=?6,
                quality=?7, output_dir=?8, filename_template=?9, container=?10, capture_from_start=?11,
                auth_kind=?12, auth_value=?13, extra_args=?14, max_concurrent=?15, ad_free=?16,
                audio_tracks=?17, subtitle_tracks=?18, chat_log=?19,
                fetch_thumbnail=?20, fetch_chat_assets=?21, dual_capture=?22,
                thumbnail_in_toast=?23, sabr_codec_pref=?24, sabr_codec_custom=?25,
                automation_enabled=?26 WHERE id=?1",
            params![
                m.id,
                m.url,
                m.enabled as i64,
                m.tool.as_str(),
                m.detection_method.as_str(),
                m.poll_interval_secs,
                m.quality,
                m.output_dir,
                m.filename_template,
                m.container.as_str(),
                m.capture_from_start as i64,
                m.auth_kind.as_str(),
                m.auth_value,
                m.extra_args,
                m.max_concurrent,
                m.ad_free as i64,
                m.audio_tracks,
                m.subtitle_tracks,
                m.chat_log as i64,
                m.fetch_thumbnail as i64,
                m.fetch_chat_assets as i64,
                m.dual_capture as i64,
                m.thumbnail_in_toast as i64,
                m.sabr_codec_pref.id(),
                m.sabr_codec_custom,
                m.automation_enabled as i64,
            ],
        )?;
        Ok(())
    }

    /// Persist a detection result: last observed state + check timestamp.
    pub fn set_monitor_check_result(&self, id: i64, state: &str, checked_at: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_state = ?2, last_checked_at = ?3 WHERE id = ?1",
            params![id, state, checked_at],
        )?;
        Ok(())
    }

    /// Persist the last-detected live info on a monitor (title/game/thumbnail/
    /// viewers/go-live time), written on every poll regardless of the
    /// Auto-record flag so the grid can show a live channel's info — including
    /// Went Live/Started On/Duration — without a recording. Empty strings +
    /// `viewers = -1` clear stale info when a channel goes offline; `live_since
    /// = None` likewise clears the go-live time (a fresh value is stamped again
    /// the next time it's seen live).
    #[allow(clippy::too_many_arguments)]
    pub fn set_monitor_live_meta(
        &self,
        id: i64,
        title: &str,
        game: &str,
        thumbnail_url: &str,
        viewers: i64,
        live_since: Option<i64>,
        live_since_approx: bool,
    ) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_title = ?2, last_game = ?3,
                 last_thumbnail_url = ?4, last_viewers = ?5,
                 last_live_since = ?6, last_live_since_approx = ?7 WHERE id = ?1",
            params![id, title, game, thumbnail_url, viewers, live_since, live_since_approx as i64],
        )?;
        Ok(())
    }

    /// Make a monitor due for polling on the very next scheduler tick (resets
    /// `last_checked_at`), without touching its state — how an EventSub
    /// shared-chat push accelerates the collab refresh.
    pub fn mark_monitor_poll_due(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_checked_at = 0 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// The monitor's current live-collab JSON (`monitor.last_collab`; '' when
    /// none). Read back by the collab refresher's degraded path so a transient
    /// Shared Chat fetch failure keeps showing the last-known partners.
    pub fn monitor_last_collab(&self, id: i64) -> Result<String> {
        let conn = self.db();
        Ok(conn
            .query_row(
                "SELECT last_collab FROM monitor WHERE id = ?1",
                params![id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .unwrap_or_default())
    }

    /// Set (or clear, with `""`) the live "Stream Together" collab JSON shown
    /// by the grid ([`crate::models::CollabLive`]) — a narrow single-column
    /// setter like [`Self::set_monitor_viewers`], written by both the
    /// scheduler's poll and the in-recording `meta_watcher`.
    pub fn set_monitor_live_collab(&self, id: i64, collab_json: &str) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_collab = ?2 WHERE id = ?1",
            params![id, collab_json],
        )?;
        Ok(())
    }

    /// Update just the live viewer count, independent of `set_monitor_live_meta`
    /// — used by the in-recording `meta_watcher` (`downloader.rs`), which polls
    /// title/game/viewers directly while the scheduler skips an actively-
    /// recording monitor entirely. A narrow single-column setter so a viewer
    /// refresh can't clobber the thumbnail/go-live fields that only the
    /// scheduler's full poll outcome should own.
    pub fn set_monitor_viewers(&self, id: i64, viewers: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_viewers = ?2 WHERE id = ?1",
            params![id, viewers],
        )?;
        Ok(())
    }

    pub fn clear_channel_errors(&self, channel_id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET last_state = 'idle' WHERE channel_id = ?1 AND last_state IN ('error', 'failed')",
            params![channel_id],
        )?;
        Ok(())
    }

    pub fn set_monitor_enabled(&self, id: i64, enabled: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET enabled=?2 WHERE id=?1",
            params![id, enabled as i64],
        )?;
        Ok(())
    }

    /// Master automation switch for a single instance. Off = fully dormant.
    /// Independent from `enabled` (the Auto-record flag).
    pub fn set_monitor_automation_enabled(&self, id: i64, on: bool) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET automation_enabled=?2 WHERE id=?1",
            params![id, on as i64],
        )?;
        Ok(())
    }

    pub fn delete_monitor(&self, id: i64) -> Result<()> {
        let conn = self.db();
        conn.execute("DELETE FROM monitor WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Cache the auto-detected Twitch-subscription ad-free status for a monitor,
    /// but only while a Twitch account is connected, atomically under the single
    /// connection lock: the `EXISTS` check on `connected_key` and the update can't
    /// interleave with a concurrent `disconnect` (which clears that key + the
    /// cache), so a Disconnect landing mid-refresh can't resurrect a stale result.
    /// `sub` is `Some(true)` subscribed / `Some(false)` not / `None` unknown.
    /// Returns whether a row was written.
    pub fn set_monitor_ad_free_sub_if_connected(
        &self,
        id: i64,
        sub: Option<bool>,
        checked_at: i64,
        connected_key: &str,
    ) -> Result<bool> {
        let conn = self.db();
        let n = conn.execute(
            "UPDATE monitor SET ad_free_sub = ?2, ad_free_sub_at = ?3
             WHERE id = ?1
               AND EXISTS (SELECT 1 FROM app_settings WHERE key = ?4 AND value <> '')",
            params![id, sub.map(|b| b as i64), checked_at, connected_key],
        )?;
        Ok(n > 0)
    }

    /// Minimal Twitch-monitor rows for the ad-free refresher — just the fields it
    /// needs, avoiding the heavy channel/recording/ad-break join of
    /// [`Self::list_monitors_with_channels`] on its frequent poll tick.
    pub fn twitch_monitors_for_ad_free(&self) -> Result<Vec<AdFreeRow>> {
        let conn = self.db();
        let mut stmt = conn.prepare(
            "SELECT id, url, ad_free, ad_free_sub, ad_free_sub_at, last_state
             FROM monitor WHERE url LIKE '%twitch.tv%'",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(AdFreeRow {
                    id: r.get(0)?,
                    url: r.get(1)?,
                    ad_free: r.get::<_, i64>(2)? != 0,
                    ad_free_sub: r.get::<_, Option<i64>>(3)?.map(|v| v != 0),
                    ad_free_sub_at: r.get(4)?,
                    last_state: r.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Clear all cached auto Twitch-sub ad-free results (e.g. on disconnect).
    pub fn clear_ad_free_sub(&self) -> Result<()> {
        let conn = self.db();
        conn.execute(
            "UPDATE monitor SET ad_free_sub = NULL, ad_free_sub_at = NULL",
            [],
        )?;
        Ok(())
    }

    /// Fetch a single monitor (joined with its channel) by monitor id.
    pub fn get_monitor_with_channel(&self, id: i64) -> Result<Option<MonitorWithChannel>> {
        Ok(self
            .list_monitors_with_channels()?
            .into_iter()
            .find(|r| r.monitor.id == id))
    }

    /// The monitor and stream/video id a recording belongs to (so a
    /// `RecordingFinished` event can resolve the channel for a rich toast and
    /// build a platform-specific VOD URL when a video id is known).
    pub fn monitor_id_for_recording(
        &self,
        recording_id: i64,
    ) -> Result<Option<(i64, Option<String>)>> {
        let conn = self.db();
        let row = conn
            .query_row(
                "SELECT monitor_id, stream_id FROM recording WHERE id = ?1",
                params![recording_id],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        Ok(row)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::test_util::*;

    #[test]
    fn migrate_and_crud_roundtrip() {
        let store = Store::open_in_memory().unwrap();

        // upsert_channel is idempotent on URL.
        let c1 = store
            .upsert_channel("Alice", "https://twitch.tv/alice", Platform::Twitch)
            .unwrap();
        let c2 = store
            .upsert_channel("Alice", "https://twitch.tv/alice", Platform::Twitch)
            .unwrap();
        assert_eq!(c1, c2);

        // two monitor instances for the same channel (streamlink + yt-dlp).
        let mut m = sample_monitor(c1);
        let m1 = store.insert_monitor(&m).unwrap();
        m.tool = Tool::YtDlp;
        m.container = Container::Ts;
        // Exercise the SABR codec-pref columns round-tripping through the
        // positional read in list_monitors_with_channels (idx 48/49).
        m.sabr_codec_pref = SabrCodecPref::Custom;
        m.sabr_codec_custom = "res,fps,br".into();
        let m2 = store.insert_monitor(&m).unwrap();
        assert_ne!(m1, m2);

        let rows = store.list_monitors_with_channels().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.channel.id == c1));
        assert!(rows.iter().any(|r| r.monitor.container == Container::Ts));
        let r2 = rows.iter().find(|r| r.monitor.id == m2).unwrap();
        assert_eq!(r2.monitor.sabr_codec_pref, SabrCodecPref::Custom);
        assert_eq!(r2.monitor.sabr_codec_custom, "res,fps,br");
        // The other instance keeps the default Inherit.
        let r1 = rows.iter().find(|r| r.monitor.id == m1).unwrap();
        assert_eq!(r1.monitor.sabr_codec_pref, SabrCodecPref::Inherit);

        store.set_monitor_enabled(m1, false).unwrap();
        let rows = store.list_monitors_with_channels().unwrap();
        assert!(
            !rows
                .iter()
                .find(|r| r.monitor.id == m1)
                .unwrap()
                .monitor
                .enabled
        );

        store.delete_monitor(m2).unwrap();
        assert_eq!(store.list_monitors_with_channels().unwrap().len(), 1);

        // deleting the channel cascades to monitors.
        store.delete_channel(c1).unwrap();
        assert_eq!(store.list_monitors_with_channels().unwrap().len(), 0);
    }
    #[test]
    fn automation_switch_and_live_meta() {
        let store = Store::open_in_memory().unwrap();
        let cid = store
            .upsert_channel("Live One", "https://twitch.tv/live1", Platform::Twitch)
            .unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Both master switches default ON (new columns DEFAULT 1); automation_on
        // requires both channel + monitor.
        let row = |s: &Store| {
            s.list_monitors_with_channels()
                .unwrap()
                .into_iter()
                .find(|r| r.monitor.id == mid)
                .unwrap()
        };
        assert!(row(&store).monitor.automation_enabled);
        assert!(row(&store).channel.automation_enabled);
        assert!(row(&store).automation_on());

        // Disabling the instance's master switch turns automation off; the
        // Auto-record flag (`enabled`) is untouched.
        store.set_monitor_automation_enabled(mid, false).unwrap();
        let r = row(&store);
        assert!(!r.monitor.automation_enabled);
        assert!(!r.automation_on());
        assert!(r.monitor.enabled, "Auto-record flag independent of master");

        store.set_monitor_automation_enabled(mid, true).unwrap();
        store.set_channel_automation_enabled(cid, false).unwrap();
        assert!(!row(&store).automation_on(), "channel master gates too");
        store.set_channel_automation_enabled(cid, true).unwrap();
        assert!(row(&store).automation_on());

        // Live meta persists + round-trips; offline clears it.
        assert_eq!(row(&store).last_viewers, -1, "unknown by default");
        assert_eq!(row(&store).monitor.last_live_since, None, "unset by default");
        store
            .set_monitor_live_meta(
                mid, "Ranked grind", "VALORANT", "https://t/x.jpg", 1234, Some(1_000_000), true,
            )
            .unwrap();
        let r = row(&store);
        assert_eq!(r.last_title, "Ranked grind");
        assert_eq!(r.last_game, "VALORANT");
        assert_eq!(r.last_thumbnail_url, "https://t/x.jpg");
        assert_eq!(r.last_viewers, 1234);
        assert_eq!(r.monitor.last_live_since, Some(1_000_000));
        assert!(r.monitor.last_live_since_approx);

        store
            .set_monitor_live_meta(mid, "", "", "", -1, None, false)
            .unwrap();
        let r = row(&store);
        assert_eq!(r.last_title, "");
        assert_eq!(r.last_viewers, -1);
        assert_eq!(r.monitor.last_live_since, None, "cleared on offline");
    }
    #[test]
    fn create_container_with_flags_seeds_channel_switches() {
        let store = Store::open_in_memory().unwrap();

        // Plain create_container always defaults both switches on (schema
        // default) — the mismatch this feature avoids.
        let plain = store.create_container("Plain").unwrap();
        let ch = |s: &Store, id: i64| {
            s.list_channels().unwrap().into_iter().find(|c| c.id == id).unwrap()
        };
        assert!(ch(&store, plain).enabled);
        assert!(ch(&store, plain).automation_enabled);

        // Auto off + Enabled off on the seeding instance -> channel matches.
        let off = store.create_container_with_flags("Off", false, false).unwrap();
        assert!(!ch(&store, off).enabled);
        assert!(!ch(&store, off).automation_enabled);

        // Auto off + Enabled on -> channel matches independently per flag.
        let mixed = store.create_container_with_flags("Mixed", false, true).unwrap();
        assert!(!ch(&store, mixed).enabled);
        assert!(ch(&store, mixed).automation_enabled);
    }
    #[test]
    fn ad_free_flag_roundtrips() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        m.ad_free = true;
        let mid = store.insert_monitor(&m).unwrap();
        let row = store.get_monitor_with_channel(mid).unwrap().unwrap();
        assert!(row.monitor.ad_free);

        // update_monitor persists a cleared flag too.
        let mut m2 = row.monitor.clone();
        m2.ad_free = false;
        store.update_monitor(&m2).unwrap();
        assert!(!store.get_monitor_with_channel(mid).unwrap().unwrap().monitor.ad_free);
    }
    #[test]
    fn ad_free_sub_write_is_gated_on_connection() {
        let store = Store::open_in_memory().unwrap();
        let cid = store.create_container("Streamer").unwrap();
        let mut m = sample_monitor(cid);
        m.channel_id = cid;
        let mid = store.insert_monitor(&m).unwrap();

        // Not connected (key absent): the guarded write is a no-op.
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 100, "twitch_user_id")
            .unwrap();
        assert!(!wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            None
        );

        // Connected: the write lands.
        store.set_setting("twitch_user_id", "12345").unwrap();
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 100, "twitch_user_id")
            .unwrap();
        assert!(wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            Some(true)
        );

        // Disconnect (key emptied): a later write can't resurrect a value.
        store.set_setting("twitch_user_id", "").unwrap();
        store.clear_ad_free_sub().unwrap();
        let wrote = store
            .set_monitor_ad_free_sub_if_connected(mid, Some(true), 200, "twitch_user_id")
            .unwrap();
        assert!(!wrote);
        assert_eq!(
            store.get_monitor_with_channel(mid).unwrap().unwrap().ad_free_sub,
            None
        );
    }
}
