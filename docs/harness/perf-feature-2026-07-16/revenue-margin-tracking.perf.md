# Performance Optimizer — Revenue & Margin Tracking

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. `get_customer_margin` fetches every customer's revenue to compute one customer
- **Severity**: High
- **Category**: over-fetch / unbounded-result
- **File**: `crates/api/src/revenue.rs:402-419` (store method absent in `crates/store/src/sqlite/revenue.rs:62-81`)
- **Scenario**: `GET /v1/margin/customer/:id` on a project with thousands of billing customers and a long revenue history (per-usage `kind=Usage` records accumulate fast). One dashboard drill-in per customer.
- **Root cause**: The cost side is correctly scoped in SQL (`customer_cost_by_model`/`_by_name` filter `json_extract(metadata,'$.customer_id') = ?`), but the revenue side calls `store.list_revenue_events(proj, since, until)` — which returns **all** customers' revenue in the window — then discards all but one customer in Rust (`.filter(|r| r.customer_id == Some(id))`, line 412-415). Every row is deserialized `RawRevenue → RevenueEvent`, including two `parse_ts` calls each (`sqlite/revenue.rs:242-256`), before being thrown away. The `idx_revenue_customer` index on `revenue_events(customer_id)` already exists but is never exercised by this path.
- **Impact**: Work is O(project revenue rows) per request when O(one customer's rows) suffices. For a project with 500 customers of comparable revenue volume, ~99.8% of rows read, transferred, and timestamp-parsed are immediately dropped — a ~500× over-read on the revenue leg of a single-customer view.
- **Fix sketch**: Add a store method `list_revenue_events_for_customer(project, customer, since, until)` that appends `AND customer_id = ?4` to the `list` query (uses `idx_revenue_customer`), and call it from `get_customer_margin` instead of list-then-filter. The subsequent `compute_margin(&mine, …)` already expects a single-key slice, so the call site simplifies.
- **Trade-offs**: none material; strictly narrows the row set with an existing index.

## 2. Margin's revenue↔cost join scans the full events window with a non-sargable `json_extract` GROUP BY
- **Severity**: High
- **Category**: non-sargable / table-scan (hot-path)
- **File**: `crates/store/src/sqlite/revenue.rs:85-114` (and `tokens_by_dimension:119-147`); Postgres twin `crates/store-pg/src/revenue.rs:71-101`
- **Scenario**: `GET /v1/margin` (also `/simulate`, `/trend`, `/customer`) over a 30-day window on a busy project — `events` is the highest-volume table (all monitored LLM traffic), millions of rows/month. This is the per-request revenue-vs-cost aggregation.
- **Root cause**: `cost_by_dimension` groups by `json_extract(metadata, '$.customer_id')` over `WHERE project_id=? AND ts>=? AND ts<?`. `idx_events_project_ts` serves the time range, but the grouping key is a **computed JSON extraction**: every row in the window has its `metadata` TEXT blob JSON-parsed and the path extracted, then hashed/sorted for `GROUP BY`. No expression index exists on the extracted key, so the group step is unindexable. The Postgres port is worse — `metadata` is a TEXT column and the query does `(metadata::jsonb)->>'customer_id'`, casting text→jsonb **per row** in both the SELECT and GROUP BY.
- **Impact**: Per-row JSON parse across the entire windowed event set on every margin call — the dominant cost of the feature and it runs under the global SQLite `Mutex<Connection>`, so it serializes with ingest writes. At a few million events/window a full-window JSON-parse GROUP BY is seconds of mutex-held CPU, repeated for each of the four margin endpoints.
- **Fix sketch**: (SQLite) add an expression index `CREATE INDEX idx_events_cust_ts ON events(project_id, json_extract(metadata,'$.customer_id'), ts)` (and a product twin) so the extraction is precomputed and the group becomes an index scan; `cost_usd` still needs the row, but the JSON parse is eliminated. (Postgres) store `metadata` as `jsonb` (or add a generated `customer_id`/`product_id` column) and index it. Longer-term, a per-day/per-key cost rollup table maintained on ingest turns the whole join into a small indexed range read.
- **Trade-offs**: expression index adds write-time cost on event insert and disk; a rollup table adds maintenance complexity. Both are justified given this is the feature's hot path.

## 3. `list_revenue_events` wastes an ORDER BY and defeats the ts index on its period branch
- **Severity**: Medium
- **Category**: non-sargable / recompute
- **File**: `crates/store/src/sqlite/revenue.rs:62-81`; Postgres twin `crates/store-pg/src/revenue.rs:47-69`
- **Scenario**: Every `/v1/margin*` call (all four endpoints load revenue via this method), windows up to 365 days for `/trend`.
- **Root cause**: Two issues in one query. (a) `ORDER BY ts DESC` is pure waste — every caller feeds the result into `compute_margin`, which aggregates into a `BTreeMap` (`core/src/margin.rs:75-82`) and re-sorts by margin; the DB sorts the entire result set for nothing. (b) The `WHERE` is `project match AND ( (period_start & period_end non-null AND period_start<until AND period_end>since) OR (period null AND ts in window) )`. There is **no index on `period_start`/`period_end`**, and the top-level `OR` mixes the `ts` column with the period columns, so `idx_revenue_project_ts` can't satisfy the predicate — the period branch degrades to scanning all of the project's revenue rows regardless of window. Results are also unbounded (no `LIMIT`); subscription/usage histories grow without cap.
- **Impact**: Bounded by revenue volume (lower than events), so real but not catastrophic — however for a subscription-heavy biller the period branch reads the project's entire revenue table on every margin call, and the throwaway `ts DESC` sort adds an O(n log n) step the caller never uses. Both scale with total history, not with the window.
- **Fix sketch**: Drop `ORDER BY ts DESC` (caller doesn't rely on order). Add a partial index on the period branch, e.g. `CREATE INDEX idx_revenue_period ON revenue_events(project_id, period_end, period_start) WHERE period_start IS NOT NULL`, so overlap lookups are index-driven. Optionally rewrite the `OR` as a `UNION ALL` of the two sargable branches so each uses its own index.
- **Trade-offs**: extra index on writes; `UNION ALL` rewrite is more SQL to maintain. Dropping the sort is free.

---

**Checked but deliberately not filed:**
- `compute_margin` (`core/src/margin.rs`) — single pass into `BTreeMap`s, O(revenue + costs); no quadratic behavior, allocations are proportional and unavoidable. Sound.
- `recognized_amount`'s O(revenue × days) use in the per-day trend — real, but lives in `margin_trend` which the parallel audit owns; the hard `MAX_TREND_DAYS=365` cap already bounds it.
- FX/`unconverted_currencies` linear scan — trivial next to the DB work, and FX is the parallel audit's scope.
- `render/src/margin.rs` — pure string formatting over already-capped row sets; no hot path.
- `insert`/`insert_batch` upserts — correctly indexed on the `id` PK, batch wrapped in one transaction; no issue.
