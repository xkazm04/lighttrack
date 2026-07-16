# Performance Optimizer — Margin Simulation & FX

> Total: 3
> Critical: 1 | High: 2 | Medium: 0 | Low: 0

## 1. Firestore `cost_by_dimension` full-scans the entire `events` collection on every margin request

- **Severity**: Critical
- **Category**: full-table-scan / external-call
- **File**: `crates/store-firestore/src/revenue.rs:49-76` (and `:29-45` for the same shape on `revenue_events`)
- **Scenario**: Any project on the Firestore backend hitting `GET /v1/margin`, `/v1/margin/simulate`, `/v1/margin/trend` or `/v1/forecast` (`crates/api/src/revenue.rs:125,224` and `crates/api/src/forecast.rs:222`). `events` is the ingest table — one doc per monitored LLM call — so it is the largest collection in the system by orders of magnitude. A project with 5M logged calls asking for a 30-day margin window pulls all 5M docs to compute a rollup whose window may contain 200k of them.
- **Root cause**: `rest.query("events", &project_filter(project), None, None)` sends only a `project_id EQUAL` filter — `order` and `limit` are both `None`. The `[since, until)` predicate is applied *client-side* in the Rust loop (`if ts < since || ts >= until { continue; }`), after every matching doc has been read, transferred, JSON-decoded and `parse_ts`'d. The module header rationalizes this ("Firestore has no `OR` predicate or `GROUP BY`/`SUM`"), which is true of the *aggregation* but not of the range filter — a single-field inequality on `ts` ANDed with `project_id EQUAL` is a plain composite-index query that Firestore supports natively. `list()` (`:35`) has the identical omission on `revenue_events`.
- **Impact**: Three distinct costs, all scaling with total project history rather than window size. (1) **Money**: Firestore bills per document read at ~$0.06/100k. 5M docs = ~$3.00 *per margin request*; one dashboard polling every 30s burns ~$8.6k/day against a table that answers in milliseconds on SQLite. (2) **Latency**: 5M docs over the REST API at ~1KB/doc is ~5GB of JSON per request. (3) **Memory**: `rest.query` returns `Vec<Fields>` — the entire collection is materialized in RAM before the filter loop starts, so a large project OOMs the process rather than merely being slow. The work is fully wasted: the response is a handful of aggregate rows.
- **Fix sketch**: Push the window into the query. `fmt_ts` (`crates/store/src/codec.rs:22`) writes `to_rfc3339_opts(SecondsFormat::Nanos, true)` — fixed-width, always `Z`-suffixed — so these strings sort lexicographically in exact chronological order and Firestore string inequalities are sound against them without a schema change:
  ```rust
  let mut filters = project_filter(project);
  filters.push(("ts", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(since))));
  filters.push(("ts", "LESS_THAN", json!(fmt_ts(until))));
  let docs = rest.query("events", &filters, None, None)?;
  ```
  `build_sq` (`crates/store-firestore/src/rest.rs:138`) already ANDs an arbitrary filter list, so no client changes are needed. Keep the client-side `ts` check as a cheap guard. Requires a `(project_id ASC, ts ASC)` composite index — Firestore's error response names the index to create. Apply the same to `list()`, where the two-bound `recognizable()` predicate can be narrowed by `ts`/`period_end >= since` on the indexable field and the overlap logic kept client-side. Metric to watch: docs-read per margin request should fall to the window's event count; alert if it tracks collection size.
- **Trade-offs**: Needs one composite index deployed before the query stops erroring (fail-loud, not silent — acceptable, but it is a deploy-ordering step). Cannot remove *all* client-side work: the `GROUP BY`/`SUM` genuinely has no Firestore equivalent, so a very wide window still streams its events. Pagination on top would bound memory further but is a larger change.

## 2. `compute_margin_trend` materializes a dense series for every key, then throws all but `top_n` away

