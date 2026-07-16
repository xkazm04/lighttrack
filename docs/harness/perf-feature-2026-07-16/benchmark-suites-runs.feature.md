# Feature Scout — Benchmark Suites & Runs

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Compare two arbitrary runs (run-vs-run diff), not just a scalar baseline
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/render/src/compare.rs:11-53`, `crates/render/src/benchmarks.rs:31-72`, `crates/api/src/benchmarks.rs:176-186`, `crates/runner/src/compare.rs:124-319`
- **Scenario**: A team ran the suite last week (run A) and again today after a prompt tweak (run B). They want "which cases got better, which regressed, by how much" — the core Braintrust/LangSmith experiment-diff view. Today they cannot get it.
- **Root cause**: The only baseline is the benchmark's single scalar `baseline_score` (`Benchmark.baseline_score`, read in `significance_verdict` / `decide_gate`). `run_compare` compares *targets within one invocation* against that scalar; it never references another run. `render::benchmarks::runs` renders a flat history table with a mean sparkline (`benchmarks.rs:66-70`) — an aggregate trend, no per-case A-vs-B delta. `render::compare::leaderboard` diffs targets, not runs. There is no `GET /v1/benchmark-runs/{a}/compare/{b}` and no render mode that ingests two runs' per-case `report.cases[]` arrays (which compare mode already stores at `runner/compare.rs:221-225`).
- **Impact**: Run-vs-run diffing is the flagship reason teams buy an eval product ("did my change make it better or worse, and where?"). The per-case data is already persisted; only the diff surface is missing. Highest-leverage gap in this context.
- **Fix sketch**: (1) Add `decide_run_diff(run_a, run_b)` in the runner/api that joins `report.cases[]` by `case` index, emits per-case `score_a/score_b/delta` + overall mean delta + significance (reuse `Summary`/`significance_verdict` on the paired deltas). (2) New render mode `compare_runs` in `render/src/compare.rs` (or a `run_diff` fn) rendering an improved/regressed/unchanged case table with a headline verdict. (3) Wire a CLI subcommand + `GET /v1/benchmark-runs/{id}/diff?base={id}` using the existing `list_benchmark_runs` fetch.
- **Trade-offs**: Simple-mode runs store no per-case array (see #3), so diffs are aggregate-only there until provenance is enriched; compare-mode runs work immediately.

## 2. Promote a run to baseline / update the baseline — baseline is frozen at creation
- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/api/src/benchmarks.rs:96-134` (only create; no update handler anywhere in the file), `crates/api/src/benchmarks.rs:230-257` (`decide_gate` reads `baseline`)
- **Scenario**: An operator sets `baseline_score = 0.80` at benchmark creation, ships a genuine model improvement to 0.88, and wants the CI gate to now guard the *new* bar so future regressions below 0.88 fail. There is no way to move the baseline.
- **Root cause**: `baseline_score` (and the `schedule_interval_secs` recurrence, and the target matrix) are only ever written in `create_benchmark`. The module exposes create / list / get / list_runs / post_run / gate — **no `PATCH`/update handler**, and the store traits (`create/get/list` in all three drivers) have no update either. The gate compares the latest run against whatever baseline was set once at creation and can never track legitimate quality gains; the only recourse is direct DB surgery.
- **Impact**: A regression gate whose bar can never rise ratchets in the wrong direction — either it stays permanently loose (baseline set low "to be safe") or every real improvement silently un-guards the new level. "Promote this good run to the baseline" is the natural, expected companion to gating and is entirely absent.
- **Fix sketch**: (1) Add `PATCH /v1/benchmarks/{id}` (admin-guarded like create) accepting `baseline_score` (+ optionally `judge_model`, `schedule_interval_secs`); add `update_benchmark` to the store trait and the three drivers (SQLite/PG/Firestore all already have `create` to mirror). (2) Add a convenience `POST /v1/benchmarks/{id}/promote-baseline` that reads a run's `mean_score` and writes it as the new baseline, echoing the run id it came from.
- **Trade-offs**: Moving a baseline is a governance action — record who/when (reuse the run id in a note) so a loosened gate is auditable; otherwise none material.

## 3. Pin the eval config in each run's report (reproducibility) — simple mode records almost nothing
- **Severity**: Medium
- **Category**: reproducibility
- **File**: `crates/runner/src/bench.rs:192-201` (`report = json!({ "mode": "simple" })`), `crates/runner/src/compare.rs:271-278`
- **Scenario**: A score dropped between two runs. Was it the model, or did someone edit the rubric / swap the dataset / change the judge model in between? For a simple-mode run there is no way to tell what config produced a historical score.
- **Root cause**: In simple mode the persisted `report` is just `{ "mode": "simple" }` plus significance/price annotations — it does **not** snapshot `judge_model`, the rubric text or `rubric_id`, `dataset_ref`, dataset size/hash, or the baseline in effect. Compare mode is better (records `provider`/`model`/`prompt_label`/`baseline`/`gen_samples`/`judge_samples` at `compare.rs:271-277`) but still pins no rubric version/hash or dataset identity. Since the benchmark definition is mutable in place (dataset/rubric/baseline can change under a stable id), past runs become uninterpretable — the exact "did my change make it better?" question this product answers.
- **Impact**: Undermines trust in the whole run history and blocks reliable run-vs-run diffing (#1): comparing two runs is only meaningful if you can assert they used the same rubric+dataset+judge. Cheap to add, high interpretability payoff.
- **Fix sketch**: In both `run_simple` and `run_compare`, before posting the run, fold a `config` block into `report`: `{ judge_model, rubric_id, rubric_hash, dataset_ref, n_cases, dataset_hash, baseline }` (hash the rubric text and dataset via a stable digest so an edit is detectable). Surface a "config changed since baseline" flag when a diff (#1) spans mismatched hashes.
- **Trade-offs**: Adds a few fields to the free-form `report` JSON only — no schema/column change (`report` is already opaque TEXT/JSON in all three stores); none material.
