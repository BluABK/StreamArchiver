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
| 4 — Graceful finalize-on-stop, desktop notifications | ✅ |
| 4 — Twitch EventSub real-time push (conduit) | ✅ (needs live Twitch creds to verify) |
| 4 — Installer / packaging | ⏳ planned |

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
   downloads, and **start at login** (autostart). Folder fields have a **Browse…**
   button.

The channel table shows, per monitor: On (enable/disable), Name, Platform (with a
brand badge), Tool, Detection, poll interval, State, **Went Live** (the platform's
go-live time — `~`-prefixed when only our first-detected time is known, e.g. for
scrape), **Started On** (when we began recording), **Lost time** (Started On −
Went Live, i.e. how much of the stream we missed), **Duration** (live, `HH:MM:SS`),
and **Added** (when the channel was added).

### Detection methods

| Method | Platforms | Needs creds | Notes |
|---|---|---|---|
| Twitch API (Helix) | Twitch | Yes | Batched, scales to many channels; **default for Twitch**. |
| Twitch EventSub | Twitch | Yes (same app creds) | Real-time push via conduit + websocket (no polling, no public endpoint). Reconciles on connect. |
| Scrape poll | YouTube `/live`, Kick, generic | No | **Default for YouTube/Kick**; fragile to site changes. |
| Generic probe | any streamlink/yt-dlp URL | No | Uses `streamlink --stream-url` to test liveness. |

> To verify EventSub: set Twitch creds, add a Twitch channel with method **Twitch
> EventSub**, then `streamarchiver --run-for 120` with `RUST_LOG=info` — it logs
> `eventsub: connected (conduit …); N channel(s) subscribed` and
> `stream.online -> monitor N` when a channel goes live.

> Tool tip: use **streamlink for Twitch** (reaches 1440p/2K HEVC) and **yt-dlp for
> YouTube** (`--live-from-start`; streamlink hits YouTube segment 403s). The app
> defaults accordingly.

### Output

Recordings capture to a progressively-flushed `.ts` (so a crash/forced-stop leaves
usable data) and are remuxed losslessly to **`.mkv`** on clean stop. MKV is the
default; pick TS per channel if you prefer. **MP4 is never produced** (poor for
interrupted writes). Filename template variables: `{name} {date} {time} {timestamp}`.

### Authentication

Two separate concerns:

**Platform API (detection).** Twitch detection uses OAuth2. Either:
- **Client ID + Secret** in Settings → app-token (client-credentials) flow, or
- **Connect Twitch** (Settings → *Twitch account*) → OAuth2 **device-code** login
  (also `streamarchiver --twitch-login`). Stores a refreshable **user token**;
  detection then prefers it, so the Client Secret becomes optional. Register a
  free app at <https://dev.twitch.tv/console/apps> to get a Client ID.

**Authenticated downloads** (sub-only / members-only / ad-reduced / higher quality).
Set a global default in Settings → *Download authentication*, and/or override
per channel in the add/edit form (a per-channel value always wins):
- **Browser cookies** → yt-dlp `--cookies-from-browser <browser>` (works for
  Twitch sub/Turbo and YouTube members).
- **Cookies file** → yt-dlp `--cookies <cookies.txt>`.
- **Auth token** → streamlink `--twitch-api-header=Authorization=OAuth <token>`
  for Twitch.

> Note: streamlink (Twitch) authenticates via the token header; yt-dlp uses
> cookies. The form offers each tool the form it actually supports.

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
streamarchiver --twitch-login                     # OAuth2 device-code Connect flow
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

- YouTube Data API / Kick official API detectors (current scrape works without keys).
- Installer + AppUserModelID (for branded Windows toast notifications).
- macOS/Linux polish (tray via `ksni`, process-group kill).
