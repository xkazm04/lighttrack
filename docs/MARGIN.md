# Margin & profit surface

`margin = recognized revenue − attributed LLM cost`, per customer or product. Revenue comes from
`revenue_events` (Stripe/Polar webhooks or manual `POST /v1/revenue`); cost is summed from `events`
(monitored ingest only — judge/benchmark spend lives in `scores`, so event cost is COGS-correct by
construction). The two streams join on the billing id carried in event `metadata.customer_id` /
`metadata.product_id`. Currency normalization to USD happens at ingest — see `docs/CURRENCY.md`.

## Endpoints

### `GET /v1/margin?by=customer|product&since=&until=&below=<pct>`
Single-window rollup, most-unprofitable first. Recognition amortizes subscriptions across their period
and nets refunds (`crates/core/src/margin.rs`). `below=<pct>` returns only rows under that margin
percentage (a free-tier row with cost and no revenue counts as below any threshold; `below=0` = the
loss-making roster). The response carries a `currency_note` / `unconverted_currencies` caveat when any
window revenue used a currency with no FX rate.

### `GET /v1/margin/trend?by=customer|product&days=N&top=M`
Per-day `{date, revenue, cost, margin}` series per dimension key, over a trailing `days` window
(default 30, capped 365). Revenue is recognized **per UTC day by the same rules** as `/v1/margin`
(the shared `recognized_amount`, applied over each one-day sub-window — no duplicated math); cost comes
from the per-day dimension rollup (`daily_cost_by_dimension`). Keys are capped to the top-`M` by
absolute total margin (default 20, `LIGHTTRACK_MARGIN_TREND_TOP_N`); a complete all-keys `totals`
series is always returned. Answers "is customer X's margin improving?".

### `GET /v1/margin/customer/:id?since=&until=`
One customer's window revenue + cost, broken down **by model** and **by use-case name**, so you can see
which models drive that customer's cost. Cost buckets read `events` scoped by
`json_extract(metadata,'$.customer_id')`.

### `GET /v1/margin/simulate?by=customer|product&price_per_mtok=<f64>&flat_monthly=<f64>&since=&until=`
Pricing **what-if**: recompute margin under a hypothetical price model, so reporting becomes decision
support. Each key's revenue is *replaced* by `price_per_mtok · tokens/1e6 + flat_monthly`, where
`tokens` is the key's prompt+completion tokens over the window (`tokens_by_dimension`) and the flat fee
is **prorated to the window length vs a 30-day month** (`flat_monthly · window_days/30`). The cost side
is the real windowed cost (the same `cost_by_dimension` machinery), and the **actual** margin — from
real `revenue_events` via `compute_margin` — rides alongside every row, so each carries
`margin_delta_usd` (`simulated − actual`), the what-if uplift. Rows sort by simulated margin ascending
(the would-still-lose-money key first).

- **At least one** of `price_per_mtok` / `flat_monthly` is required — omitting both is `400`. An unset
  price contributes nothing.
- **Read-only.** The response carries `"simulated": true` and echoes the `assumptions` (including
  `window_days`, the proration basis). Nothing is written — no revenue record is created.
- The `flat_monthly` fee is applied **per dimension key present in the window**, including the
  aggregate `unattributed` bucket (untagged usage rolls up under one key, so a per-customer flat fee is
  approximate there). The token-metered term is exact per key.
- Same `currency_note` / `unconverted_currencies` caveat as `/v1/margin`.

The pure recompute lives in `crates/core/src/margin_sim.rs` (`compute_margin_simulation`,
`hypothetical_revenue`), unit-tested for the per-key formula, proration, and param validation.

## Guardrails: from measuring the loss to acting on it

Everything above *measures*. `/v1/forecast` *predicts*. A customer-scoped `LimitRule` *caps*. Until
M4 nothing joined the three, so the cap's number was hand-typed and went stale on the next invoice —
a loss-making free-tier customer kept burning inference until a human read Slack and typed a number.

Two mechanisms close that gap. Both act **only** through the forecast sweep
(`LIGHTTRACK_FORECAST_SWEEP_SECS`), which is off by default: nothing in this section happens on a
deployment that has not switched it on.

### Derived thresholds (`revenue_share`)

A limit rule's `threshold` accepts a bare number (a fixed cap, exactly as before) **or** an object:

```json
{ "metric": "cost_usd", "window": "month", "action": "block",
  "scope": { "customer": "cus_123" },
  "threshold": { "pct": 80, "dimension": "customer" } }
```

That cap is *not* a stored number. At every evaluation — ingest admission and `/v1/limits/status`
alike — it resolves to 80% of the customer's recognized revenue over the rule's own window, using
the same recognition rule this document describes for `/v1/margin`. So it follows the invoice.

