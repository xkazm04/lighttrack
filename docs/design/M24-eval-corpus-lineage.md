# M24 — Eval corpus lineage: versioned datasets, sampling strategies, failure-mined regression sets

Size L · gate contract · wave E · contexts: dataset-management, scoring, benchmark-management,
judge-calibration · depends on M11 (labels), M7 (job kinds), M9 (score kind)

## Problem
`Dataset.version` is always 1 — `create_dataset` writes it (`crates/api/src/datasets.rs`) and no
store ever updates it — so the "versioned dataset" the comparability story rests on is a constant:
the paired-test guard that refuses to pair across `dataset_version` mismatches
(`runner/history.rs`) and the run pin `dataset_pin` (`runner/bench.rs`) compare 1 with 1. Freezing is
terminal. The only sampler is "newest N events with an input" (`runner/dataset.rs`) while
`docs/BENCHMARK_FRAMEWORK.md` §1 promises `recent | random | stratified | errors-only` plus
near-duplicate dedupe. Failing online verdicts (`runner/score.rs` `pass=false`) never become dataset
items.

## Design
1. `crates/core/src/dataset.rs`: `Dataset += parent_id: Option<String>`; `SamplingStrategy { Recent, Random, Stratified, Errors }`;
   `ImportSpec { from: Events | Scores, filter: { pass: Option<bool>, status: Option<Status>, model: Option<String>, since: Option<DateTime> }, strategy, n, dedupe: bool }`.
2. Store (all backends; conformance section; `DatasetLineage` surface):
   `fork_dataset(scope, id) -> Dataset` (same name, `version + 1`, items copied, unfrozen, `parent_id` set),
   `import_dataset_items(scope, dataset_id, spec) -> u32` (SQLite/PG SQL: stratified = per-(model,
   status) quota; dedupe on a normalised-input hash column added additively; Firestore client-side or
   `Unsupported`), `list_dataset_versions(scope, project, name)`. Columns via `ADDED_COLUMNS_LATE` /
   PG tail block. Conformance pins fork increments `version` and links `parent_id`, and refuses
   import into a frozen dataset.
3. API: `POST /v1/datasets/:id/fork`, `POST /v1/datasets/:id/items:import` (re-scrubs through
   `lighttrack_anon::scrub` like `dataset build`; 409 on frozen), `GET /v1/projects/:id/datasets/:name/versions`.
   Routes in `ROUTE_SCOPES`. Benchmark-level `regression_dataset: Option<String>` policy: failing
   online scores under the benchmark's rubric append to the current unfrozen version, which the next
   scheduled run freezes and pins.
4. Runner: `dataset build --strategy --from scores --below <t> --dedupe` calls the server import;
   `JobKind::DatasetSample` payload (M7) carries the `ImportSpec`; `schedule` forks instead of
   creating a fresh dataset per cycle; `score.rs`: after a failing verdict, if the project's benchmark
   declares `regression_dataset`, POST the event id to the import endpoint (best-effort, counted).
   `bench.rs::dataset_pin` unchanged — it starts recording real versions.
5. Labels (M11): `fork` carries labels forward by subject; `import` from `Scores` attaches any label
   on the source event.
6. MCP: `fork_dataset`, `import_dataset_items` (write-gated). CLI `lt datasets fork|import|versions`.
   Docs: `BENCHMARK_FRAMEWORK.md` §1 becomes true; register docs in `catchup-marker.json`.

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-mcp, -cli; SQLite conformance incl. the new section; `history.rs` pairing test with a real v1/v2.

## Evaluation
Before: `version` is 1 for every dataset (no UPDATE path); 1 sampling strategy; 0 paths from scores
to dataset items. After: `fork` yields v2 linked to v1; `history.rs` rejects a real version mismatch
(test); failure-mined items count > 0 in a fixture run.
