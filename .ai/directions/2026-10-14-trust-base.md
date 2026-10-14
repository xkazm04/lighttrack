---
subject: software-engineering/trust-base
project: tracklight
raised_by: intake intake-vox-1014
source: librarian/sources/2026-10-14-rtk-ai-vox.md
stage: the workspace test gate (cargo test --workspace on the Linux runner) and the escape census
size: ~16 files / ~60 lines / S
status: proposed
---

## Why the scope implies it

`scope.does` is "ingest LLM telemetry, score with judges, benchmark providers, serve an
operator API": a multi-threaded service whose tests run on the parallel test runner on
a Linux runner. The registry technique `trust-base/census-counts-unmarked-members`
(landed 2026-10-14) counts trust-base members the escape marker does not spell: calls
to functions a later language revision re-marked as unsafe, which an edition-2021
crate calls with no marker. Measured on this tree the same day: **0 marker sites and
49 unmarked environment writes**, in 15 files, all in tests. The standard library
documents an environment write as sound only if no other thread reads or writes the
environment by any route (on Linux; Windows is always sound). The test runner runs
tests on parallel threads, and exactly one integration test file serializes its
writes behind a lock (`crates/engine/tests/provider_boundary.rs:30`); the other 14
files write the environment while tests in other modules may be reading it. A census
by marker reports this tree's trust base as empty.

## What the first context contains

- One process-wide test helper (in an existing shared test-support location) that
  takes a single lock and restores the variable on drop, and every environment write
  in tests moved onto it, so environment-touching tests serialize against each other.
- The census as a line in the gate: the migration lint `deprecated_safe_2024` run per
  crate, reported as its own count beside the marker count (the text census, checked
  against the lint on one crate, is the fast stand-in).
- It must NOT absorb: the edition-2024 migration itself (a separate decision that
  would mark every site and add audit reminders without auditing), production
  configuration loading, or the readers of the environment outside tests.

## The measurable

Unmarked environment writes in tests not taken under the shared lock: 47 today (49
minus the two in the one locked file) to 0. Floor: `cargo test --workspace` passes at the
default thread count, and the conformance jobs stay green.

## What would make this wrong

If the readers that race are only other environment-writing tests (then the shared
lock alone fixes it and the census line adds nothing), or if the runner's C library
already synchronizes environment reads against writes on the CI image, in which case
the race is real only on other targets and the direction shrinks to the census line.
A flake history for `cargo test --workspace` showing zero environment-related failures
over the last two months is weak evidence of the second and should be read first.
