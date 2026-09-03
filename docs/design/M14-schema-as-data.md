# M14 — Schema-as-data: one declarative table model rendering the DDLs, column lists and migrations

Size XL · gate contract · wave F (last: it must see every table and column waves A–E added) ·
contexts: store-interface, store-sqlite-*, store-postgres, store-firestore, core-types

## Problem
The logical schema is hand-copied into three DDL files whose headers each claim to mirror another,
two hand-mirrored migration lists (`ADDED_COLUMNS`/`ADDED_COLUMNS_LATE` in
`crates/store/src/sqlite/schema.rs` vs the `ALTER TABLE … ADD COLUMN IF NOT EXISTS` lines in
`schema/postgres/001_init.sql`), and per-table `COLS` strings + row mappers in each backend
(`sqlite/events.rs`, `store-pg/src/events/cols.rs`, `store-firestore/src/events.rs`). It has already
drifted: BigQuery has a fraction of the tables, lacks many columns, uses native `TIMESTAMP` against
the fixed-width-string invariant (`codec.rs`), and is untested. Waves A–E appended ~10 tables and
~30 columns as "self-contained blocks at the END of the file" — the right merge tactic and exactly the
accretion this item retires. Adding one column today is ~9 coordinated edits across 3 crates.

## Design
1. `crates/store/src/schema/{mod,model,tables,render_sqlite,render_pg,render_bq,migrations}.rs`
   (each ≤300 LOC): `Table { name, columns: &[Column], indexes: &[Index], since: SchemaVersion }`,
   `Column { name, kind: Text|Int|Real|Bool|Json|Ts, nullable, default: Option<&str>, added_in: Option<SchemaVersion> }`,
   `Index { name, columns, predicate: Option<&str> }`. `tables.rs` declares **every** table as const
   data — enumerate them from the current `schema/sqlite/001_init.sql` (the reference) and cross-check
   against PG; every column added after the original ship gets `added_in` sourced from
   `ADDED_COLUMNS`/`ADDED_COLUMNS_LATE` and the PG ALTER lines.
2. Renderers: SQLite DDL + ordered post-batch ALTER list; Postgres DDL + `IF NOT EXISTS` ALTERs (the
   column-before-index ordering guaranteed by construction: render CREATE TABLEs, then ALTERs, then
   indexes); BigQuery DDL with an explicit type map (`Ts → STRING` fixed-width, `Json → JSON`) —
   decide and record the BigQuery choice in `DECISIONS.md`.
3. **The checked-in DDL files become generated**: `cargo test -p lighttrack-store --test schema_doc`
   fails when `schema/{sqlite,postgres,bigquery}/001_init.sql` differ from the render (regenerate
   with `UPDATE_SCHEMA_SQL=1`), same pattern as `parity_doc.rs`. Keep the human comments by carrying
   a `doc: &str` on `Table`/`Column` that the renderers emit as SQL comments. Idempotency of the
   Postgres render against an already-applied database must be verified on a real PG (the env-gated
   conformance run) and, locally, by applying the rendered SQLite DDL twice.
4. Derive `COLS`, insert placeholders (`?1..?n` / `$1..$n`) and the Firestore field-name list from
   `Table::events()` etc.; replace the hand-written constants incrementally (events, scores, jobs
   first). A per-table conformance round-trip iterates columns from the model so a mapper that drops
   a column fails.
5. `ADDED_COLUMNS*` → `schema::migrations_for(Dialect::Sqlite)`; the PG tail ALTERs →
   `migrations_for(Dialect::Postgres)` rendered ahead of indexes. The `received_at` backfill stays a
   named step.
6. `pub fn schema_fingerprint() -> String` (hash of the model) exposed through the M1 manifest and
   `/v1/capabilities`.
7. Docs: `docs/DATA_MODEL.md` generated table list (or a stale-check); register in the catch-up marker.

## Out of scope
Any new table or column (waves A–E are complete). Export (rejected M13).

## Gates
`cargo build/test/clippy` for lighttrack-store, -store-pg, -store-firestore, -core, -api; SQLite
conformance; the new `schema_doc` test; `cargo test -p lighttrack-core --tests` (marker guards).

## Evaluation
Before: tables — sqlite N, postgres N−k, bigquery ≪ N (count them in the PR); 2 independent
migration lists; 3 hand-written events mappers. After: 1 model; rendered-DDL equality tests for 3
dialects; 0 table/column drift; a column addition = 1 edit + intent.
