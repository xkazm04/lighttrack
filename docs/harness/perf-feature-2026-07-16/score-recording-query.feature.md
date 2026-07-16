# Feature Scout — Score Recording & Query

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Human override / relabel of a judge verdict
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/store/src/lib.rs:443-445` (trait), `crates/api/src/scores.rs:16-29`, `crates/core/src/score.rs:37-62`
- **Scenario**: An operator monitoring prod sees the judge marked a good response `pass=false` (or a hallucinated low `value`). That verdict already tripped the rolling `score_drop` regression detector (`st.alerts.record_score`, scores.rs:27). They want to correct it — relabel the value/pass and mark it human-reviewed — so alerts and any downstream calibration/mean reflect ground truth, not judge noise.
- **Root cause**: Scores are strictly append-only. The `Store` trait exposes only `insert_score` / `list_scores` / `list_trace_scores` — there is no `update_score`/`override_score`, and `POST /v1/scores` only inserts. `Score` (score.rs:37-62) has no `human_value` / `overridden_by` / `reviewed_at` field, so a human correction can neither be recorded nor distinguished from a judge verdict. The judge↔human calibration machinery exists (`core/src/calibration.rs`) but there is no path to capture the human label that would feed it from prod monitoring.
- **Impact**: The context's stated need ("human override/relabel of a bad judge verdict") is unmet. Bad judge verdicts permanently pollute means, sparklines, and alert baselines; operators cannot build trust or a labeled correction set.
- **Fix sketch**: Add `human_value: Option<f64>`, `human_pass: Option<bool>`, `overridden_by: Option<String>`, `reviewed_at: Option<DateTime>` to `Score`; add `update_score(id, patch)` to the trait (UPDATE by id in each backend); expose `PATCH /v1/scores/:id`. Have render/mean and `alerts.record_score` prefer `human_value` when present. Schema adds nullable columns (backward-compatible).
- **Trade-offs**: New columns/migration in three backends; must decide whether an override re-evaluates the alert window (recommended: recompute).

## 2. Query scores by rubric and time window (per-dimension drift)
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/api/src/scores.rs:31-48`, `crates/store/src/sqlite/scores.rs:56-75`, `crates/store-pg/src/scores.rs:37-57`
- **Scenario**: "Is my `faithfulness` rubric drifting this week?" An operator (or the responder's regression investigator) wants the scores for one rubric over a date range. The context is explicitly per-dimension trends + drift.
- **Root cause**: `ScoresParams` supports only `project` + `limit`; the store `list` filters solely by `project_id ORDER BY created_at DESC LIMIT`. No `rubric`, `since`/`until`, or `pass` filter exists. Live evidence of the gap: `responder/src/enrich.rs:53` must fetch `?project=&limit=` then filter by rubric **client-side** (lines 64-69) — so when a project has many rubrics, the regressed rubric's rows can fall outside the `limit=30` window and the investigator sees "(no recent scores found)" despite scores existing. The server cannot answer "scores for rubric X" at all.
- **Impact**: Per-dimension monitoring — the core promise of this context — degrades to fetching a blob and eyeballing. Drift on a specific dimension is invisible/unreliable, and the responder enrichment is fragile.
- **Fix sketch**: Add `rubric`, `since`, `until` (and optionally `pass`) to `ScoresParams`; thread into `list_scores(project, rubric, since, until, limit)`, composing WHERE clauses in each backend (sqlite/pg add `AND rubric = ? AND created_at >= ?`; firestore adds EQUAL/GTE filters). Repoint `enrich.rs` at the server-side `rubric` filter.
- **Trade-offs**: Trait signature change ripples to all three backends + conformance test; a `(project_id, rubric, created_at)` index keeps it cheap.

## 3. Score table blends rubrics in mean/trend and drops the event link
- **Severity**: Medium
- **Category**: half-implemented
- **File**: `crates/render/src/scores.rs:12-49`
- **Scenario**: An operator runs `list_scores` on a project judged by several rubrics (e.g. `helpfulness`, `safety`). The header shows one `mean 0.71 · trend` sparkline computed across *all* rows regardless of rubric, and the table has no pointer back to the scored event.
- **Root cause**: `list()` pushes every row's `value` into one `vals`/`trend` vector (lines 20-42) and prints a single mean + sparkline — mixing heterogeneous rubrics (and even different `max` scales) into a meaningless aggregate. The table columns are When/Rubric/Score/Pass/Judge/Cost; `event_id` is stored and returned in the JSON (`Score.event_id`) but never rendered, so a low score can't be traced to its event/prompt from this view (the trace drill-down `list_trace_scores` exists but goes the other direction). This is the "stored-but-never-read" inverse: `event_id` is a dead field in the primary score view.
- **Impact**: The headline metric misleads whenever more than one rubric is present, and operators can't jump from a bad score to the offending event — undercutting "link a low score back to its event/prompt."
- **Fix sketch**: Group rows by `rubric` and emit a per-rubric mean + sparkline (or a summary line per rubric) instead of one global mean; add a short `event_id` column (or make the row linkable) so a low score points at its event. Guard mean when `max` scales differ (normalize `value/max`).
- **Trade-offs**: Slightly larger output; grouping only matters for multi-rubric projects (single-rubric output is unchanged).