Every status carries a `basis` explaining the number it was evaluated against, and the 429 message
says it in words ("threshold = 80% of $412.00 recognized customer revenue"). Two honesty rules:

- **An unmeasurable guardrail is inert, loudly.** A customer with no revenue on file — or a backend
  that does not serve revenue at all — resolves to `+inf`, `basis.kind = "unknown"`, and never
  breaches. Resolving them to `$0.00` instead would hard-stop a new customer on their first call.
  `/v1/limits/status` counts these in `cost_basis.inert_thresholds`.
- **It costs nothing when unused.** Resolution is gated on a rule actually being `revenue_share`, so
  a deployment with only fixed caps pays not one extra query on the ingest path. When it is used,
  revenue is read once per distinct window, inside the same locked connection (SQLite) or
  advisory-locked transaction (Postgres) as the usage read and the insert.

### Escalation

A rule may carry an `escalation`:

```json
"escalation": { "on_eta_days": 2, "to": "throttle", "for_hours": 12 }
```

When the sweep's `budget_breach` forecast says this rule breaches within `on_eta_days`, it stamps
`escalated_until` and the rule *acts* as `to` until that passes. The configured `action` is never
overwritten, so de-escalation is a field clear rather than a remembered undo — and because the lapse
is stored on the row, a sweep that stops running cannot leave a project throttled indefinitely.

### Margin policies

`POST /v1/projects/:id/margin-policies` (admin only) creates a standing instruction:

```json
{ "trigger": { "below_pct": 20 }, "action": { "cap_to_revenue": { "factor": 0.8 } },
  "min_cost_usd": 25, "cooldown_secs": 3600, "expiry_secs": 86400 }
```

Triggers: `{"below_pct": N}`, `"negative_margin"`, `{"erosion_eta_days": N}` (the forecast's
`eta_unprofitable_days`). Actions: `"warn"`, `{"cap_to_revenue":{"factor":F}}` (which creates a
`revenue_share` rule, so the cap re-derives itself), `"throttle"`, `"block"`.

`min_cost_usd` is not optional in spirit: without it a customer who cost four cents and paid nothing
gets a guardrail, which is the noise that trains operators to ignore the feature.

Three properties, unit-tested in `crates/core/src/margin_policy.rs`:

1. **Idempotent.** Same picture, same rules → no writes. A timer that churned the rule table every
   tick would be worse than no timer.
2. **Origin-scoped.** Every rule a policy creates carries `origin: "margin_policy:<id>:<subject>"`,
   and the engine only ever touches rules carrying its own origin. An operator's hand-made cap is
   untouchable by automation, full stop.
3. **Self-expiring.** Every created rule carries `expires_at`. Past it the rule is inert whether or
   not a sweep is running to reap it, so a guardrail cannot outlive the condition that raised it.

Deleting a policy does **not** delete its rules inline — the sweep's reverse pass owns removal (one
way in, one way out), and until it next runs the rules lapse on their own `expires_at`.

`/v1/margin` rows carry `guardrail: <rule id>` when a policy has acted on that key, and
`margin_erosion` alerts carry `policy_applied` — so "someone should do something" becomes "this is
what is already being done".

### Where to look

- `crates/core/src/limits/threshold.rs` — `Threshold`, `ThresholdBasis`, `Escalation` (pure).
- `crates/core/src/margin_policy.rs` — `evaluate_policies` (pure; the three properties above).
- `crates/store/src/threshold.rs` — resolution against revenue, shared by every backend.
- `crates/api/src/margin_guardrails.rs` — the two sweep passes and the cooldown.

## Backend parity

| Method                     | SQLite | Postgres | Firestore |
|----------------------------|:------:|:--------:|:---------:|
| `list_revenue_events`      |  full  |   full   |   empty   |
| `cost_by_dimension`        |  full  |   full   |   empty   |
| `tokens_by_dimension`      |  full  | **empty**|   empty   |
| `daily_cost_by_dimension`  |  full  | **empty**|   empty   |
| customer model/name cost   |  full  |   empty  |   empty   |
| margin policies (M4)       |  full  |   full   |   full    |

- **SQLite** is the reference backend; every margin surface is fully served.
- **Postgres** serves `/v1/margin` fully. It does **not** yet implement `daily_cost_by_dimension` or
  `tokens_by_dimension` (both inherit the trait's empty default), so `/v1/margin/trend` returns the
  **revenue** side per day with a **zero cost** series, and `/v1/margin/simulate` returns **zero
  simulated token-revenue** (flat-fee terms still apply) until those queries are ported — a documented
  handoff, not a bug. The per-customer model/name breakdown likewise returns empty on Postgres.
- **Firestore** returns empty for the whole margin surface by default (no aggregate queries ported).

These stances follow the store trait's "additive default methods" convention: an unported backend
compiles unchanged and degrades to empty rather than erroring.
