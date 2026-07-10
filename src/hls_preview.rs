//! Live HLS playlist generation over growing SABR fragmented-MP4 files.
//!
//! A SABR live-edge preview download produces two separate growing fMP4 files
//! (video + audio). No mpv-native mechanism follows them reliably at the live
//! edge: `appending://` retries reads at EOF for only ~2 s, which loses the
//! race against SABR's segment cadence once and then the demuxer latches EOF
//! permanently (seek nudges land in the demuxer cache and never touch the
//! file). ffmpeg's HLS demuxer, however, is *designed* to poll a live
//! playlist forever — so we hand the player a hand-rolled local HLS master:
//! each media playlist maps the file's `ftyp+moov` init via `EXT-X-MAP` and
//! addresses runs of `moof+mdat` fragments as `EXT-X-BYTERANGE` segments into
//! the growing file. An updater rewrites the playlists every couple of
//! seconds as new fragments land. Verified against a live capture: mpv plays
//! 1:1 with wall clock at the edge with ~40 s of readahead and never EOFs.
//!
//! SABR writes a moof/mdat pair every ~16 ms, far too granular for playlist
//! entries — fragments are coalesced into ~[`SEG_SECS`]-second segments using
//! the `tfdt` timestamp deltas.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Target coalesced HLS segment duration, in media time.
const SEG_SECS: f64 = 3.0;

/// One coalesced playlist segment: a byte range covering whole moof+mdat
/// fragments, starting at `tfdt` (in the track's timescale).
#[derive(Clone, Copy, Debug)]
struct Segment {
    offset: u64,
    len: u64,
    tfdt: Option<u64>,
}

/// Incremental top-level ISOBMFF box walker over a growing fMP4.
struct Scanner {
    file: File,
    /// Next unparsed top-level box offset.
    off: u64,
    /// Byte length of the leading `ftyp+moov` init segment, once seen.
    init_end: Option<u64>,
    /// Media timescale from the first `moov > trak > mdia > mdhd`.
    timescale: u32,
    /// Coalesced segments, append-only.
    segments: Vec<Segment>,
    /// Start of the currently-open (not yet emitted) group.
    group: Option<(u64, Option<u64>)>,
    /// `moof` awaiting its `mdat`: (offset, tfdt).
    pending_moof: Option<(u64, Option<u64>)>,
}

/// Read a box header at `off`: (total box size, fourcc). `None` on a short or
/// nonsense read (box still being written, or not ISOBMFF).
fn box_header(f: &mut File, off: u64) -> Option<(u64, [u8; 4])> {
    let mut hdr = [0u8; 8];
    f.seek(SeekFrom::Start(off)).ok()?;
    f.read_exact(&mut hdr).ok()?;
    let mut size = u32::from_be_bytes([hdr[0], hdr[1], hdr[2], hdr[3]]) as u64;
    let typ = [hdr[4], hdr[5], hdr[6], hdr[7]];
    if size == 1 {
        let mut big = [0u8; 8];
        f.read_exact(&mut big).ok()?;
        size = u64::from_be_bytes(big);
    }
    (size >= 8).then_some((size, typ))
}

/// Find the first direct child box named `name` within `[off, end)`.
fn find_child(f: &mut File, mut off: u64, end: u64, name: &[u8; 4]) -> Option<(u64, u64)> {
    while off + 8 <= end {
        let (size, typ) = box_header(f, off)?;
        if &typ == name {
            return Some((off, size));
        }
        off += size;
    }
    None
}

/// Timescale from the first `trak > mdia > mdhd` inside `moov` (one track per
/// SABR per-format file).
fn mdhd_timescale(f: &mut File, moov_off: u64, moov_size: u64) -> Option<u32> {
    let end = moov_off + moov_size;
    let (trak, tsz) = find_child(f, moov_off + 8, end, b"trak")?;
    let (mdia, msz) = find_child(f, trak + 8, trak + tsz, b"mdia")?;
    let (mdhd, _) = find_child(f, mdia + 8, mdia + msz, b"mdhd")?;
    let mut ver = [0u8; 1];
    f.seek(SeekFrom::Start(mdhd + 8)).ok()?;
    f.read_exact(&mut ver).ok()?;
    // v0: 4B flags-rest + 2×4B times; v1: + 2×8B times.
    let ts_off = mdhd + 8 + 4 + if ver[0] == 1 { 16 } else { 8 };
    let mut ts = [0u8; 4];
    f.seek(SeekFrom::Start(ts_off)).ok()?;
    f.read_exact(&mut ts).ok()?;
    Some(u32::from_be_bytes(ts))
}

