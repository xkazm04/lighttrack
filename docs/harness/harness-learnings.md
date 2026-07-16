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

## Testing constraints (important)
- **2026-07-16** — PG and Firestore conformance tests (`store-pg`/`store-firestore` `tests/conformance.rs`)
  run ONLY against a live database via an env var; without it they **skip and report ok**. On the Windows dev
  box there is no live PG/Firestore, so those backends are **compile-verified only** — the default `cargo test
  --workspace` never actually exercises their queries. SQLite conformance (`store/tests/sqlite_conformance.rs`,
  in-memory) runs the full `run()` for real every time. Plan backend-parity work accordingly: write it, but
  flag PG/Firestore as needing live-DB verification.

## Open follow-ups (updated 2026-07-16, Wave 4)
- **DONE:** score-recording #1 (Critical) + #2 (High) closed (`a62b792`); store-trait conformance-gap #1
  (High) closed (`bcc23dc`). See `perf-feature-2026-07-16/FIXES-WAVE-4.md`.
- **OPEN — PG/Firestore query-method parity.** The conformance suite now encodes the contract for
  `list_events_filtered` / `cost_summary_windowed` / `usage_since_scoped` / `usecase_costs`, which PG/Firestore
  still inherit wrong. Implement per backend (PG `usecase_costs` also needs a `name` column). Needs live DBs.
  Plan in `followups-2026-07-16.md`. Also: Firestore transport batch write (`commit_update` 1-element array).
- Waves 2, 3, 5–8 unstarted. See `perf-feature-2026-07-16/INDEX.md` and the 10 themes (T1–T10) — the themes,
  not the 179 individual findings, are where the leverage is. Note theme T2 (backend parity) is now half-done:
  conformance covers it; the per-backend impls remain.
