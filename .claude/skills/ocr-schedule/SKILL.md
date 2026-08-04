---
name: ocr-schedule
description: This skill should be used when the user wants a streamer schedule graphic decoded into structured events — "OCR this schedule", "what does this schedule image say", "extract the events from this banner", "run /ocr-schedule", given either a file path or a pasted/attached image. General-purpose, standalone use — not tied to any specific channel already tracked by the app.
argument-hint: "[filepath] [--cli [model]]"
---

# /ocr-schedule — general-purpose schedule image OCR

Decode any streamer schedule graphic into the same structured event schema
StreamArchiver's own Schedule OCR feature produces (`src/schedule_ocr.rs`),
for one-off/standalone use — not limited to channels the app already
tracks.

## Resolve the input

- If `$ARGUMENTS` contains a file path, use that image. Verify it exists
  first.
- Otherwise, use the image(s) the user pasted/attached in their message —
  they're already visible in this conversation's context.
- If neither is present, ask the user for a file path or to paste an
  image; don't guess.

## Parse `$ARGUMENTS` for the optional CLI passthrough

If `$ARGUMENTS` includes `--cli`, this run should reproduce the app's
actual production pipeline instead of doing the extraction directly (see
below) — shell out to the real `claude` CLI the same way `schedule_ocr.rs`
does, using whichever model name follows `--cli` (default `haiku`,
matching `DEFAULT_MODEL`). This is useful when the user specifically wants
to know what the shipped app would produce, not just a good-faith read.
Otherwise (the default, no `--cli`), do the extraction directly yourself —
it's faster, free, and — since it runs with this conversation's full model
and context rather than a sandboxed one-shot subprocess call — usually at
least as accurate. Point users who want the literal production pipeline
compared across many images at `/ocr-test-lite` instead.

## Apply the current extraction rules

Read `src/schedule_ocr.rs`'s `fn build_prompt` **fresh, right now** — do
not rely on a remembered copy of the rules, they've changed multiple times
already. Whether extracting directly or shelling out via `--cli`, apply
that exact current ruleset: font-legibility care on stylized text, treating
each visible card's own printed day-of-week label as ground truth (not
grid-position counting, since blank "nothing scheduled" filler graphics
can hide an unknown number of days), a non-decreasing day-sequence sanity
check, timezone-abbreviation handling (use the table in the prompt; if a
primary timezone is given as an argument, prefer it the way a per-channel
config override would), multi-timezone-printed-once collapsing, the
`collab`/`'w'`-prefix convention, skipping OFFLINE/TBD cards, and the
`confidence` self-report per event.

If doing direct extraction (no `--cli`): look at the image as carefully as
the prompt asks the OCR model to, apply the same rules, and construct the
event objects yourself in the identical schema (`title`, `collab`, `date`,
`day`, `time`, `time_label`, `timezone`, `datetime`, `confidence`,
`source_image`). Use the current real year for `date`/`datetime` unless the
image itself prints one.

If using `--cli`: build the equivalent prompt text (same substitution
rules as `/ocr-test`'s Step 3), run
`claude --model <model> --add-dir "<image_dir>" -p "<prompt>" --output-format json`,
and parse its `result` field.

## Present the result

Show a clean table: Title | Collab | Day | Date | Time | Timezone |
Confidence — plus the raw JSON if it'd be useful to the user (e.g. they
plan to hand it to something else). Call out any `confidence: "low"`
events explicitly since those are the ones worth a human double-check.
This is a read-only inspection tool — it doesn't write anything into the
app's database or its schedule sources; if the user wants this schedule
tracked for real, point them at the app's own Schedule sources
configuration (right-click a channel → Properties → Schedule sources) or
the "🔄 Rescan this event" action on an existing tracked event.