/// `baseMediaDecodeTime` from `moof > traf > tfdt`.
fn tfdt_time(f: &mut File, moof_off: u64, moof_size: u64) -> Option<u64> {
    let end = moof_off + moof_size;
    let (traf, tsz) = find_child(f, moof_off + 8, end, b"traf")?;
    let (tfdt, _) = find_child(f, traf + 8, traf + tsz, b"tfdt")?;
    let mut ver = [0u8; 1];
    f.seek(SeekFrom::Start(tfdt + 8)).ok()?;
    f.read_exact(&mut ver).ok()?;
    f.seek(SeekFrom::Start(tfdt + 12)).ok()?;
    if ver[0] == 1 {
        let mut t = [0u8; 8];
        f.read_exact(&mut t).ok()?;
        Some(u64::from_be_bytes(t))
    } else {
        let mut t = [0u8; 4];
        f.read_exact(&mut t).ok()?;
        Some(u32::from_be_bytes(t) as u64)
    }
}

impl Scanner {
    /// Open a growing file, verifying it starts with an ISOBMFF `ftyp`.
    fn open(path: &Path) -> Option<Scanner> {
        let mut file = crate::iomon::fs::open_sync(crate::iomon::Cat::Preview, path).ok()?;
        let (_, typ) = box_header(&mut file, 0)?;
        if &typ != b"ftyp" {
            return None;
        }
        Some(Scanner {
            file,
            off: 0,
            init_end: None,
            timescale: 90_000,
            segments: Vec::new(),
            group: None,
            pending_moof: None,
        })
    }

    /// True current size via the open handle (directory-entry sizes are stale
    /// for files another process is writing).
    fn size(&mut self) -> u64 {
        self.file.seek(SeekFrom::End(0)).unwrap_or(0)
    }

    /// Parse any newly-landed complete top-level boxes, coalescing fragments.
    fn scan(&mut self) {
        let end = self.size();
        while self.off + 8 <= end {
            let Some((size, typ)) = box_header(&mut self.file, self.off) else { break };
            if self.off + size > end {
                break; // box still being written
            }
            match &typ {
                b"moov" if self.init_end.is_none() => {
                    // SABR re-appends a fresh ftyp+moov after reconnects; the
                    // EXT-X-MAP must reference only the FIRST init.
                    if let Some(ts) = mdhd_timescale(&mut self.file, self.off, size) {
                        self.timescale = ts.max(1);
                    }
                    self.init_end = Some(self.off + size);
                }
                b"moof" => {
                    self.pending_moof =
                        Some((self.off, tfdt_time(&mut self.file, self.off, size)));
                }
                b"mdat" => {
                    if let Some((moof_off, t)) = self.pending_moof.take() {
                        match self.group {
                            None => self.group = Some((moof_off, t)),
                            Some((g_off, Some(g_t))) => {
                                if let Some(t) = t {
                                    let secs =
                                        (t.saturating_sub(g_t)) as f64 / self.timescale as f64;
                                    if secs >= SEG_SECS {
                                        self.segments.push(Segment {
                                            offset: g_off,
                                            len: moof_off - g_off,
                                            tfdt: Some(g_t),
                                        });
                                        self.group = Some((moof_off, Some(t)));
                                    }
                                }
                            }
                            Some((_, None)) => {
                                // No timestamp on the group start; restart it.
                                self.group = Some((moof_off, t));
                            }
                        }
                    }
                }
                _ => {}
            }
            self.off += size;
        }
    }

    /// Segment durations in seconds. Each segment ends where the next one
    /// starts; the last emitted segment ends at the open group's start.
    fn durations(&self) -> Vec<f64> {
        let mut durs = Vec::with_capacity(self.segments.len());
        for (i, s) in self.segments.iter().enumerate() {
            let next_t = match self.segments.get(i + 1) {
                Some(n) => n.tfdt,
                None => self.group.and_then(|(_, t)| t),
            };
            let d = match (s.tfdt, next_t) {
                (Some(a), Some(b)) if b > a => (b - a) as f64 / self.timescale as f64,
                _ => durs.last().copied().unwrap_or(SEG_SECS),
            };
            durs.push(f64::max(d, 0.04));
        }
        durs
    }
}

