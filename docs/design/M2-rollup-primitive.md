# M2 — One grouped-rollup primitive behind every cost/usage/margin/forecast surface

Size XL · gate contract (only if legacy DTOs are removed — this build is additive) · wave B ·
contexts: cost-pricing, event-ingest, limit-enforcement, margin-analytics, predictive-forecast,
store-interface, store-sqlite-core, store-postgres, store-firestore

## Problem
Eight `Store` methods answer "usage and cost over a window grouped by one dimension", each with its
own signature and row type (`crates/store/src/lib.rs`): `cost_summary`/`cost_summary_windowed`
(~749-764 → `CostRow`), `usecase_costs` (~768 → `UseCaseCostRow`), `usage_by_scope` (~806 →
`ScopeUsage`), `daily_usage`/`daily_cost_by_dimension` (~820-838), `cost_by_dimension`/
`tokens_by_dimension` (~1164-1186), `customer_cost_by_model`/`customer_cost_by_name` (~1188-1210).
Four are SQLite-only (`tokens_by_dimension`, `customer_cost_by_*`, `daily_cost_by_dimension`), so on
the production Postgres backend `/v1/margin/simulate|trend|customer/:id` and `/v1/forecast` return
501 and the forecast sweep warns per project per tick. SQLite carries five near-identical GROUP BY
strings (`sqlite/revenue.rs` ~88-203, `sqlite/forecast.rs` ~47-65); Postgres already has a generic
`scope_expr` (`store-pg/src/events/usage.rs` ~20-28) unused by its revenue module. No row DTO
carries an unpriced count, so every aggregate sums NULL cost as $0. The dimension vocabulary is
triplicated (`LimitScope::kind_str` in `core/limits.rs` ~61-66; the `"customer"|"product"|"prompt"`
`dim` string args; the SQL whitelist in `sqlite/events.rs` ~728).

## Design
1. `crates/core/src/rollup.rs` (new, <300 LOC):
   ```rust
   pub enum Dimension { Project, Provider, Model, Name, ApiKey, Customer, Product, Prompt, Day }
   pub enum TimeKey { Ts, ReceivedAt }           // accounting reads key on received_at
   pub struct RollupQuery<'a> { project: Option<&'a str>, group_by: Vec<Dimension> /*1..=3, unique*/,
       since: DateTime<Utc>, until: Option<DateTime<Utc>>, time_key: TimeKey,
       filter: Vec<(Dimension, String)> }
   pub struct RollupRow { keys: Vec<Option<String>>, calls: u64, input_tokens: u64, output_tokens: u64,
       cost_usd: f64, unpriced_calls: u64, client_reported_cost_usd: f64, errors: u64 }
   ```
   `Dimension::parse(&str)` is the single vocabulary; `LimitScope::kind_str` and the `dim` args
   route through it. `Dimension::storage()` → `Column("provider") | MetadataPath("$.customer_id") | DayOf(time_key)`.
2. `Store::rollup(&self, q: &RollupQuery) -> Result<Vec<RollupRow>>` default `Unsupported`.
   Implement **once per backend**: `crates/store/src/sqlite/rollup.rs` (SQL builder from the fixed
   whitelist; `json_extract(metadata,'$.x')`; `substr(<time_key>,1,10)` for Day;
   `SUM(cost_usd IS NULL)`; `SUM(CASE WHEN json_extract(metadata,'$.cost_source')='client' THEN cost_usd END)`;
   reuse `project_pred` so the sargable index path pinned by the test at `sqlite/revenue.rs` ~317 holds),
   `crates/store-pg/src/rollup.rs` (extend `scope_expr` with product/prompt; `COUNT(*) FILTER`),
   `crates/store-firestore/src/rollup.rs` (client-side fold over the existing windowed query,
   bounded by existing scan caps; Day bucketing on the chosen time key — Firestore has no
   `received_at` yet, so `TimeKey::ReceivedAt` falls back to `ts` there and the row carries no
   marker; document it). Declare the `Rollup` surface in each backend's M1 manifest.
3. Adapters: `crates/store/src/rollup_compat.rs` — the eight legacy methods become **trait
   default impls over `rollup`** mapping to the legacy DTOs (so a backend that implements `rollup`
   gets all eight; SQLite may keep its hand-written versions this wave — do not delete them yet).
   `CostRow`/`UseCaseCostRow`/`CostByDimension` gain `unpriced_calls: u64` (serde default 0).
4. Conformance: one fixture set; assert for each legacy method that `rollup`-derived rows equal the
   legacy rows on SQLite (ordering normalised), and that `unpriced_calls` counts the NULL-cost rows.
   A `Rollup`-surface refusal assertion via the M1 driver for backends that do not declare it (none
   after this wave).
5. API: `GET /v1/rollup?project=&by=model,day&since=&until=&time=received_at&filter=customer:acme`
   (project key or admin; `api_key` dimension labels admin-only) in `crates/api/src/rollup.rs`;
   `/v1/costs`, `/v1/costs/prompts`, `/v1/usecases`, `/v1/limits/usage` responses gain
   `unpriced_calls` (additive). Migrate their handlers to call `rollup` through the compat layer or
   directly — behaviour identical, shapes identical plus the new field. Margin and forecast readers
   (`revenue.rs`, `forecast.rs`) keep calling the legacy method names; they now succeed on PG/Firestore
   through the default impls.
6. MCP: `query_rollup` read tool (readOnlyHint). Render: a generic rollup table in `crates/render`.
7. `docs/DATA_MODEL.md`: the dimension table (one authority) and the `unpriced_calls` disclosure rule.
   Fix the stale parity table in `docs/MARGIN.md`.

## Out of scope
Deleting the legacy trait methods (a later cleanup once callers migrate). Price-book changes (M26).
Forecast gating logic (M27).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -mcp,
-render; SQLite conformance; new equality tests.

## Evaluation
Before: 8 grouped methods, 7 DTOs, 4 SQLite-only, 0 DTOs with an unpriced count; `/v1/forecast`
and 3 margin endpoints 501 on PG. After: 1 primitive per backend; the 4 SQLite-only methods answer
on all backends through defaults; every cost row carries `unpriced_calls`; equality conformance green.
