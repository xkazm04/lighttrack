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

## Backend parity

| Method                     | SQLite | Postgres | Firestore |
|----------------------------|:------:|:--------:|:---------:|
| `list_revenue_events`      |  full  |   full   |   empty   |
| `cost_by_dimension`        |  full  |   full   |   empty   |
| `daily_cost_by_dimension`  |  full  | **empty**|   empty   |
| customer model/name cost   |  full  |   empty  |   empty   |

- **SQLite** is the reference backend; every margin surface is fully served.
- **Postgres** serves `/v1/margin` fully. It does **not** yet implement `daily_cost_by_dimension`
  (inherits the trait's empty default), so `/v1/margin/trend` on Postgres returns the **revenue** side
  per day with a **zero cost** series until the query is ported — a documented handoff, not a bug. The
  per-customer model/name breakdown likewise returns empty on Postgres.
- **Firestore** returns empty for the whole margin surface by default (no aggregate queries ported).

These stances follow the store trait's "additive default methods" convention: an unported backend
compiles unchanged and degrades to empty rather than erroring.