- **Severity**: High
- **Category**: allocation / unbounded-growth
- **File**: `crates/core/src/margin_trend.rs:99-111` (with `build_series:115-142`)
- **Scenario**: `GET /v1/margin/trend?days=365` on a project with many distinct billing dimension keys. `key_count` is deliberately uncapped (it is reported so the client can say "showing 20 of N"), so a B2C-shaped project with 50k customer ids gets 50k series built at full 365-day density — and then `series.truncate(top_n)` at `:110` discards 49,980 of them.
- **Root cause**: The ordering is build-everything → total → sort → truncate. `build_series` allocates a `Vec<MarginTrendPoint>` of `days.len()` for each key, and each point carries `date: d.clone()` — a heap-allocated `String` per (key, day) pair. The cap is applied last because `build_totals` (`:106`) needs all keys, and because the sort key (`total_margin_usd`) is a byproduct of `build_series`. But totals only need per-day sums across keys, and the sort key only needs two scalars per key — neither actually requires the dense point vectors to exist.
- **Impact**: Peak allocation is O(key_count × days). At 50k keys × 365 days that is 18.25M `MarginTrendPoint`s: ~24 bytes of `f64` plus a `String` (24-byte header + a 10-byte heap buffer) each, so roughly **1.0–1.2 GB transient**, plus 18.25M individual malloc/free round-trips for date strings that are 20 distinct values repeated. This is a single-request memory spike inside `spawn_db`'s blocking pool — concurrent trend requests multiply it, and the response that survives is ~20 series (a few hundred KB). `MAX_TREND_DAYS = 365` bounds the `days` factor; nothing bounds `key_count`.
- **Fix sketch**: Two independent changes, either of which helps; together they make the cost proportional to the *output*.
  1. **Select before building.** Compute per-key `(total_revenue, total_cost)` from the `rev`/`cost` maps directly (each is already a `BTreeMap<key, BTreeMap<day, f64>>` — sum its values, O(populated entries), no dense vec). Accumulate the per-day `totals` from those same maps in the same pass. Then sort the `(key, |margin|)` pairs, take `top_n`, and call `build_series` only for the surviving keys. Peak drops from `key_count × days` to `top_n × days` (~7.3k points).
  2. **Stop cloning the date.** `date: String` is 20 distinct values shared across every series. Make `MarginTrendPoint.date` an `Arc<str>` (or have `day_strings` own the values and store an index), so the per-point cost is a refcount bump rather than a malloc. Serializes identically.
  Note `build_totals:144-155` also indexes `s.points[i]` across all series — after fix (1) it no longer can, which is why the totals must move into the map pass. It also sums the *rounded* per-point values, so totals shift by <1e-6 if recomputed from raw; pin that in the existing `top_n_caps_by_absolute_margin_and_totals_are_complete` test.
- **Trade-offs**: The totals accumulation moves away from `build_totals`'s straightforward "sum the built series" shape into the map pass — slightly less obvious, and the property that totals are computed pre-cap must stay covered by the existing test. `Arc<str>` in a public serialized struct is a minor API ripple for `core` consumers.

## 3. `compute_margin_trend` recognizes every revenue event against every day in the window

- **Severity**: High
- **Category**: hot-path / quadratic-algorithm
- **File**: `crates/core/src/margin_trend.rs:73-87`
- **Scenario**: `GET /v1/margin/trend?days=365` where the project has a year of revenue records. Subscription billing generates roughly one event per customer per month, so 10k customers is ~120k events; `list_revenue_events` returns them all for the window. The nested loop then runs 120k × 365 ≈ **44M** iterations.
- **Root cause**: For each event the code walks *all* `day_strings` and calls `recognized_amount` on each one-day sub-window. But the vast majority of events are point-in-time (`period_start`/`period_end` = `None`), and for those `recognized_amount` (`crates/core/src/margin.rs:142-149`) reduces to `r.ts >= since && r.ts < until` — it can match at most **one** day, so 364 of 365 calls are guaranteed-zero work done to find it. Subscription events match only the days their period overlaps, which is likewise a contiguous, directly-computable index range. Worse, the loop body reconstructs the day boundary from scratch every iteration:
  ```rust
  let d0 = (start_day + Duration::days(i as i64)).and_hms_opt(0, 0, 0).unwrap().and_utc();
  let d1 = d0 + Duration::days(1);
  ```
  `day_strings` was precomputed once at `:67`, but these `DateTime`s — which depend only on `i`, not on `r` — are rebuilt R times each: ~44M date constructions with a fallible `and_hms_opt().unwrap()` per call.
