# Perf+Feature Fix Wave 1 — Admission control on paid calls (theme T1)

> 4 commits, 4 of the 5 wave-1 criticals closed (+ 1 High folded in). 1 critical deferred with a plan.
> Baseline preserved: `cargo check --workspace --all-targets` clean; workspace tests 0 failed before and after (added 5 tests).
> Branch `vibeman/perf-feature-2026-07-16` (off `main`, NOT pushed).

## Mental model

Every finding here is one shape: **a paid subprocess / LLM call spawned or wasted with no governor.** The
responder and agent are self-amplifying billing loops (more errors → more alerts → more paid runs); the
pairwise phase spends super-linearly and throws away paid verdicts on one tail error. The fix in each case is
a bound placed *before* the spend, plus not discarding spend already made.

## Commits

| # | Commit | Finding(s) closed | Sev | Files |
|---|---|---|---|---|
| 1 | `c334495` | responder-investigation #1 + auto-responder-pipeline #1 (same root cause) | 2× Critical | `responder/src/{config,breaker,pipeline,main}.rs` |
| 2 | `8334709` | device-agent #1 | Critical | `agent/src/connect.rs` |
| 3 | `c02366c` | judge-pairwise #1 (Critical) + #2 (High) | Critical+High | `runner/src/{cli,pairwise}.rs` |

## What was fixed

1. **Responder investigate stage — admission control.** The read stage runs first and always and is a full
   billable Claude Code child, but nothing between an accepted webhook and the spawn checked concurrency,
   cooldown, or dedup — the `Breaker` gated only ACT (the auto-fix edit) and only counted *applied* fixes.
   A flapping project spawned one ~$1 session per alert, self-amplifying, on an unauthenticated endpoint
   whose only dedup was a remote alerter cooldown in another process (~$60/hr/project, ×N mapped projects).
   Extended `Breaker` with investigation admission: in-flight dedup (atomic check+reserve), per-project
   cooldown (600s), rolling-hour spawn cap (20/h), global concurrency semaphore (2). RAII guard held across
   investigate+act+deliver; gate placed after error classification so transient errors don't consume it.

2. **Agent connector — concurrent pipe drain + bounded wait.** `write_all`-then-`wait_with_output` deadlocked
   once either the stdin or stderr pipe filled its ~64KB OS buffer (a real-sized envelope + any stderr from
   the script). Delivery runs inline on the serial run loop, so one wedged connector hung the whole agent
   forever; the cloud then re-ran the full paid Claude call on another device at lease expiry. Now: write the
   envelope on a thread, drain stderr concurrently (bounded to 8KB retained), poll `try_wait` against a 60s
   deadline and kill a runaway so the task settles `failed`. Writer BrokenPipe treated as benign on clean exit.

3. **Pairwise judging — pre-flight cost gate + tail resilience.** (a) Round-robin is O(targets²·cases) games ×
   2 judge calls each with no cap/estimate; 8 targets × 100 cases = 5,600 calls (~$56), the total printed only
   after everything is paid. Added a pre-flight line (games, call count, rough $) and a `--max-games` guard
   (default 500) that aborts before any spend, printing the exact value to pass to proceed. (b) The eager fold
   did `outcome?`, so one transient tail failure discarded every already-paid verdict *and* the generation
   spend and skipped `post_run` (so the ledger under-reported). Now failed games are counted (`judge_errors`),
   dropped, and the rest tallied; `post_run` always runs.

## Verification

| Gate | Before | After |
|---|---|---|
| `cargo check --workspace --all-targets` | clean | clean |
| workspace tests | 0 failed | 0 failed (added: agent +1, responder +2, runner +1 = 5 tests) |

## Patterns established (catalogue)

1. **Admission-control guard before a paid spawn** — dedup (atomic HashSet check+insert) + per-key cooldown +
   rolling-hour cap + concurrency semaphore, returned as an RAII guard so release is automatic. Reusable for
   any "unbounded paid child" site (T1).
2. **Concurrent-drain subprocess I/O** — never `write_all` all of stdin then `wait`; write on a thread, drain
   the other pipe(s) concurrently (bounded), poll `try_wait` against a deadline, kill on expiry.
3. **Don't discard eager-parallel paid results on one error** — when `parallel_map` has already paid for every
   item, fold `Err` into a counter and keep the successes; always reach the cost-recording step.
4. **Pre-flight cost gate on combinatorial spend** — compute the call count from inputs *before* the first
   call, surface it, and refuse past a bound with the exact override value.

## Deferred (see followups-2026-07-16.md)

- **score-recording #1 (Critical)** — client-side anti-join with a hard 1000-score cap silently re-judges
  events (and re-reads up to 1000 Firestore docs per interval) once a project passes 1000 scores. The correct
  fix is a server-side unscored-events query (or an `event_ids`-scoped scores read), which spans the `Store`
  trait + all three backends + the `/v1/scores` API + the `idx_scores_event` index — the **backend-parity /
  query-correctness family (Wave 2/4)**, not this wave's governor model. Deferred deliberately to be done with
  conformance coverage rather than as a rushed 6-file cross-backend change. Plan in the followups note.

## What remains

Wave 2 (money-truth Firestore/forecast scans), Wave 3 (privacy & consent integrity), Wave 4 (backend parity +
conformance — where deferred score #1 lands). See INDEX.md "Suggested fix-wave split".
