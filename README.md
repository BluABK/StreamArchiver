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

The window has three tabs: **Streams** (monitor channels for live broadcasts),
**Videos** (on-demand downloads), and **Settings**.

## Using it

### Streams (live monitoring)

1. **Add stream** → paste a URL (platform auto-detected). Pick the **tool**, the
   **detection method** (platform-filtered), poll interval, quality, **container**
   (MKV default), output folder, and an optional filename template.
2. **＋Inst** adds another capture *instance* for the same channel — e.g. run both
   `streamlink` and `yt-dlp` on one channel into different folders.
3. **Settings** → Twitch/YouTube credentials, default output folder, max concurrent
   downloads, and **start at login** (autostart). Folder fields have a **Browse…**
   button.

### Videos (on-demand downloads)

The **Videos** tab downloads a *specific* video or VOD now — a YouTube video, a
Twitch VOD, or any `streamlink`/`yt-dlp` URL — rather than watching a channel for
live streams. Paste a URL in the form at the bottom, adjust the settings shown
alongside it (**tool**, **quality**, **auth**, output folder, filename template,
extra args), and click **Download**. Output is always **MKV** (yt-dlp remuxes to
MKV; streamlink/ffmpeg capture to `.ts` then remux). Downloads share the same
global concurrency limit as live recordings.

**Per-platform defaults.** The form pre-fills from saved defaults for the pasted
URL's platform; edit any field to override it for that download. The
**⚙ Per-platform defaults** panel to the right of the download form sets the
default tool/quality/auth/output/filename/extra args for Twitch, YouTube, Kick,
and Generic (each collapsible) — saved automatically. The form's **Auth** has a **Default
(per-platform)** option (selected by default, uses the platform default's auth)
plus the explicit choices; **Inherit (global)** stays available and chains to the
Settings → *Download authentication* default.

Each row shows status (`queued` → `downloading` → `completed`/`failed`/`stopped`)
and size, with per-row actions: **Stop**, **Retry**, **Open folder**, **Open
file**, **Copy URL**, and **Delete** (removes the row; the file is kept). A
download left in flight by a crash/quit is marked `orphaned` on the next start.

The channel table shows, per monitor: On (enable/disable), Name, Platform (with a
brand badge), Tool, Detection, poll interval, State, **Went Live** (the platform's
go-live time — `~`-prefixed when only our first-detected time is known, e.g. for
scrape), **Started On** (when we began recording), **Lost time** (Started On −
Went Live, i.e. how much of the stream we missed), **Duration** (live, `HH:MM:SS`),
and **Added** (when the channel was added).

### Row actions & shortcuts

Left-click a row to select it; **right-click** any row for a context menu:
Start/Stop recording, **Open channel URL** (browser), **Open output folder** (file
manager), **Copy URL**, Edit…, Add tool instance, Enable/Disable, and Delete. The
inline per-row buttons (▶/⏹ ✏ ➕ 🗑) do the same.

Keyboard shortcuts:

| Key | Action |
|---|---|
| `Ctrl/Cmd+N` | Add channel |
| `Ctrl/Cmd+,` | Open Settings |
| `F5` | Refresh the list |
| `Enter` | Edit the selected row |
| `Delete` | Delete the selected row |
| `Esc` | Close the open dialog |

Deleting always asks for confirmation (the recorded files are kept either way).

### Detection methods

A monitor's **Detection** method is *how* the app learns a channel went live. The
dropdown is filtered to the methods valid for the channel's platform, with a
sensible default pre-selected. Hover the **Detection** field (or the table column)
in-app for a one-line description of each.

| Method | Platforms | Needs creds | Latency | Notes |
|---|---|---|---|---|
| **Twitch API (Helix)** | Twitch | Client ID + Secret, or a connected account | one poll interval | Polls `Get Streams`, batched up to 100 channels/call; scales well. **Default for Twitch.** |
| **Twitch EventSub** | Twitch | Client ID + Secret | ~seconds | Real-time push over a WebSocket (conduit + app token); ignores the poll interval, idles cheaply, reconciles on (re)connect. No public endpoint needed. |
| **YouTube Data API** | YouTube | API key | one poll interval | `search.list?eventType=live`; reports the real go-live time. **Quota-limited (~100 checks/day)** — use a long interval. |
| **Kick official API** | Kick | Client ID + Secret | one poll interval | client-credentials app token; more reliable than scraping (no Cloudflare). |
| **Scrape poll** | YouTube `/live`, Kick, generic | No | one poll interval | **Default for YouTube/Kick**; no credentials, but fragile to site changes. Go-live time is approximate (`~`). |
| **Generic probe** | any streamlink/yt-dlp URL | No | one poll interval | `streamlink --stream-url` liveness test; works anywhere those tools do. |

**Polling vs. push (Helix vs. EventSub).** Helix *asks* "is it live?" every poll
interval, so you notice within that interval (and the **Lost time** column ≈ the
interval). EventSub is *told* the moment a channel goes live, so it catches the
start within seconds and ignores the per-channel interval — at the cost of holding
a WebSocket. Both report the real go-live time and use the same Twitch app creds;
EventSub specifically needs the **Client Secret** (it authenticates with an app
token). Choose **EventSub** to minimize missed footage, **Helix** for a simpler,
fully stateless poll. (Connecting a Twitch account also satisfies Helix — its user
token expires, so the app auto-refreshes it and falls back to the app token; if
you'd rather not reconnect, set a Client Secret and the app token is used.)

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

**Platform API (detection).** OAuth2 / API-key, per platform (all optional —
scrape works without any):
- **Twitch** → Client ID + Secret (app token) *or* **Connect Twitch** (Settings →
  *Twitch account*) OAuth2 **device-code** login (also `--twitch-login`), which
  stores a refreshable user token detection prefers (Secret then optional).
  Register at <https://dev.twitch.tv/console/apps>.
- **YouTube** → **API key** (Settings) enables the *YouTube Data API* method.
  Create one in a Google Cloud project with the YouTube Data API v3 enabled.
- **Kick** → **Client ID + Secret** (Settings) enables the *Kick official API*
  method (client-credentials app token). Register at <https://kick.com/settings/developer>.

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
