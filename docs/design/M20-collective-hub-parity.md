# M20 — Hub-grade collective store: parity on Postgres/Firestore, atomic replace, keyed reads

Size L · gate contract · wave B · contexts: model-leaderboard, store-interface, store-sqlite-eval,
store-postgres, store-firestore

## Problem
The four collective `Store` methods (`crates/store/src/lib.rs` ~1253-1275: `upsert_collective_entry`,
`delete_collective_entries`, `list_collective_entries`, `purge_collective_entries_before`) default
to `Unsupported`; only `sqlite/mod.rs` (~726-738) wires `sqlite/collective.rs`. `grep "fn .*collective"`
in `store-pg` and `store-firestore` returns nothing, so the hub (`POST /v1/collective/ingest`),
`GET /v1/collective/leaderboard` and `DELETE …/contribution` answer 501 on the Neon deployment.
Ingest is delete-then-N-upserts across separate `with()` calls (`crates/api/src/collective/ingest.rs`
~104-118) — a failure mid-loop leaves a contributor partially replaced. The per-contributor
rate-limit reads the **entire** table (`ingest.rs` ~148-155) and the leaderboard decodes the whole
table per request (`leaderboard.rs` ~68). `docs/ROADMAP.md` ~157 claims "full Store parity" — false
here. `schema/postgres/001_init.sql` has no `collective_entries` table.

## Design
1. Store trait additions (append in the collective block; keep the four fine-grained methods):
   ```rust
   fn replace_collective_contribution(&self, contributor_id: &str, entries: &[CollectiveEntry],
       purge_before: Option<DateTime<Utc>>) -> Result<ReplaceAck { deleted, inserted, purged }>;
   fn latest_collective_receipt(&self, contributor_id: &str) -> Result<Option<DateTime<Utc>>>;
   fn list_collective_entries_filtered(&self, f: &CollectiveFilter { received_after: Option<DateTime<Utc>> }) -> Result<Vec<CollectiveEntry>>;
   ```
   Default impls: `replace_*` composes delete + upserts (non-atomic fallback, documented);
   `latest_receipt` and `list_filtered` compose over `list_collective_entries`. Backends override
   with atomic/keyed versions. Declare a `Collective` surface in the M1 manifest.
2. SQLite: `replace_*` in one transaction; `latest_receipt` = `SELECT MAX(received_at) WHERE contributor_id=?`;
   filtered list uses the existing index or add `idx_collective_received`.
3. Postgres: `collective_entries` table in `schema/postgres/001_init.sql` (PK
   `(contributor_id, provider, model, task_type)`, `received_at TEXT` fixed-width RFC3339, other
   columns mirroring SQLite); `crates/store-pg/src/collective.rs` with a sqlx transaction for replace.
4. Firestore: `crates/store-firestore/src/collective.rs`, docs
   `collective_entries/{contributor}_{provider}_{model}_{task}`; replace via batched writes chunked
   at 500 ops (`MAX_ENTRIES` is 5000 — chunk, and write a tombstone/`generation` marker so a
   half-applied replace is detectable; or lower the cap and document). Purge = query + batched delete.
5. Conformance: new `collective` section (upsert/replace/list/purge/receipt; replace leaves no
   partial set after a simulated failure where the backend supports transactions) run by the M1
   driver on all three backends; SQLite runs locally, PG/Firestore under their env gates.
6. API: `ingest.rs` calls `replace_collective_contribution` once; `enforce_min_interval` uses
   `latest_collective_receipt`; `leaderboard.rs` pushes only the pre-floor-safe predicate
   (`received_after` retention) into the store and keeps merge → source-floor → provider/task
   filters in memory (pipeline order rule from the golden path).
7. Docs: fix `docs/ROADMAP.md` parity claim; `docs/BENCHMARK_FRAMEWORK.md` §6 hub-on-Postgres note.

## Out of scope
Recommendations (rejected M21). Contribution ledger (M22). Model identity (M8, wave A — already
merged; use `ModelId`/`ProviderId` where the collective sanitizer now expects them).

## Gates
`cargo build/test/clippy` for lighttrack-store, -store-pg, -store-firestore, -api; SQLite
conformance incl. the new section; `tests_collective.rs` green.

## Evaluation
Before: 4/4 collective methods `Unsupported` outside SQLite; ingest = 1 delete + N upserts, N
connection acquisitions; rate-limit reads whole table. After: conformance has ≥5 collective cases
declared on all three backends; ingest = 1 transaction on SQLite/PG; `GET /v1/collective/leaderboard`
→ 200 on `postgres://` (assert via the PG conformance when env is present; otherwise via the
manifest declaring `Collective`).
