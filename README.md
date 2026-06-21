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

A **channel** is a *container* (just a name) holding one or more **instances**.
Each instance has its **own URL/platform** + tool + detection + output, so one
channel can mix sources — e.g. the same creator on **Twitch *and* YouTube**, or
two tools on one URL.

1. **Add stream** → name the channel and add its **first instance**: paste a URL
   (platform auto-detected; tool + detection default to it), then adjust poll
   interval, quality, **container** (MKV default), output folder, filename
   template, auth. (Or **Add channel** to create an empty container and add
   instances to it afterwards.)
2. **➕** on a channel row (or **Add instance to channel** in the menu) adds
   another instance — including one on a **different platform** (paste a YouTube
   URL on a Twitch channel, etc.).
3. **On** toggles work at both levels: the **channel** checkbox enables/disables
   *all* its instances at once; each **instance** has its own checkbox (disable
   just YouTube for the day, keep Twitch). **✏** renames the channel; the
   per-instance **✏** edits that instance (incl. its URL). **🗑** deletes a
   channel (and its instances) or a single instance.
4. **Settings** → Twitch/YouTube credentials, default output folder, max concurrent
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

**List formats.** Click **List formats** to probe the URL with the selected tool
(`yt-dlp --list-formats`, streamlink's stream list, or `ffprobe`) and show the
available formats/qualities in a window — handy for picking a **Quality** value.

**Auto-detect title + channel.** Tick **Auto-detect** to look up the real
title *and* channel/uploader (via yt-dlp) at download time. These populate the
**Channel** column and the `{title}`/`{channel}` template variables — and
`{title}` is used for `{name}` when **Name** is left blank (so files aren't named
`video_…`). See [Filename templates](#filename-templates) for the full variable
list.

**Per-platform defaults.** The form pre-fills from saved defaults for the pasted
URL's platform; edit any field to override it for that download. The
**⚙ Per-platform defaults** panel to the right of the download form sets the
default tool/quality/auth/output/filename/extra args for Twitch, YouTube, Kick,
and Generic (each collapsible) — saved automatically. The form's **Auth** has a **Default
(per-platform)** option (selected by default, uses the platform default's auth)
plus the explicit choices; **Inherit (global)** stays available and chains to the
Settings → *Download authentication* default.

Each row shows the title, **Channel** (when detected), status (`queued` →
`downloading` → `completed`/`failed`/`stopped`), live **Speed** (download rate
while active; yt-dlp downloads only), size, and the **File** path on disk. Per-row
inline actions plus a **right-click context menu** offer: **Open file**, **Open
folder**, **Copy URL**, **Copy file path**, **Stop**/**Retry**, and **Delete**
(removes the row; the file is kept). A download left in flight by a crash/quit is
marked `orphaned` on the next start.

**Sort & filter.** Click any column header to sort by it (click again to reverse;
a ▲/▼ shows the active column); type in the box under a header to filter that
column (case-insensitive substring). Filters combine across columns. This works on
the **Videos** and **Streams** tables alike.

The channel table shows, per channel: On (enable/disable), Name, Platform (with a
brand badge), Tool, Detection, **Polled** (when it was last checked, with the poll
interval in parentheses — e.g. `2026-06-21 14:02:33 (60s)`), State, **Game** and
**Title** (the current category/title of the latest recording), **Went Live** (the
platform's go-live time — `~`-prefixed when only our first-detected time is known,
e.g. for scrape), **Started On** (when we began recording), **Lost time** (how
much of the stream we missed), **Duration** (live, `HH:MM:SS`), and **Added** (when
the channel was added). Timestamps follow the **Date format** chosen in Settings
(default ISO).

> The console log (run with `RUST_LOG=info,streamarchiver=debug`, the default)
> reports detection: a `DEBUG scheduler: polling N monitor(s) due […]` line per
> cycle, a `DEBUG poll: <name> [<method>] <result>` line per check, and an
> `INFO poll: <name> [<method>] <old> -> <new>` line whenever a channel's state
> changes (with the go-live time when it goes live, or the error detail).

**Recording history (collapsible).** Each channel row is a tree you can expand
(the ▶ triangle) to see its **past streams**, and each stream that took more than
one attempt expands again to its individual **takes**:

```
▼ Layna            twitch  streamlink  recording
   ▼ 🎬 2026-06-20 18:00   recording   · 2 takes
        Take 1   18:00–18:12   failed       (crashed)
        Take 2   18:13–…       recording
   ▶ 🎬 2026-06-19 21:30   completed
```

A channel with **multiple capture instances** (e.g. streamlink *and* yt-dlp on the
same channel) instead expands to one row per instance, and each instance expands
to its own streams → takes. The app groups attempts into one stream by the
platform's **stream/video id** when detection knows it (Twitch Helix/EventSub,
YouTube Data API, Kick API); for id-less methods (scrape/probe) it groups attempts
that share a go-live time or that abut in time (a crash + retry, or a manual
stop+restart, becomes one stream with several takes). A take row offers **Open
file / Open folder / Copy file path / Remove from list** (the file is kept).

**Lost time & capture-from-start.** Normally Lost time is `Started On − Went Live`
— the gap before we began. But with **Capture from start** enabled (yt-dlp
`--live-from-start` / streamlink `--hls-live-restart`) the early footage isn't
actually lost; it's pulled from the platform's DVR. So for those recordings the
app watches the capture and **drops Lost time to 0 once it catches up to the live
edge** (confirmed again at the end by checking the captured length covers the
whole broadcast). If a from-start capture *doesn't* reach the live edge — it's
stopped, crashes, or the stream ends first — the not-yet-downloaded part is the
recent *tail*, not the beginning, so we don't claim a "lost" figure: the column
just shows the provisional `Started − Went Live` estimate until catch-up is
confirmed.

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
| **Twitch EventSub + Helix** | Twitch | Client ID + Secret | ~seconds, with a poll backstop | Does both: EventSub push **and** Helix polling. Whichever sees live first starts the recording, so a missed event (network drop, app started after go-live) is still caught. A longer poll interval is fine — it's just a safety net. |
| **YouTube WebSub (VPS push)** | YouTube | [yt-websub](../yt-websub) relay (URL + token) | ~seconds, with a poll backstop | Push via an external relay on a public VPS: it subscribes to YouTube's WebSub/PubSubHubbub hub and streamarchiver polls it for events. Each notification triggers an **on-demand liveness check** (records only if actually live), with scrape polling as a safety net. A longer poll interval is fine. |
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
fully stateless poll, or **Twitch EventSub + Helix** for the most robust option —
instant push with a polling backstop so you still start the recording if an event
is ever missed. (Connecting a Twitch account also satisfies Helix — its user token
expires, so the app auto-refreshes it and falls back to the app token; if you'd
rather not reconnect, set a Client Secret and the app token is used.)

> To verify EventSub: set Twitch creds, add a Twitch channel with method **Twitch
> EventSub**, then `streamarchiver --run-for 120` with `RUST_LOG=info` — it logs
> `eventsub: connected (conduit …); N channel(s) subscribed` and
> `stream.online -> monitor N` when a channel goes live.

**YouTube WebSub (push via VPS).** YouTube can *push* go-live notifications over
WebSub/PubSubHubbub, but the hub needs a public callback URL — which a home machine
doesn't have. The companion [yt-websub](../yt-websub) server runs on a small public
VPS: it subscribes to the hub for your channels, durably logs each notification, and
exposes them over a token-authenticated HTTPS API. streamarchiver (at home) **polls**
that API. Because a WebSub notification fires for uploads and metadata edits too —
not just go-lives — each event is treated as a *"check this channel now"* trigger:
streamarchiver runs its normal liveness check and records **only if the channel is
actually live** (so it's safe and idempotent), while the scrape poll stays on as a
backstop. To use it: deploy `yt-websub` (see its README), then in **Settings →
YouTube WebSub** set the **VPS base URL** + **bearer token**, and set the relevant
YouTube monitors' **Detection** to **YouTube WebSub (VPS push)**. streamarchiver
auto-resolves each channel to its `UC…` id, pushes the set to the VPS, and the VPS
manages the hub subscriptions.

> Tool tip: use **streamlink for Twitch** (reaches 1440p/2K HEVC) and **yt-dlp for
> YouTube** (`--live-from-start`; streamlink hits YouTube segment 403s). The app
> defaults accordingly.

### Output

Recordings capture to a progressively-flushed `.ts` (so a crash/forced-stop leaves
usable data) and are remuxed losslessly to **`.mkv`** on clean stop. MKV is the
default; pick TS per channel if you prefer. **MP4 is never produced** (poor for
interrupted writes).

### Audio & subtitle tracks

To archive as much of a stream as possible, each instance has **Audio tracks** and
**Subtitle tracks** fields (on the Streams add/edit form):

- **Audio tracks** — which audio tracks to capture, via streamlink's
  `--hls-audio-select`. Empty = the tool's default (one track); **`all`** (or
  `*`) = every audio track; or a comma-separated list of language codes/names
  (e.g. `en,de`). Honored by **streamlink**; the **ffmpeg** tool keeps all
  video+audio tracks via its capture mapping (it can't select a *subset*), and
  **yt-dlp** ignores it (it captures its default audio).
- **Subtitle tracks** — which subtitles to capture, via yt-dlp's `--sub-langs`,
  written as **sidecar files** next to the recording (e.g. `clip.en.vtt`) — a
  lossless, replayable archive, **not** embedded into the container. Empty =
  none; **`all`** (or `*`) = every subtitle; or a comma-separated list of
  language codes. Honored by **yt-dlp** only — **streamlink can't mux
  subtitles**. Best-effort for live streams (live subtitle availability varies by
  platform).

The **MKV remux** on clean stop preserves *all* captured video/audio/subtitle
tracks (not just one per type), and subtitle sidecars are moved along if the file
is later renamed (see *Filename media info*), so the tracks you select are the
tracks you keep.

**New** instances default both to **`all`** (maximum archival). **Existing**
instances keep their previous behavior (empty) until you edit them. Power-user
**Extra args** are appended after these, so they can still override.

### Title & category change log

While a stream records, StreamArchiver polls its metadata and logs every **title**
and **game/category** change for that take — so the archive captures *what* the
broadcast was, not just the footage. (The normal scheduler pauses polling during a
recording, so this runs as a dedicated per-recording poller.)

- **Game** and **Title** columns show the *current* (latest-logged) value of the
  most recent recording, updating live as the stream changes. Both are narrow and
  truncated — **hover** to read the full value.
- A **Changes** column counts only *actual* changes for the latest take — the
  value each field *started* with is the initial state, not a change, so it isn't
  counted or listed (it still shows as the `old` side of the first real change).
  **Hover** a stream/take row's count to see the list inline, or **double-click**
  it to open a scrollable, copyable log window; each entry shows the offset from
  the take's start, the kind, and `old → new`.
- **Sources, per platform:**
  - **Twitch** — Helix (`Get Streams`); needs Twitch credentials (Settings), the
    same app/user token as live detection. Title + the game/category.
  - **Kick** — the public v2 channel JSON (no credentials). Title + category.
  - **YouTube** — scraped from the `/live` page (no credentials). Title, plus the
    broad *content category* (e.g. “Gaming”) — YouTube has no public per-stream
    game field, so the category is the closest stable signal.
  - Generic URLs have no metadata source, so they log nothing.
- Polling is coarse (about once a minute) since changes are infrequent, so the cost
  is low — one request per active recording. (Twitch and Kick hit small JSON
  endpoints; the YouTube path fetches the full `/live` watch page each poll.)

The categories played can also be folded into the filename — see `{games}` below.

### Chat logs

Tick **Log chat** on an instance to archive chat alongside the recording (new
instances default it on):

- **Twitch** — a built-in **anonymous** chat logger (read-only, no account
  needed) connects over Twitch's IRC-over-WebSocket gateway and writes a
  **`<name>.chat.jsonl`** sidecar — one JSON object per message with timestamp,
  login, display name, text, color, and badges. Works with any tool (it's a
  separate connection, independent of streamlink/yt-dlp).
- **YouTube** (with the **yt-dlp** tool) — yt-dlp's `live_chat` writes a
  **`<name>.live_chat.json`** sidecar (folded into `--sub-langs` with any
  subtitles you selected).
- Other platform/tool combinations don't capture chat. Kick chat isn't supported
  yet.

Chat sidecars sit next to the video and **follow it** if the file is renamed
(see *Filename media info*), so they stay matched to their recording.

### Filename templates

The **filename template** sets the output file's *name*. The separate **Output
folder** field sets the directory, and the extension (`.mkv`/`.ts`) is appended
automatically — don't include either. The template is available on the Streams
add/edit form, the Videos download form, and the per-platform defaults. Leaving it
blank uses `{name}_{date}_{time}`.

These are the **only** variables (it's the app's own scheme — not streamlink's or
yt-dlp's output templates):

| Variable | Expands to |
|---|---|
| `{name}` | **Streams:** the channel (container) name. **Videos:** the **Name** field if set, else the auto-detected title, else `video`. |
| `{title}` | The stream/video title. **Videos only**, and only when **Auto-detect** is on (live recordings don't resolve a title, so it's empty there). |
| `{channel}` | The uploader/channel name. **Videos only**, when **Auto-detect** is on; empty otherwise. |
| `{video_id}` | The platform **stream/video id**. **Streams:** set when detection knows it (Twitch Helix/EventSub, YouTube Data API, Kick API); empty for id-less methods (scrape / generic probe). **Videos:** set when **Auto-detect** is on. |
| `{quality}` | The **configured quality selector** (e.g. `1080p60`, `best`, `bv*+ba`) — what you asked for, not necessarily the actual resolution (see `{resolution}`). |
| `{resolution}` | **Actual** capture resolution `WxH` (e.g. `1920x1080`). Requires media probing — see *Filename media info* below; empty when off/unavailable. |
| `{width}` / `{height}` | Actual width / height in pixels (e.g. `1920` / `1080`). Same probing requirement. |
| `{fps}` | Actual frame rate, rounded to a whole number (e.g. `60`, `30`). Same probing requirement. |
| `{vcodec}` | Actual video codec (e.g. `h264`, `hevc`, `av1`). Same probing requirement. |
| `{take}` | **Streams:** this monitor's attempt number (1, 2, 3, …) — a built-in way to keep names unique when you omit `{date}`/`{time}`. Empty for Videos. |
| `{games}` | **Streams:** the distinct game/category names played during the recording (Twitch, Kick & YouTube — see *Title & category change log*), in order of first appearance, joined with `, ` and length-capped. Only known once the stream ends, so it's filled by a **post-capture rename** (see below). Empty for generic URLs / Videos / when no category was logged. |
| `{date}` | Capture-start date, **UTC**, `YYYYMMDD` (e.g. `20260620`). |
| `{time}` | Capture-start time, **UTC**, `HHMMSS` (e.g. `183001`). |
| `{timestamp}` | Capture start as a **Unix timestamp** (whole seconds). |

Notes:

- `{date}`/`{time}` are **UTC** (not local time) and use the moment the
  capture/download *started*.
- Characters illegal in filenames (`< > : " / \ | ? *`) and control characters are
  replaced with `_` and the result is trimmed — so `{channel}/{name}` does **not**
  create subfolders (use the Output folder for the directory).
- Unknown `{…}` tokens are left as literal text; only the variables above are
  substituted.
- If a template expands to nothing usable, it falls back to `{name}_{date}_{time}`.
- **Collisions are handled automatically:** if the target file already exists, the
  app appends ` (2)`, ` (3)`, … (file-manager style) rather than overwriting — so
  even a template with no unique part (e.g. just `{name}`) never clobbers an
  earlier recording. Use `{take}` (or `{date}`/`{time}`/`{video_id}`) if you'd
  rather the difference be part of the name itself.

#### Filename media info ({resolution}/{fps}/…)

Actual resolution/fps/codec aren't known when the filename is first chosen (it's
picked before recording starts), so **Settings → Defaults → Filename media info**
selects how they're obtained — only relevant when your template uses one of those
variables:

- **Off** (default) — don't probe; those variables stay empty.
- **Pre-probe (before recording)** — probe the stream first so the name is final
  from the start. Adds a little latency and is best-effort: the probed format can
  differ from what actually gets recorded (or shift mid-stream). Use a
  post-rename mode for guaranteed-accurate values.
- **Post-capture rename** — record first, then probe the finished file and rename
  it. Most accurate; the final name only appears once the capture completes.
- **Pre-probe + rename** — pre-probe for an initial name, then correct it after
  capture if the actual media differs.

Probing uses the capture tool to resolve the stream and `ffprobe` to read it
(post-rename `ffprobe`s the finished file). Applies to both Streams and Videos.

`{games}` works the same way but is independent of this setting: because the
categories played are only known once the stream ends, a template using `{games}`
always gets a post-capture rename (and any subtitle/chat sidecars are moved along
with the file).

Examples: `{name}_{date}_{time}` → `Layna_20260620_183001.mkv`; for a Videos
download with **Auto-detect** on, `{channel} - {title} [{video_id}]` →
`SomeChannel - Cool Stream [dQw4w9WgXcQ].mkv`.

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
  Twitch sub/Turbo and YouTube members). No manual export needed — yt-dlp reads
  the cookies straight from the browser's profile at download time.
- **Cookies file** → yt-dlp `--cookies <cookies.txt>`.
- **Auth token** → streamlink `--twitch-api-header=Authorization=OAuth <token>`
  for Twitch.

> **Browser profiles / sessions.** The browser value accepts yt-dlp's
> `browser:profile` form, so you can point at a *specific* logged-in profile
> instead of the browser's default (most-recently-used) one — exactly what you
> want for a dedicated "YouTube" Firefox profile. Use the **Profile / session**
> field in Settings → *Download authentication*, or type it inline in any
> per-platform / per-channel / per-video **Browser** field, e.g.
> `firefox:dmrf6eed.YouTube`. The profile is the **folder name** under
> `…\Mozilla\Firefox\Profiles\` (find it at `about:profiles`) or an **absolute
> path** to that folder. Leaving the profile blank uses the browser default —
> which is why a separate login can otherwise be missed. (Chromium browsers use
> a profile *directory* name like `Default` or `Profile 1`.) Tip: the profile DB
> can be locked while that browser is open; if a read fails, close it (or that
> profile) and retry.

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
