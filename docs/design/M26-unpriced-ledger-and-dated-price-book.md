# M26 — Unpriced-traffic ledger, forward fill, and a dated price book

Size L · gate policy (forward fill writes stored rows) · wave C · contexts: cost-pricing,
event-ingest, limit-enforcement, margin-analytics · depends on M2 (rollup `unpriced_calls`)

## Problem
The null-cost invariant is honoured at ingest (`crates/api/src/events.rs` ~128-130,
`core/pricing.rs` ~162-163) and disclosed on traces (`TraceTotals.unpriced_spans`) and inside limit
evaluation (`CostEvidence.unpriced_calls`), but nothing lets the operator act on it: no surface lists
which `(provider, model)` pairs are unpriced and how much traffic they carry; a price landing later
never fills the historical NULLs (`docs/ARCHITECTURE.md` §7a: "nothing is written onto the event
row, and there is no price discovery"); `model_prices` is one overwritten row per model
(`crates/store/src/sqlite/prices.rs` ~13-21), `effective_date` is always `Utc::now()` at PUT time
(`crates/api/src/prices.rs` ~106), there is no `verified_at`, and the seed's `_meta.last_verified`
(2026-05-31) is surfaced nowhere at runtime. M2 added `unpriced_calls` to cost rows; this item
closes the loop: see the gap → add the price → the numbers become honest.

## Design
1. **Unpriced ledger**: `Store::list_unpriced(project: Option<&str>, since) -> Vec<UnpricedRow { provider, model, calls, input_tokens, output_tokens, first_seen, last_seen }>`
   — implement via the M2 rollup (`group_by [Provider, Model]`, filter `cost_usd IS NULL`) on
   SQLite/PG, Firestore client-side; default `Unsupported`; map into a surface (extend `EventsCore`
   or add `Pricing`). `GET /v1/costs/unpriced?project=&since=` (READ scope), ranked by calls.
   MCP `list_unpriced_models` (readOnlyHint). CLI `lt prices unpriced`.
2. **Forward fill**: `Store::fill_unpriced_cost(provider, model, price: &ModelPrice, stamp: &Value) -> u64`
   pricing existing `cost_usd IS NULL` rows for that key from the new row, per row (tier/lane
   resolution applied in Rust: select the NULL rows in pages, compute with `PriceBook`, update in
   one transaction per page), stamping `metadata.cost_source = "book_fill"` and
   `metadata.priced_at`. Rows already stamped `client`/`book` are never touched (the
   `no-retroactive-repricing` rule). SQLite + PG; Firestore batched or `Unsupported` → 501.
   `PUT /v1/prices/:provider/:model?fill_unpriced=1` (admin) returns `{filled, remaining_unpriced}`;
   opt-in per call, logged at `info`. Conformance: fill is idempotent (second fill updates 0).
3. **Dated book**: `model_prices` becomes append-only on `(provider, model, effective_from)` with
   `verified_at`, `source_url`, `note`. SQLite needs a table rebuild → a migration step in
   `crates/store/src/sqlite/schema.rs` (create new table, copy, drop, rename; idempotent); PG
   additive migration (new PK via a new table + copy, or add `effective_from` with default and
   change the PK in an `IF NOT EXISTS`-guarded DO block); Firestore doc per
   `(provider, model, effective_from)`. `PriceBook::from_rows` picks the row with the latest
   `effective_from <= now` per key. `list_prices` returns current rows;
   `GET /v1/prices/history/:provider/:model` returns the timeline. `put_price` accepts
   `verified_at`, `note`, `effective_from` (default now). Boot posture line warns when the newest
   `verified_at` is older than `LIGHTTRACK_PRICE_STALE_DAYS` (default 60); `/v1/costs` responses
   carry `price_book: { verified_at, stale: bool }`.
4. `ModelPriceRow += verified_at: Option<DateTime<Utc>>, note: Option<String>` (serde defaults);
   `config/pricing.json` rows gain `verified_at` from `_meta.last_verified`.
5. `crates/api/src/prices.rs` splits into `prices.rs` (read/history) + `prices_fill.rs`.
6. Limits: `CostEvidence` already recomputes imputation at eval time — after a fill the imputed
   share drops automatically; add a `cost_basis.notes` pointer at `/v1/costs/unpriced`.
7. `docs/PRICING.md`: "stamped vs filled vs imputed" table; `docs/ARCHITECTURE.md` §7a updated.

## Out of scope
Rollup internals (M2). Forecast (M27).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -mcp,
-cli, -render; SQLite conformance incl. fill idempotency + dated-book selection; the migration
must be proven on a DB file created from the OLD schema (write a test that opens a pre-migration
fixture and asserts the rebuild).

## Evaluation
Before: 0 routes list unpriced models; `model_prices` rows overwritten; `last_verified` invisible
at runtime. After: `/v1/costs/unpriced` rows; `PUT …?fill_unpriced=1 → {filled}` with
`unpriced_calls` trending to zero for that model; `/v1/prices/history` timeline; stale warning.
