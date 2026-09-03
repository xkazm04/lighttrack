# M11 — Human verdict ledger: labels as data, calibration off files, trust as a queryable state

Size XL · gate contract · wave D · contexts: judge-calibration, dataset-management, scoring,
rubric-system, benchmark-management, prompt-registry · depends on M9 (rubric_id/kind, Rubric.version),
M10 (gate plumbing), M7 (job kinds)

## Problem
Human judgement has no home in the data model. `CalibrationItem` is deserialized from a JSONL file on
the runner's disk (`crates/core/src/calibration.rs`, `runner/calibrate.rs::load_items`);
`DatasetItem` has `expected` but no human score or labeler; κ history is a metrics blob stuffed
into `Score.reasoning` under a reserved rubric name (`runner/calibrate_watch.rs`) and the previous κ
is recovered by scanning the newest 500 scores client-side; `Agreement.trusted` reaches only stdout
and exit code 5. Neither `decide_gate` (`api/benchmarks.rs`) nor `gate_promotion` (`api/prompts.rs`)
can ask whether the judge that produced a run is trusted for that rubric — the "uncalibrated gate"
failure mode, and D15's own caveat that its labels are n=12 and "ours".

## Design
1. `crates/core/src/label.rs`: `Label { id, project_id, subject: LabelSubject { Event(id) | DatasetItem(id) | Score(id) }, rubric_id: Option<String>, value: f64, pass: Option<bool>, dimensions: Vec<ScoreDim>, labeler: String, note: Option<String>, created_at }`.
   `crates/core/src/calibration.rs`: `CalibrationRecord { id, project_id, judge: String, rubric_id: Option<String>, dataset_id: Option<String>, dataset_version: Option<u32>, kappa, pearson, mae, rmse, n, trusted, kappa_bar, created_at }`;
   `JudgeTrust { Trusted | Untrusted | Unknown }` + the deciding record.
2. Store (all three backends; conformance sections; new `Labels` and `Calibrations` surfaces):
   `insert_label`, `list_labels(project, subject filter, cursor)`, `labels_for_dataset(dataset_id)`,
   `insert_calibration`, `latest_calibration(project, rubric_id: Option<&str>, judge)`,
   `list_calibrations(project, cursor)`. Tables `labels`, `calibrations` as self-contained blocks at
   the END of both DDLs. A backend that cannot store labels returns `Unsupported`, never `[]`.
3. API: `POST /v1/labels` (project key with `manage`, or admin), `GET /v1/labels?project=&subject=&rubric_id=&cursor=`
   (READ); `POST /v1/calibrations` (runner, admin), `GET /v1/judges/trust?project=&rubric_id=&judge=`
   → `{ trust: trusted|untrusted|unknown, calibration: {...} | null }`; `GET /v1/scores?needs_review=1`
   → low agreement, near threshold, `injection_suspected`, `floor_hit`, or judge/human disagreement
   (computed from `detail` and any label on the same subject); `POST /v1/datasets/:id/items:from-label`
   ("promote to golden set": copy a labeled event into an unfrozen dataset with the label attached).
   All routes in `ROUTE_SCOPES`.
4. Runner: `lt-runner calibrate --dataset <id>` builds `CalibrationItem`s from items + labels
   (`--file` kept as import; `lt-runner labels import <file>` writes them through `POST /v1/labels`);
   `calibrate_watch::post_calibration` posts the `CalibrationRecord` AND the reserved-rubric Score
   (the score is derived from the record now, not the other way round); `previous_kappa` reads
   `latest_calibration` instead of scanning scores. `JobKind::Calibrate` payload (M7) carries
   `dataset_id`.
5. Gates: `decide_gate` and `gate_promotion` read `latest_calibration` for
   `(bench.rubric_id, bench.judge_model)` and annotate the response with `judge_trust`; a
   per-project policy `require_trusted_judge: bool` (on `Project`, additive column) blocks with
   409 when trust is `untrusted` or `unknown`. Rubric versions (M9) do not inherit trust: a new
   version starts `unknown`; expose `active = trust != unknown` on `GET /v1/rubrics/:id`.
6. MCP: `list_labels`, `get_judge_trust` (read); `record_label` (write-gated). CLI: `lt labels`,
   `lt judges trust`. Render: labels + trust tables.
7. Docs: `CALIBRATION.md` persistence section; `BENCHMARK_FRAMEWORK.md` §3; register any new doc in
   `docs/catchup-marker.json`.

## Out of scope
Dataset fork/versioning (M24). Multi-judge panels (rejected M25).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-mcp, -cli, -render; SQLite conformance incl. the two new sections.

## Evaluation
Before: 0 storage for human labels; trust exists only as stdout/exit code; previous κ from a
500-score scan; gates never consult calibration. After: `GET /v1/judges/trust` answers per
(rubric, judge); labels count > 0 via API; every gate response carries `judge_trust`;
`require_trusted_judge` blocks (test).
