---
name: ocr-test
description: This skill should be used when the user asks to "test OCR accuracy", "spot-check the schedule scanner", "audit the schedule OCR", "run /ocr-test", or otherwise wants a broad accuracy check of StreamArchiver's Schedule OCR feature across many channels at once.
disable-model-invocation: true
argument-hint: "[count] [model]"
---

# /ocr-test — broad OCR accuracy audit

Spot-test StreamArchiver's schedule-image OCR feature (`src/schedule_ocr.rs`)
by running the app's real CLI pipeline against a random sample of cached
channel images, flagging likely misreads, and suggesting concrete prompt
fixes. This is the same failure class fixed in the 2026-08-04 session (see
project memory `schedule-ocr-properties-rescan.md`) — title letter-legibility
and day/date misattribution — so the checks here specifically target those.

Default sample size is **50 channels**. Parse `$ARGUMENTS` (whitespace
separated, both optional, either order): the first token that parses as an
integer overrides the sample count; a token matching `haiku`/`sonnet`/`opus`
overrides the model (default `haiku`, matching `DEFAULT_MODEL` in
`schedule_ocr.rs`).

Before starting, tell the user the sample size, model, and a rough
time/cost estimate (see step 4), since this spawns real paid CLI calls.

## Step 1 — Locate the asset cache

Resolve `%APPDATA%\StreamArchiver\data\asset-cache\channel_assets` (use
`$env:APPDATA` in PowerShell — never hardcode a username). If the directory
doesn't exist or has no channel subfolders, tell the user there's nothing to
test and stop.

## Step 2 — Sample channels and pick one candidate image each

Pick the sample size worth of DISTINCT channel folders at random from that
directory (PowerShell `Get-Random` is fine here — this is a normal agent
turn with full tool access, not a Workflow script, so there's no
`Math.random()`/`Get-Random` restriction).

For each chosen channel, resolve exactly ONE test image using this
priority (mirrors the app's own source priority in
`src/detectors.rs`'s `ocr_twitch_banner`/`ocr_other_image`/
`ocr_youtube_community`):

1. Any file under `<channel>\**\schedule_src\*` (any platform/account) —
   this is content the app's community-post/pinned-tweet/other-image OCR
   sources have already flagged as schedule-relevant. If several, take the
   most recently modified.
2. Otherwise, `<channel>\**\banner.*` (the Twitch offline banner fallback).
   If several accounts, take the most recently modified.
3. If a channel has neither, discard it and sample a replacement channel,
   up to a reasonable retry cap, so the final set has the full requested
   count of valid images (or as close as the cache allows — report the
   actual count tested if fewer).

Most cached images will turn out to be ordinary channel art, not schedule
graphics — that's expected and useful data, not a setup mistake (see
Step 5).

## Step 3 — Build the current prompt (read live, never hardcode)

Read `src/schedule_ocr.rs`'s `fn build_prompt` **fresh, right now** — the
exact rules have changed multiple times already and will keep changing.
Reconstruct the equivalent prompt text it would produce for each image
using the "no primary timezone configured" branch (the default — the
abbreviation-table version, not the named-zone version) and
`year = <current real year>`. Substitute `{image_path}` with each test
image's absolute path (forward slashes). Do not reuse a prompt string from
a previous run of this skill or from memory — always re-derive it from the
current source so this test stays honest about what's actually shipping.

## Step 4 — Run the CLI against every sampled image

For each `(channel, image_path)` pair:

```
claude --model <MODEL> --add-dir "<image_dir>" -p "<prompt>" --output-format json > <tmp>/<n>.json 2><tmp>/<n>.err
```

Use a scratch directory under the session's scratchpad for the numbered
output files. Each call can take 30s–3min and costs roughly $0.02–0.06 on
haiku — run them in **bounded batches** (about 6–8 concurrent Bash tool
calls per message, i.e. multiple tool_use blocks in one turn) rather than
all at once, to avoid overloading the machine; wait for each batch to
finish before starting the next. Give the user the estimate up front:
roughly `(count / 8) * ~2min` wall-clock, `count * ~$0.04` cost.

## Step 5 — Parse and sanity-check each result

For each output file, use `python3` to load the JSON envelope and read its
`result` field (strip markdown fences the same way `parse_events` does —
trim to the outermost `[`...`]`).

- **Parse/CLI failure** → flag as `cli_failure`.
- **Empty array `[]`** → **not a failure**. Label it `no_schedule_detected`
  and track it separately — most sampled images won't be schedule
  graphics at all, and a correct "found nothing" is exactly what should
  happen on those. Only images that returned ≥1 event count toward
  accuracy stats.
- For every event in a non-empty result:
  - **Day/date consistency**: does the weekday of `date` (Python
    `datetime.date.fromisoformat(date).strftime('%A')`) match the `day`
    field, case-insensitively? Mismatch → flag `day_date_mismatch` (this
    is exactly the bug fixed 2026-08-04 — a day-label misread or bad
    anchor arithmetic).
  - **Monotonic sequence**: within one image's event array, are `date`
    values non-decreasing in order? A decrease → flag
    `non_monotonic_dates`.
  - **Confidence**: note how many events came back `"low"` vs `"high"` —
    a `"low"` on a title-only or date-only issue is a sign the escalation
    path (see `events_need_escalation` in `schedule_ocr.rs`) should have
    already kicked in and swapped to the fallback model; if you ran with
    `haiku` and saw `"low"` confidence survive into the final JSON, that's
    worth noting (though this skill runs the primary model only by
    default — it doesn't reproduce the app's own escalation retry, so a
    `"low"` here is just a signal, not necessarily a bug).
  - **Title sanity**: flag titles under 3 characters, titles that are
    mostly non-alphabetic, or exact-duplicate titles at the same
    `datetime` within one image as `possible_misread` for manual
    spot-check — there's no ground truth to verify against automatically,
    so these are leads, not confirmed bugs.

## Step 6 — Report

Produce:

- **Summary counts**: total sampled, `no_schedule_detected`,
  `cli_failure`, clean schedule reads, flagged schedule reads (broken
  down by flag type).
- **Per-flagged-image detail**: channel name, image path, which flag(s)
  fired, and the raw extracted event(s) that triggered it.
- **Suggested refinements**: if any flag type recurs 2+ times, name the
  specific `build_prompt` rule area implicated (font-legibility rule /
  per-card day-label rule / date-anchor rule / timezone table) and propose
  a concrete wording change. Do not just propose a fix — after editing
  `schedule_ocr.rs`, re-run `/ocr-test-lite` (or re-test the specific
  flagged image directly via one `claude` CLI call) to empirically confirm
  the change actually helps before calling it done; a session on
  2026-08-04 found prompt wording changes are NOT reliably predictable
  from reasoning alone — always verify live against the real image.

For the full 50-image report specifically, also render it as an HTML
table via the Artifact tool (per-row: channel, image, status, flags) —
50 rows is unwieldy as plain chat text. Clean up the scratch temp files
when done.
