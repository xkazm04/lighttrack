# Performance Optimizer — Model Pricing & Cost Rollup

> Total: 2
> Critical: 0 | High: 1 | Medium: 1 | Low: 0

## 1. Full price-book scan on every ingested event (twice for dated model IDs)
- **Severity**: High
- **Category**: hot-path / table-scan
- **File**: `crates/core/src/pricing.rs:195-223` (`resolve_exact`), reached from `resolve` (178-193) via `cost_usd_mode` (156-174) via `event::ensure_cost` on every single/batch ingest (`crates/api/src/events.rs:35-36`).
- **Scenario**: Every LLM event whose `cost_usd` isn't client-supplied is priced from the book on the ingest hot path (`prepare_event` → `ensure_cost` → `cost_usd_mode` → `resolve`). At real ingest volume this is the single most-executed code path touching pricing — thousands of calls/sec under load, and once per item in every batch (`events_batch.rs:91`).
- **Root cause**: `resolve_exact` never does a plain O(1) lookup first. For the prompt-length tier feature it builds `format!("{}/{}@in>", provider, model)` (line 209) and then **iterates the entire `entries` HashMap** (lines 211-217) computing `strip_prefix`/`parse` on every key — even though the seed (`config/pricing.json`, 15 rows) and the overwhelmingly common book contain **zero** `@in>N` rows. Worse: real Anthropic/OpenAI model IDs arrive as dated snapshots (e.g. `claude-haiku-4-5-20251001`), so the exact base key never matches; `resolve` (line 188) then trims the date suffix and calls `resolve_exact` **again**, incurring a **second** full scan. Each call also allocates 2-4 transient `String`s (`Self::key`, the `@in>` prefix, the suffix key) per event.
- **Impact**: O(N·M) work for N events over an M-entry book, where an O(N) indexed lookup suffices; doubled for the common dated-model case, plus ~4·N throwaway allocations. With M≈15 it's small but the constant is pure waste on the hottest path; as variant/tier rows and models accumulate (M into the hundreds) per-event latency grows linearly and starts showing up in ingest p99.
- **Fix sketch**: Try the exact base lookup first (`self.entries.get(&Self::key(...))`) and return immediately in the no-variant case. Only when tier pricing is actually in play do the scan — and gate it behind a cheap flag (e.g. store a `has_tier_rows: bool` or a `HashMap<model_key, Vec<(threshold, price)>>` sidecar built once in `from_rows`/`new`) so events for models without tiers never iterate. Optionally intern the `provider/model` key to cut allocations. Preserve the "highest exceeded threshold wins" semantics.
- **Trade-offs**: Adds a small precomputed index rebuilt on each book swap (already infrequent — see #2). None material on the read path.

## 2. `put_price` re-reads the entire price book from the DB after each single edit
- **Severity**: Medium
- **Category**: recompute / n-plus-one (round-trip)
- **File**: `crates/api/src/prices.rs:53-63`; store reads `crates/store/src/sqlite/prices.rs:35-40`, `crates/store-pg/src/prices.rs:36-42`, `crates/store-firestore/src/prices.rs:24-29`.
- **Scenario**: An admin updates one model's price via `PUT`. The handler does a first `spawn_db` to `upsert_price`, then a **second** `spawn_db` to `list_prices()` — a full re-read of `model_prices` — solely to rebuild the in-memory book for hot-swap.
- **Root cause**: The reload discards the fact that exactly one row changed and re-materializes the whole table. On SQLite/PG that's an extra round trip + full `SELECT ... ORDER BY` on the global `Mutex<Connection>` (serialized against ingest). On **Firestore** `list_prices` issues a `runQuery` over the whole `model_prices` collection with no limit — **billed per document read** — on every price edit, and then re-sorts client-side (line 27) only for the result to be dropped into an order-independent HashMap by `from_rows`.
- **Impact**: 2× DB round trips per edit and, on Firestore, one full-collection read (per-doc billing) each time an admin touches a single price. Bounded because edits are rare, but it's avoidable money/latency and it contends the SQLite mutex with the ingest hot path.
- **Fix sketch**: After a successful `upsert_price`, mutate the in-memory book directly — insert/replace the single `ModelPrice` under `PriceBook`'s write lock (add a `PriceBook::upsert(key, ModelPrice)` helper) — instead of round-tripping `list_prices`. Reserve the full reload for startup/seed (`main.rs:182`). Drops the second `spawn_db` and the Firestore full-collection read entirely.
- **Trade-offs**: The in-memory book must stay consistent with the row just written; since the handler already holds the exact `ModelPriceRow` it wrote, this is a straight insert — no staleness window (the current full-reload has the same visibility, one writer). No cross-process invalidation exists today either way.

---

### Checked and deliberately NOT filed
- **`render/costs.rs` (`summary`/`usecases`) and `render/prices.rs`**: receive **already-aggregated** rows (GROUP BY happens in SQL upstream, in the neighbour `events.rs`/store layer covered by other audits) and only sort + sum in memory — O(n log n) over a bounded, pre-grouped result. No scan, no recompute worth flagging.
- **`st.prices` `RwLock` read on ingest**: it's a cheap read-lock over an in-memory book; not contended (writes are rare admin edits). Not a finding.
- **`list_prices` server-side `ORDER BY` then re-sort in render**: minor redundant ordering, but `list` is not on the hot path and the sort is cheap — not worth a Low.
- The pricing book being cached in-memory (`Arc<RwLock<PriceBook>>`) already resolves the classic "reference data re-read per event" concern — so the caching finding you'd expect is **already implemented**; I did not invent a staleness problem, since #2's direct-mutate fix keeps it fresh.
