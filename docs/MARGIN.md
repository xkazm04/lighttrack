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

## Backend parity

| Method                     | SQLite | Postgres | Firestore |
|----------------------------|:------:|:--------:|:---------:|
| `list_revenue_events`      |  full  |   full   |   full    |
| `cost_by_dimension`        |  full  |   full   |   full    |
| `tokens_by_dimension`      |  full  |   full   |   full    |
| `daily_cost_by_dimension`  |  full  |   full   |   full    |
| customer model/name cost   |  full  |   full   |   full    |

The whole margin surface is served on all three backends. It used to be SQLite-only past the first
two rows: `/v1/margin/trend` returned a revenue series against a **zero cost** series on Postgres, and
`/v1/margin/simulate` returned zero simulated token-revenue — numbers that read as answers.

What closed it is one method. Every row above is now a fixed grouping of `Store::rollup`
(`docs/DATA_MODEL.md`), reached through trait default impls, so a backend that implements the
primitive serves all of them with identical semantics rather than five hand-written queries that
could each be forgotten. `docs/PARITY.md` is generated from the backends' own manifests and is the
authority if this table ever falls behind it.

Two caveats that survive:

- **Firestore** stores no `received_at`, so its windows and day buckets read the client-declared
  `ts`. A caller with a skewed clock can shift its own spend between margin-trend buckets there.
- Every cost row carries `unpriced_calls`. When it is non-zero the cost — and therefore the margin
  computed from it — is a **floor**: those calls had no price in the book and contributed `$0.00`.
