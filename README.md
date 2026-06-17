# StreamArchiver

A lightweight, always-on desktop app (Windows-first, cross-platform-friendly) that
monitors an arbitrary number of channels/links, detects when they go **live**, and
automatically records them using `streamlink`, `yt-dlp`, and `ffmpeg`.

Written in **Rust** with a **native egui UI** (no web/Chromium). Runs in the system
tray with the window opened on demand; idle CPU is ~0% and the resident footprint is
small.

## Status

| Phase | State |
|---|---|
| 1 — Tray app, on-demand UI, SQLite store, settings, autostart | ✅ |
| 2 — Shared batched poll scheduler + detectors (Twitch API, YouTube/Kick scrape, generic probe) | ✅ |
| 3 — Download supervisor (record → `.ts`, remux → MKV, tree-kill, backoff, orphan recovery) | ✅ |
| 4 — Graceful finalize-on-stop, desktop notifications | ✅ (in progress) |
| 4 — Twitch EventSub real-time push, installer | ⏳ planned |

## Requirements

- **Runtime tools** on `PATH`: [`streamlink`](https://streamlink.github.io/),
  [`yt-dlp`](https://github.com/yt-dlp/yt-dlp), [`ffmpeg`](https://ffmpeg.org/).
- **To build**: Rust (stable) + the MSVC toolchain on Windows.

## Build & run

```sh
cargo build --release
./target/release/streamarchiver        # opens the window; closing it hides to tray
./target/release/streamarchiver --hidden   # start straight to the tray (used by autostart)
```

Right-click the tray icon → **Open** / **Quit**. Quitting gracefully stops any active
recordings (finalizing the MKV) before exiting.

## Using it

1. **Add channel** → paste a URL (platform auto-detected). Pick the **tool**, the
   **detection method** (platform-filtered), poll interval, quality, **container**
   (MKV default), output folder, and an optional filename template.
2. **＋Inst** adds another capture *instance* for the same channel — e.g. run both
   `streamlink` and `yt-dlp` on one channel into different folders.
3. **Settings** → Twitch/YouTube credentials, default output folder, max concurrent
   downloads, and **start at login** (autostart).

### Detection methods

| Method | Platforms | Needs creds | Notes |
|---|---|---|---|
| Twitch API (Helix) | Twitch | Yes | Batched, scales to many channels; **default for Twitch**. |
| Scrape poll | YouTube `/live`, Kick, generic | No | **Default for YouTube/Kick**; fragile to site changes. |
| Generic probe | any streamlink/yt-dlp URL | No | Uses `streamlink --stream-url` to test liveness. |

> Tool tip: use **streamlink for Twitch** (reaches 1440p/2K HEVC) and **yt-dlp for
> YouTube** (`--live-from-start`; streamlink hits YouTube segment 403s). The app
> defaults accordingly.

### Output

Recordings capture to a progressively-flushed `.ts` (so a crash/forced-stop leaves
usable data) and are remuxed losslessly to **`.mkv`** on clean stop. MKV is the
default; pick TS per channel if you prefer. **MP4 is never produced** (poor for
interrupted writes). Filename template variables: `{name} {date} {time} {timestamp}`.

### Twitch credentials (for the Twitch API method)

1. Create a free app at <https://dev.twitch.tv/console/apps> (any OAuth redirect, e.g.
   `http://localhost`).
2. Copy the **Client ID** and a **Client Secret** into **Settings**. Detection uses the
   client-credentials (app-token) flow — no per-user login.

## Data & locations

- Config/state DB: `%APPDATA%\StreamArchiver\data\streamarchiver.sqlite3` (SQLite, WAL).
- Override the DB path with `STREAMARCHIVER_DB`, default output dir with
  `STREAMARCHIVER_OUT` (handy for testing).

## CLI / diagnostics

```sh
streamarchiver --probe <url>                      # one-shot live check
streamarchiver --add "<name>" <url> [method] [tool]
streamarchiver --list                             # monitors + state
streamarchiver --recordings                       # recent recording log
streamarchiver --capture-test <tool> <url> <secs> # record N s, kill tree, remux
streamarchiver --run-for <secs>                   # headless: run core then stop
streamarchiver --hidden                           # start to tray (no window)
```

## Architecture

Single binary; the tokio core (scheduler + download supervisor) runs regardless of the
window. One shared scheduler batches detection (e.g. one Twitch Helix call covers up to
100 channels) rather than one thread/process per channel. The supervisor spawns tools as
child processes, captures logs, and kills whole process trees on stop. State lives in
SQLite; the UI subscribes to an event bus (no hot-polling).

```
tray ── open/quit ──► core (tokio): store · scheduler · detectors · supervisor · events
                                   └── child processes: streamlink / yt-dlp / ffmpeg
egui window (on demand) ◄── events ──┘
```

## Roadmap

- Twitch **EventSub** conduit (real-time push; lower latency than polling).
- YouTube Data API / Kick official API detectors.
- Installer + AppUserModelID (for branded Windows toast notifications).
- macOS/Linux polish (tray via `ksni`, process-group kill).