- **Impact**: Single-threaded CPU burn inside `spawn_db`'s blocking pool, occupying a pool slot and stalling other DB work for the duration. The dominant term is the 44M redundant `DateTime` constructions, not the arithmetic. Roughly 99.7% of the iterations for point-in-time events produce nothing. The `MAX_TREND_DAYS = 365` clamp and its comment ("to bound the O(revenue × days) recognition work") bound the `days` factor only — the revenue count is whatever the window holds, so the ceiling rises with the business.
- **Fix sketch**: Hoist the invariant, then index directly instead of scanning.
  1. Precompute `day_bounds: Vec<(DateTime<Utc>, DateTime<Utc>)>` once alongside `day_strings` (365 constructions total, down from 44M) and index it in the loop. This alone is a few lines and removes the dominant cost with zero behavior change.
  2. Narrow the day range per event. Point-in-time events (`(period_start, period_end)` not both `Some`, or `pe <= ps` — mirror `recognized_amount`'s own match arms so the rules cannot drift): compute `i = (r.ts.date_naive() - start_day).num_days()`, bounds-check it, and touch that one day. Period events: clamp `[period_start, period_end)` to the window and derive the contiguous `i` range. Total work becomes O(R + Σ overlap_days) instead of O(R × D).
  Keep `recognized_amount` as the sole arithmetic — the fix only decides *which* days to call it for, preserving the module's "no duplicated recognition math" invariant. The existing tests (`point_in_time_lands_on_its_day_only`, `subscription_amortizes_evenly_across_days`, `refund_is_negative_on_its_day`) cover exactly the boundary behavior at risk. Also worth folding in: `rev.entry(key.clone())` clones the key on every hit — hoist the entry lookup out of the day loop.
- **Trade-offs**: Step (1) is pure win. Step (2) adds a day-index computation that duplicates `recognized_amount`'s point-vs-period *dispatch* (not its math), so a future third `RevenueKind` shape must be reflected in both places — a real, if small, coupling risk. If step (1) alone brings the trend within budget, deferring step (2) is defensible.

---

### Checked and deliberately not filed

- **`fx.rs` — per-request FX provider calls**: does not exist. `shared_fx()` (`:136-139`) is a `OnceLock<Arc<FxTable>>` seeded once from `config/fx_rates.json`; `to_usd` is a `HashMap` lookup with no I/O and no external HTTP. The classic "FX rate fetched per request" shape is genuinely absent here — the static-book design already forecloses it.
- **`to_usd` / `is_convertible` allocating a `String` per call via `to_uppercase()`** (`fx.rs:116,129`): real, but the call sites are per-revenue-record (`crates/api/src/revenue.rs:88-92`, and the billing adapters at ingest), where revenue-record counts are orders of magnitude below event counts and the surrounding work (HTTP, JSON) dwarfs one small allocation. Micro-optimization; not worth the readability cost.
- **`margin_sim.rs`**: no finding survived scrutiny. `compute_margin_simulation` is O(R + C + T + K log K) — one `BTreeMap` build over tokens, one pass over `compute_margin`'s output, one sort. There is no scenario *grid*: `SimAssumptions` is a single point in price space, so the "simulate over a grid" shape the context describes is not what the code does. Its only real cost is the store queries its caller (`crates/api/src/revenue.rs:221-231`) issues, which is finding #1 on the Firestore backend.
- **`build_totals`'s O(key_count × days) pass** (`margin_trend.rs:144-155`): the same shape as finding #2 and fixed by the same change; folded in there rather than double-counted.
