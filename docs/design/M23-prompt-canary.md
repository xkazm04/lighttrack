# M23 — Served-version quality ledger: close the loop after promotion with an online canary

Size L · gate policy (auto-revert changes a served label) · wave D · contexts: prompt-registry,
scoring, benchmark-management, store-* · depends on M10 (promotion gate), M2 (rollup), M9 (kind/rubric_id)

## Problem
Promotion is the last thing the registry observes: after `labels.insert(label, version)`
(`crates/api/src/prompts.rs::promote`) nothing measures the served version's quality.
`ResolvedPrompt.tag` (`"<name>@v<version>"`) is the documented convention for `metadata.prompt` on
every event produced with the prompt, and `GET /v1/costs/prompts` groups **cost** by it — but no
store method or endpoint groups **scores** by an event dimension. A promoted version that regresses
in production is invisible until someone eyeballs `/v1/scores`.

## Design
1. Store: `score_summary_by_dimension(project, dim: Dimension /* Prompt today */, since, until, rubric_id: Option<&str>) -> Vec<ScoreSummaryRow { key: Option<String>, n, mean, pass_rate, ci95_low, ci95_high, cost_usd }>`
   joining `scores → events` on `event_id` and grouping by `metadata.prompt` (SQLite
   `json_extract`, PG `->>`; Firestore `Unsupported` → 501). Reuse the M2 `Dimension` vocabulary.
   New `ScoreSummaries` surface; conformance section with three tagged events + scores.
2. `GET /v1/quality/prompts?project=&since=&until=&rubric_id=` (READ) → per-version rows;
   `crates/api/src/quality.rs` (new). MCP `get_prompt_quality` (read). CLI `lt prompts quality`.
   Render: a quality table.
3. `Prompt += canary: Option<CanaryPolicy { label: String (default "canary"), production_label: String (default "production"), min_n: u32, window_secs: u64, max_drop: f64, auto_revert: bool }>`
   and `label_history: Vec<LabelChange { label, version, at, reason }>` (additive JSON columns on
   the `prompts` row: SQLite `ADDED_COLUMNS_LATE`, PG `ALTER … IF NOT EXISTS` in the tail block,
   Firestore fields — the PG prompts tables exist since M10). `promote` appends to `label_history`.
4. `crates/api/src/prompt_canary_sweep.rs` (new, ≤300 LOC): a `JobKind`-free detached sweep in the
   API (same shape as `forecast_sweep`; env `LIGHTTRACK_PROMPT_CANARY_SWEEP_SECS`, off by default):
   for each prompt with a canary policy, compare canary vs production over the window with
   `stats::verdict` semantics (CI-below, paired where the same rubric scored both) and emit a
   `prompt_canary_regressed` alert through the `Alerter` (M3's ledger if merged — call the existing
   notify shape; do not restructure alerts); when `auto_revert`, move the label back to the previous
   version and append a `LabelChange { reason: "canary_regressed" }`.
5. `list_unscored_events` gains an optional `prompt` filter so `lt-runner score --prompt-tag` can
   prioritise canary traffic.
6. Docs: `BENCHMARK_FRAMEWORK.md` promotion section gains "after promotion"; register any new doc in
   `docs/catchup-marker.json`.

## Out of scope
Labels/trust (M11, same wave — do not touch `decide_gate`/`gate_promotion`; M11 owns them).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -runner,
-mcp, -cli, -render; SQLite conformance incl. the new section; a sweep unit test on a fixture that
regresses (alert fired, revert applied when enabled, not applied when disabled).

## Evaluation
Before: 0 endpoints group scores by prompt tag; `promote` has no post-condition; cost is the only
per-version surface. After: `GET /v1/quality/prompts` returns per-version quality with n and ci95;
canary sweep alerts and (opt-in) reverts, with `label_history` recording it.
