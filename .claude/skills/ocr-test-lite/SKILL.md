---
name: ocr-test-lite
description: This skill should be used when the user wants a quick, cheap spot-check of StreamArchiver's Schedule OCR accuracy — a small sample rather than the full audit. Trigger phrases include "quick OCR test", "spot-check OCR on a few channels", "run /ocr-test-lite", or verifying a recent `schedule_ocr.rs` prompt change before committing to it.
disable-model-invocation: true
argument-hint: "[count] [model]"
---

# /ocr-test-lite — quick OCR accuracy spot-check

The lightweight sibling of `/ocr-test`: same procedure, a small default
sample so it's cheap enough to run after every prompt tweak to
`src/schedule_ocr.rs`, not just occasionally.

Read `.claude/skills/ocr-test/SKILL.md` and follow its full procedure
(Steps 1–6) exactly, with these overrides:

- **Sample size defaults to 5**, not 50. Parse `$ARGUMENTS` the same way
  `/ocr-test` does (first integer token overrides the count, a
  `haiku`/`sonnet`/`opus` token overrides the model).
- **No Artifact needed** — 5 rows fits fine as plain chat text. Report
  inline instead of rendering an HTML table.
- Everything else (image discovery/priority, reading `build_prompt` live
  rather than hardcoding it, bounded-batch execution, the day/date
  consistency + monotonic-sequence + confidence + title-sanity checks, and
  the "suggest a refinement, then re-test to confirm it actually helped"
  closing step) is identical to `/ocr-test` — do not duplicate or
  re-derive that logic here, just apply it at the smaller scale.

This is the right command to reach for immediately after editing
`build_prompt` — a 5-image run against real cached images is fast enough
to use as a matter of course, the same way this session iterated live
against the CottontailVA banner four times in a row before landing on a
wording that actually worked (see `feedback`/project memory
`schedule-ocr-properties-rescan.md`: prompt-engineering for this feature
is not reliably solvable by reasoning alone — always verify against a real
image before calling a wording change "the fix").