/// Atomically replace `dst` with `content`, retrying briefly: the player may
/// hold the playlist open at reload time, and on Windows a rename onto an
/// open file fails with a sharing violation. A skipped cycle just means the
/// playlist lags one update.
fn replace_playlist(dst: &Path, content: &str) {
    use crate::iomon::Cat;
    let tmp = dst.with_extension("m3u8.tmp");
    if crate::iomon::fs::write_sync(Cat::Preview, &tmp, content).is_err() {
        return;
    }
    for _ in 0..10 {
        if crate::iomon::fs::rename_sync(Cat::Preview, &tmp, dst).is_ok() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    let _ = crate::iomon::fs::remove_file_sync(Cat::Preview, &tmp);
}

/// Render a media playlist over the scanner's segments. `media_uri` is the
/// URI of the growing file relative to the playlist. `ended` appends
/// `EXT-X-ENDLIST` (the preview download finished — lets the player end
/// cleanly instead of polling forever).
fn media_playlist(scan: &Scanner, media_uri: &str, ended: bool) -> Option<String> {
    let init_end = scan.init_end?;
    if scan.segments.len() < 2 {
        return None;
    }
    let durs = scan.durations();
    let target = durs.iter().fold(0.0f64, |a, &b| a.max(b));
    let mut s = String::with_capacity(scan.segments.len() * 96 + 256);
    s.push_str("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MEDIA-SEQUENCE:0\n");
    s.push_str(&format!("#EXT-X-TARGETDURATION:{}\n", target.ceil() as u64 + 1));
    s.push_str(&format!("#EXT-X-MAP:URI=\"{media_uri}\",BYTERANGE=\"{init_end}@0\"\n"));
    for (seg, d) in scan.segments.iter().zip(&durs) {
        s.push_str(&format!(
            "#EXTINF:{d:.3},\n#EXT-X-BYTERANGE:{}@{}\n{media_uri}\n",
            seg.len, seg.offset
        ));
    }
    if ended {
        s.push_str("#EXT-X-ENDLIST\n");
    }
    Some(s)
}

/// Live HLS generator for a SABR split-A/V preview: playlists live in
/// `out_dir`, addressing the two growing files. Call [`Self::tick`]
/// periodically; it returns `true` once `master.m3u8` is ready to play.
pub(crate) struct HlsPreview {
    video: Scanner,
    audio: Scanner,
    video_uri: String,
    audio_uri: String,
    out_dir: PathBuf,
    master_written: bool,
}

impl HlsPreview {
    /// `video`/`audio` are the growing fMP4s (largest-first order from the
    /// stream-target probe). Returns `None` unless both parse as ISOBMFF —
    /// the caller falls back to plain `appending://` playback (e.g. for the
    /// VP9-in-MKV SABR variant).
    pub(crate) fn open(video: &Path, audio: &Path, out_dir: &Path) -> Option<HlsPreview> {
        // Relative URI from the playlist dir to the media file (same dir or a
        // subdir like `.cache/…`); forward slashes for URI use.
        let rel_uri = |p: &Path| -> Option<String> {
            let rel = p.strip_prefix(out_dir).ok()?;
            Some(rel.to_string_lossy().replace('\\', "/"))
        };
        Some(HlsPreview {
            video: Scanner::open(video)?,
            audio: Scanner::open(audio)?,
            video_uri: rel_uri(video)?,
            audio_uri: rel_uri(audio)?,
            out_dir: out_dir.to_path_buf(),
            master_written: false,
        })
    }

    pub(crate) fn master_path(&self) -> PathBuf {
        self.out_dir.join("master.m3u8")
    }

    /// Scan for new fragments and rewrite the playlists. Returns `true` once
    /// the master playlist exists (both media playlists have ≥2 segments).
    pub(crate) fn tick(&mut self, ended: bool) -> bool {
        self.video.scan();
        self.audio.scan();
        let v = media_playlist(&self.video, &self.video_uri, ended);
        let a = media_playlist(&self.audio, &self.audio_uri, ended);
        if let Some(v) = &v {
            replace_playlist(&self.out_dir.join("video.m3u8"), v);
        }
        if let Some(a) = &a {
            replace_playlist(&self.out_dir.join("audio.m3u8"), a);
        }
        if !self.master_written && v.is_some() && a.is_some() {
            let master = "#EXTM3U\n#EXT-X-VERSION:7\n\
                 #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"a\",NAME=\"audio\",DEFAULT=YES,URI=\"audio.m3u8\"\n\
                 #EXT-X-STREAM-INF:BANDWIDTH=6000000,AUDIO=\"a\"\nvideo.m3u8\n";
            replace_playlist(&self.master_path(), master);
            self.master_written = true;
        }
        self.master_written
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn boxed(typ: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::with_capacity(8 + payload.len());
        v.extend_from_slice(&((payload.len() as u32 + 8).to_be_bytes()));
        v.extend_from_slice(typ);
        v.extend_from_slice(payload);
        v
    }

    /// Minimal moov with trak>mdia>mdhd carrying `timescale` (v0 layout).
    fn moov(timescale: u32) -> Vec<u8> {
        let mut mdhd = vec![0u8; 4]; // version+flags
        mdhd.extend_from_slice(&[0u8; 8]); // creation+modification (v0)
        mdhd.extend_from_slice(&timescale.to_be_bytes());
        mdhd.extend_from_slice(&[0u8; 4]); // duration
        mdhd.extend_from_slice(&[0u8; 4]); // language+quality
        let mdia = boxed(b"mdia", &boxed(b"mdhd", &mdhd));
        let trak = boxed(b"trak", &mdia);
        boxed(b"moov", &trak)
    }

    /// moof with traf>tfdt (v1) at `t`.
    fn moof(t: u64) -> Vec<u8> {
        let mut tfdt = vec![1u8, 0, 0, 0]; // version 1 + flags
        tfdt.extend_from_slice(&t.to_be_bytes());
        boxed(b"moof", &boxed(b"traf", &boxed(b"tfdt", &tfdt)))
    }

    fn write_fmp4(path: &std::path::Path, times_ms: &[u64]) {
        let mut f = std::fs::File::create(path).unwrap();
        f.write_all(&boxed(b"ftyp", b"isom")).unwrap();
        f.write_all(&moov(1000)).unwrap(); // timescale 1000 = ms
        for &t in times_ms {
            f.write_all(&moof(t)).unwrap();
            f.write_all(&boxed(b"mdat", &[0u8; 32])).unwrap();
        }
    }

    #[test]
    fn scanner_coalesces_fragments_into_segments() {
        let dir = std::env::temp_dir().join(format!(
            "sa_hls_{}_{}",
            std::process::id(),
            crate::models::now_unix()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let media = dir.join("preview.f400.mp4.sq0.part");
        // Fragments every second; groups close when the NEXT group's start is
        // ≥3s later: expect boundaries at 0→3000 and 3000→6500.
        write_fmp4(&media, &[0, 1000, 2000, 3000, 4000, 5000, 6500, 7000]);

        let mut sc = Scanner::open(&media).unwrap();
        sc.scan();
        assert_eq!(sc.timescale, 1000);
        assert!(sc.init_end.is_some());
        assert_eq!(sc.segments.len(), 2);
        assert_eq!(sc.segments[0].tfdt, Some(0));
        assert_eq!(sc.segments[1].tfdt, Some(3000));
        let durs = sc.durations();
        assert!((durs[0] - 3.0).abs() < 1e-9);
        assert!((durs[1] - 3.5).abs() < 1e-9);

        // Growing file: appending more fragments extends the segment list.
        {
            let mut f = std::fs::OpenOptions::new().append(true).open(&media).unwrap();
            f.write_all(&moof(10_000)).unwrap();
            f.write_all(&boxed(b"mdat", &[0u8; 32])).unwrap();
        }
        sc.scan();
        assert_eq!(sc.segments.len(), 3); // 6500→10000 closed

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn playlist_has_map_byteranges_and_endlist() {
        let dir = std::env::temp_dir().join(format!(
            "sa_hlspl_{}_{}",
            std::process::id(),
            crate::models::now_unix()
        ));
        let cache = dir.join(".cache");
        std::fs::create_dir_all(&cache).unwrap();
        let media = cache.join("preview.f400.mp4.sq0.part");
        write_fmp4(&media, &[0, 3000, 6000, 9000]);

        let audio = cache.join("preview.f140.m4a.sq0.part");
        write_fmp4(&audio, &[0, 3000, 6000, 9000]);

        let mut hp = HlsPreview::open(&media, &audio, &dir).unwrap();
        assert!(hp.tick(false));
        let v = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
        assert!(v.contains("#EXT-X-MAP:URI=\".cache/preview.f400.mp4.sq0.part\""));
        assert!(v.contains("#EXT-X-BYTERANGE:"));
        assert!(!v.contains("#EXT-X-ENDLIST"));
        assert!(dir.join("master.m3u8").is_file());
        assert!(dir.join("audio.m3u8").is_file());

        hp.tick(true);
        let v = std::fs::read_to_string(dir.join("video.m3u8")).unwrap();
        assert!(v.ends_with("#EXT-X-ENDLIST\n"));

        // Non-ISOBMFF input is rejected (caller falls back to appending://).
        let mkv = cache.join("preview.f303.mkv.sq0.part");
        std::fs::write(&mkv, [0x1A, 0x45, 0xDF, 0xA3, 0, 0, 0, 0, 0, 0]).unwrap();
        assert!(HlsPreview::open(&mkv, &audio, &dir).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
