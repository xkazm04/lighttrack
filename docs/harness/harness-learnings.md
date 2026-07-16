# LightTrack — harness learnings

## Structural facts
- **2026-07-16** — Rust workspace, 14 crates under `crates/` + a Rust client SDK under `clients/rust/`.
  Three store backends behind one `Store` trait (`crates/store/src/lib.rs`): SQLite (dev/self-host, single
  global `Mutex<Connection>`), Postgres, Firestore (billed per doc read, reached over REST). Renders to
  **markdown** (`crates/render/`), no web UI. Gate = `cargo check --workspace --all-targets` + `cargo test
  --workspace` (222+ tests, 0 failed at baseline).
- **2026-07-16** — Two crates are separate binaries with their own reactive loops: `crates/responder`
  (`lt-responder`, an axum service that runs Claude Code read-only against a mapped repo on an alert webhook)
  and `crates/agent` (`lt-agent`, the device side of a cloud→device task relay). Both were **invisible to the
  Vibeman context map** before the 2026-07-16 rebuild (map was 60% coverage, 18 ctx / 120 files → rebuilt to
  35 ctx / 200 files; all 18 scan criticals sat in previously-unmapped code).
- **2026-07-16** — The `Store` trait has ~20 **default-bearing methods** that Postgres/Firestore silently
  inherit; several defaults return plausible-but-wrong data (unfiltered/empty). The conformance suite
  (`crates/store/src/conformance.rs`) **never calls those methods**, so a wrong-answer default passes CI green.
  This is the root cause of the recurring backend-parity findings — extend the conformance suite before/with
  any per-backend query work.

## Conventions enforced
- **2026-07-16** — `parallel_map` (`crates/runner/src/util.rs`, `crates/engine/src/pool.rs`) is **eager**:
  it runs and pays for every item before the caller folds results. Do not `?`-propagate out of the fold — one
  error discards every already-paid result. Fold errors into a counter and keep successes.
- **2026-07-16** — The judge/generation providers already pool one `reqwest::Client` via `OnceLock`
  (`engine/src/providers.rs`); the price book is cached in `Arc<RwLock<PriceBook>>`. Don't "fix" these as
  per-call rebuilds — verified not present.

## Anti-patterns to avoid
- **2026-07-16** — `write_all(all_of_stdin)` then `wait_with_output()` on a piped child **deadlocks** once
  either pipe fills its ~64KB OS buffer. Always write on a thread + drain the other pipe(s) concurrently +
  bound the wait with `try_wait` against a deadline. (Fixed in `agent/src/connect.rs`.)
- **2026-07-16** — Cost control placed on the *wrong* stage: the responder's `Breaker` gated the ACT (write)
  stage, but the INVESTIGATE (read) stage runs first, always, and is the billable one. When adding a governor,
  gate the stage that actually spends, not the one that feels dangerous.
- **2026-07-16** — Blind top-N dedup windows (`?limit=1000`) silently break correctness once the table
  exceeds N (re-judging, missed sweeps). Recurs in score-recording, relay dead-letter, calibration drift.
  Scope the read to the specific ids you need, or do the anti-join server-side.

## Open follow-ups (from Wave 1, 2026-07-16)
- **score-recording #1 (Critical) — DEFERRED.** Client-side anti-join with a hard 1000-score cap re-judges
  events past 1000 scores. Correct fix spans Store trait + 3 backends + `/v1/scores` API + `idx_scores_event`
  index — backend-parity family, do with conformance coverage. Full plan in `followups-2026-07-16.md`.
- Waves 2–8 unstarted. See `perf-feature-2026-07-16/INDEX.md` "Suggested fix-wave split" and the 10
  cross-cutting themes (T1–T10) — the themes, not the 179 individual findings, are where the fix leverage is.
