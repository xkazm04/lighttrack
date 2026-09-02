# M4 — Measure→act: derived thresholds and margin guardrails as limit policy

Size L · gate policy · wave B · contexts: limit-enforcement, predictive-forecast, margin-analytics,
alert-delivery

## Problem
The product measures who is losing money (`filter_below`, `crates/api/src/revenue.rs` ~56-63),
predicts who will breach (`margin_erosion`/`budget_breach` alerts, `crates/api/src/forecast_alerts.rs`
~66-98, with `eta_unprofitable_days` from `core/forecast.rs` ~156-212), and owns a customer-scoped
cap mechanism (`LimitScope::Customer`, `core/limits.rs` ~57/114; CRUD on every backend, `store/lib.rs`
~876-893). Nothing connects them: `build_alerts`' only consumer is `Alerter::notify_forecast`
(`alerts.rs` ~288-302), a text post; `LimitRule.threshold: f64` (`core/limits.rs` ~251) is a
hand-typed constant; `create_limit_rule` has no caller outside the limits handler. A loss-making
free-tier customer keeps burning inference until a human reads Slack and types a number that goes
stale on the next invoice.

## Design
1. `crates/core/src/limits.rs`: `pub enum Threshold { Fixed(f64), RevenueShare { pct: f64, dimension: Dimension /* Customer today */ } }`
   with `#[serde(untagged)]` so existing rows deserialize to `Fixed` (byte-identical behaviour);
   `LimitRule.threshold: Threshold`; `validate()` extends (pct in (0, 1000]); `LimitStatus +=
   basis: ThresholdBasis { kind: "fixed"|"revenue_share", revenue_usd: Option<f64>, pct: Option<f64> }`
   so estimation announces itself. Keep `core/limits.rs` under control — it is ~838 LOC already:
   split `limits_threshold.rs` for the new types.
2. `LimitRule += escalation: Option<Escalation { on_eta_days: f64, to: LimitAction, for_hours: u32 }>`
   and `escalated_until: Option<DateTime<Utc>>` + `origin: Option<String>` (e.g. `margin_policy:<id>`).
   Store: persist as JSON in the rule row (`threshold_json`, `escalation_json`, `origin` columns —
   additive on SQLite/PG/Firestore; all three already implement limit CRUD, keep parity).
3. Resolution: `evaluate_admission` (`store/lib.rs` ~405-440) gains a `resolve_threshold: &dyn Fn(&LimitRule) -> Result<(f64, ThresholdBasis)>`
   argument. For `RevenueShare` the API resolves recognized revenue over the rule window for the
   customer via `list_revenue_events`/`cost_by_dimension` (implemented on all three backends) and
   caches per (project, customer, window) with the `RedactionCache`-style TTL in `state.rs`.
   SQLite resolves inside the same locked connection; PG inside its advisory-locked transaction;
   Firestore keeps its non-atomic path. Unknown revenue → basis `unknown`, rule evaluates as
   `Fixed(f64::INFINITY)` (never a surprise block) and the status says so.
4. `crates/core/src/margin_policy.rs`: `MarginPolicy { id, project_id, trigger: BelowPct(f64) | NegativeMargin | ErosionEtaDays(f64), min_cost_usd, action: Warn | CapToRevenue { factor } | Throttle | Block, cooldown_secs, expiry_secs, enabled }`
   and the pure `evaluate_policies(rows, forecasts, existing_rules, now) -> Vec<RuleChange>` —
   idempotent (same inputs → no changes), never touches rules without a matching `origin`.
   Store CRUD on all three backends (`margin_policies` table).
5. Sweep: in `forecast_sweep.rs` after `compute_forecast` (a) apply escalations —
   `budget_breach` alert with `eta_days <= on_eta_days` → `update_limit_rule` to `to` with
   `escalated_until`; reverse pass restores when calm; (b) evaluate margin policies and apply
   `RuleChange`s via the existing limit CRUD. Only the sweep does this, only when it is on.
6. API: `CreateLimitReq/UpdateLimitReq` accept `threshold` as number **or** object (untagged);
   `LimitStatusResp.cost_basis.notes` gains the revenue-basis caveat; `POST/GET/DELETE
   /v1/projects/:id/margin-policies` (admin); `/v1/margin` rows gain `guardrail: Option<rule_id>`;
   `breach_message` names the basis ("threshold = 80% of $412.00 recognized revenue").
7. Alerts: `margin_erosion` payload carries `policy_applied: rule_id` when a policy fired.
8. MCP: `list_margin_policies` (read); status already exposes rules. Docs: `docs/MARGIN.md`
   "Guardrails", `docs/ARCHITECTURE.md` §7 (rule origin is auditable).

## Out of scope
Forecast presentability gates (M27 — do not change `Trend` here). Alert persistence (M3). Daily
series parity on PG/Firestore (M2 provides it via rollup defaults; if `daily_usage` still returns
`Unsupported` on a backend in your worktree, the escalation pass must skip with one warn per
sweep, not per project).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -mcp;
SQLite conformance; a unit test that `evaluate_policies` is idempotent and never edits a rule
without `origin`; a test that an old rule JSON with numeric `threshold` deserializes unchanged.

## Evaluation
Before: 1 threshold form; forecast alerts have 1 consumer; 0 code paths from margin/forecast to
rules. After: `revenue_share` rules exist and their basis appears on `/v1/limits/status` and in 429
messages; policy-originated rules carry `origin`; sweep escalates and de-escalates (unit-tested on
a fixture).
