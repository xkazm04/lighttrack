# Feature Scout — Scoring Rubrics

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Rubrics carry no version, and scores don't record which rubric produced them
- **Severity**: High
- **Category**: versioning
- **File**: `crates/core/src/rubric.rs:27-40`, `crates/runner/src/rubric.rs:130-137`, `crates/core/src/score.rs:38-62`
- **Scenario**: A team runs a benchmark under rubric "faithfulness-v1", tightens the weights next month, then tries to explain why last quarter's scores were higher. There is no way to know whether the drop is model quality or a rubric change — the two are indistinguishable in the stored data.
- **Root cause**: `Rubric` has `id`, `name`, `threshold`, `dimensions`, `created_at` — but **no `version` field and no lineage pointer** (`rubric.rs:27-40`). The API surface is create + read only: the router exposes `POST/GET /v1/projects/:id/rubrics` and `GET /v1/rubrics/:id` — no `PUT`/`PATCH`/`DELETE` (`api/src/main.rs:263-266`, confirmed in `api/src/rubrics.rs`). Worse, when the runner posts each score it sets `"rubric": format!("bench:{}", bench.name)` (`runner/src/rubric.rs:132`) — the **rubric id is thrown away**. `Score.rubric` is a free-text name (`score.rs:47`), and `BenchmarkRun.report` stores `rubric.name`/`threshold` but not the id (`runner/src/rubric.rs:222-227`). A score is therefore not reproducible against the exact rubric it graded under.
- **Impact**: Reproducibility is the core promise of an eval product; competitors (Braintrust/Langfuse) pin every score to an immutable rubric version. Today, editing a rubric silently rewrites the meaning of all past scores that reference it by name, and there's no audit trail. This caps LightTrack's credibility for regulated/regression-tracking use.
- **Fix sketch**: (1) Add `version: u32` (and optional `parent_id`) to `Rubric`; treat rubrics as append-only — an "edit" writes a new row with `version+1`, same logical name. (2) Stamp `rubric_id` + `version` onto every `Score` and into `BenchmarkRun.report` at judge time (`runner/src/rubric.rs`). (3) Store column already holds `dimensions` as JSON, so add `version` to `COLS` in all three drivers (`store/src/sqlite/rubrics.rs:10`, `store-pg/src/rubrics.rs:11`, `store-firestore/src/rubrics.rs:16`). (4) `get_rubric` resolves latest version by default; add `?version=N` for pinned historical fetch.
- **Trade-offs**: Adds a column + migration across three stores; the name→latest-version resolution is new query logic. Immutability may surprise users who expect in-place edit — mitigate with a clear "new version created" response.

## 2. Dimensions are numeric-only — no boolean, categorical, or enum score types
- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/core/src/rubric.rs:5-20`, `crates/core/src/score.rs:7-35`, `crates/render/src/rubrics.rs:44-58`
- **Scenario**: An operator wants a rubric where "contains PII" is a hard yes/no gate and "tone" is one of {professional, neutral, casual} — not a 0–1 number. Today they can only express everything as a fuzzy float, forcing the judge to invent a numeric value for inherently categorical questions, which self-consistency then averages into meaningless decimals (e.g. "PII = 0.33").
- **Root cause**: `RubricDimension` has no score-type field; the only quantitative slots are `weight`, `anchors` (freeform strings), and `floor: Option<f64>` (`rubric.rs:5-20`). The judge contract `JudgeVerdict.score` is a bare `f64` (`score.rs:9`, schema at `score.rs:23-35`), and the render layer formats every dimension as `{:.2}` (`render/src/rubrics.rs:54`). There is no way to declare a dimension as boolean or categorical, so the "score-type variety" that eval products differentiate on is absent.
- **Impact**: Rubric expressiveness is explicitly where this product competes. Boolean gates (pass/fail guardrails) and categorical labels (classification eval) are table-stakes for LangSmith/Braintrust. Without them, whole eval classes — safety gates, multi-class labeling, refusal detection — can't be modeled cleanly.
- **Fix sketch**: Add `score_type: ScoreType` to `RubricDimension` (enum `Numeric { min, max }` | `Boolean` | `Categorical { options: Vec<String> }`), defaulting to `Numeric` for back-compat via `#[serde(default)]`. Thread the type into the per-dimension judge schema (neighbour engine) and into aggregation: boolean → treat as 0/1 with an implied floor; categorical → don't average, report the modal label + agreement. Render (`render/src/rubrics.rs:detail`) shows the type and, for categoricals, the option set instead of a weight column.
- **Trade-offs**: Categorical dimensions don't fold into the weighted-mean `overall`, so the aggregation code in `runner/src/rubric.rs` needs a branch (exclude from the numeric mean, surface separately). Prompt/schema construction lives in the neighbour engine — coordinate so the judge emits the right type.

## 3. No rubric library — no templates, clone, or import
- **Severity**: Medium
- **Category**: power-feature
- **File**: `crates/api/src/rubrics.rs:30-49`, `crates/store/src/sqlite/rubrics.rs:12-20`
- **Scenario**: A new project wants a solid "RAG faithfulness" or "summarization quality" rubric. Today the only path is to hand-author every dimension, weight, and anchor from scratch via a raw `POST` body — there's no starter set and no way to copy a proven rubric from another project.
- **Root cause**: `create_rubric` accepts a full inline `CreateRubricReq` and writes one rubric scoped to a single project (`api/src/rubrics.rs:30-49`); rubrics are hard-partitioned by `project_id` in every list query (`WHERE project_id = ?`, e.g. `store/src/sqlite/rubrics.rs:29-30`). There is no seed/template concept, no `clone`, and no import endpoint — every rubric is bespoke, per project.
- **Impact**: Steep cold-start; teams re-derive the same well-known rubrics, and best-practice anchoring never propagates. A curated template library is a strong onboarding + differentiation lever and pairs naturally with versioning (#1).
- **Fix sketch**: (1) Ship a set of built-in template rubrics (JSON) seeded at a reserved `project_id` (e.g. `"_templates"`) and readable by all. (2) Add `POST /v1/rubrics/:id/clone` (and/or a `from_rubric_id` field on create) that deep-copies dimensions into the caller's project as version 1. (3) Optional: accept a rubric JSON body for import/export round-trip (the `dimensions` JSON is already the on-disk shape).
- **Trade-offs**: A shared-template project needs a read exception in the auth guard (`resolve_read_project`); keep templates read-only to non-admins. Minimal store changes — reuses existing create/list paths.
