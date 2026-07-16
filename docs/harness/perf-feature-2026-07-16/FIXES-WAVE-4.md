# Perf+Feature Fix Wave 4 — Backend parity + conformance (theme T2) + deferred score critical

> 2 commits. Closed the deferred Wave-1 critical + its paired High + the parity-root-cause High.
> Baseline preserved: `cargo check --workspace --all-targets` clean; workspace tests 424 passed / 0 failed
> before and after (added conformance coverage; SQLite runs it, PG/Firestore skip without a live DB).
> Branch `vibeman/perf-feature-2026-07-16` (off `main`, NOT pushed).

## Mental model

The trait has ~20 **default-bearing methods** that SQLite overrides but Postgres/Firestore silently inherit,
and several defaults return *plausible-but-wrong* data. The conformance suite never called those methods, so
wrong backends passed green — the systemic root cause. This wave (a) fixes the score critical the right way
(a required, conformance-pinned method, not a client-side hack) and (b) makes the conformance suite honest so
the whole parity class becomes visible.

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `a62b792` | score-recording #1 (Critical) + #2 (High) | Critical + High | trait + 3 backends + api + runner + both schemas |
| 2 | `bcc23dc` | store-trait conformance-gap #1 (High) [+ event-ingestion #1 context] | High | `store/src/conformance.rs` |

## What was fixed

1. **Scoped unscored-events anti-join (score critical).** The online scorer found "unscored" events by
   fetching the top-1000 scores and anti-joining client-side; past 1000 scores an aged-out event got
   re-judged (paid), and each interval tick shipped up to 1000 Score rows / 1000 billed Firestore reads.
   Replaced with `Store::scored_event_ids(event_ids)` — **required, no default**, so no backend silently
   inherits a wrong answer, and pinned by conformance. `Store::list_unscored_events` has a correct default
   (page via `list_events`, remove the scored ids — scoped to the page, correct at any scale). Exposed as
   `GET /v1/events?unscored=1`; the runner dropped the scores fetch and the client HashSet. Added
   `idx_scores_event` in both schemas (also turns the trace-scores join into an index probe — score #2).

2. **Conformance now exercises the default-bearing methods.** Added a `parity_gap_methods` section that
   asserts `list_events_filtered` filters, `cost_summary_windowed` respects the window, `usage_since_scoped`
   scopes, and `usecase_costs` groups — behaviors SQLite has and the PG/Firestore defaults get wrong. SQLite
   passes; the PG/Firestore live tests will now fail until they port these queries (the missing drift signal).

## Verification

| Gate | Before | After |
|---|---|---|
| `cargo check --workspace --all-targets` | clean | clean (all 3 backends compile the new methods) |
| workspace tests | 424 passed / 0 failed | 424 passed / 0 failed |
| SQLite conformance (real) | — | runs `scored_event_ids` + `list_unscored_events` + `parity_gap_methods` |
| PG / Firestore conformance | skip (no live DB) | skip (no live DB) — **now encode the correct contract** |

**Honesty note:** the PG/Firestore implementations of `scored_event_ids` (score fix) and the four gap methods
are **compile-verified only** on this Windows box — their conformance tests need a live database. The score
fix's per-backend queries mirror the existing `list_scores` patterns closely; the gap-method implementations
are tracked as the next step in `followups-2026-07-16.md` and are exactly what the new conformance section
will accept.

## Patterns established (catalogue, continued from Wave 1)

5. **Required trait method for a correctness-critical operation** — no default, so every backend must
   implement it and the conformance suite pins it. Use when a silent wrong default would corrupt data or burn
   money (here: re-judging). Contrast with a *correct* default built from required primitives (`list_unscored_events`).
6. **Make the conformance suite fail wrong backends** — cover the methods where backends diverge, assert the
   reference (SQLite) behavior. A conformance suite that can't fail is documentation, not a gate.

## What remains

- PG + Firestore implementations of the four gap methods (`usecase_costs` on PG also needs a `name` column) —
  `followups-2026-07-16.md`. Firestore transport batch write (cloud-store #1). Then Waves 2/3/5–8.
