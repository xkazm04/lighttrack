# M27 — Forecast honesty gates and backend parity: a projection that can refuse

Size L · gate none · wave C · contexts: predictive-forecast, alert-delivery, store-postgres,
store-firestore · depends on M2 (daily series via rollup) and M4 (do not disturb its escalation pass)

## Problem
`Trend::fit_with` produces a slope for any `n >= 2` (`crates/core/src/forecast.rs` ~39-58,
~216-235); the handler clamps `lookback` to a minimum of 2 (`crates/api/src/forecast.rs` ~90).
`densify` fills every absent day with `0.0` (~241-252): a project that started spending three days
ago inside a 14-day lookback is fitted over eleven zeros and three real points, so the slope is
steeply positive by construction and `build_alerts` pages "on track to breach" for flat spend.
There is no flat band, no r²/confidence, no refusal output. `daily_usage`/`daily_cost_by_dimension`
were SQLite-only until M2's rollup defaults; verify they now answer on PG/Firestore and that the
sweep no longer warns per project per tick (`forecast_sweep.rs` ~134-136). `docs/ALERTS.md` ~94
references `docs/PREDICTIVE.md`, which does not exist.

## Design
1. `crates/core/src/forecast.rs`: `Trend += n_nonzero: usize, span_days: u32 (first→last non-zero
   observation), r2: Option<f64>` (withheld under the evidence floor); `presentability(&self, min_points, min_span_days) -> Result<(), Refusal { reason: String }>`
   with copy-ready prose ("4 observed days needed, 2 seen"); `FLAT_BAND` constant relative to
   level applied in `days_until_*` (a slope inside the band is "flat", not a trend). Split into
   `forecast.rs` + `forecast_gate.rs` if over 300 LOC. Unit tests: two-point perfect fit refuses;
   young-project zero-fill case no longer reads as rising; flat band.
2. `crates/api/src/forecast.rs`: `densify` takes `first_observed: Option<NaiveDate>` and fills
   zeros only after the first observed day (definition seam = the project's first event or the
   rule's `created_at`, whichever is later); `lookback.clamp(4, 90)`; `ForecastResponse +=
   refused: Vec<Refusal { subject, reason }>`; `SpendProjection += confidence: Option<f64>`.
3. `crates/api/src/forecast_alerts.rs`: `build_alerts` consumes only gated forecasts; message
   appends "confidence r²=0.87 over 12 days" or omits when withheld; burn-rate corroboration —
   require the last 3 days' mean to exceed the level before paging (`pre-breach-forecasting`).
   Dedup key gains severity (a warning→high escalation within one cooldown is the message worth
   sending — incidental defect from the scan).
4. Parity: assert via the M1 manifest and conformance that `daily_usage`/`daily_cost_by_dimension`
   answer on PG and Firestore through M2's rollup defaults; if a backend still refuses, implement
   the two series directly (`store-pg/src/events/forecast.rs`; Firestore client-side day bucketing
   on `ts`) and declare the `Forecast` surface. `forecast_sweep.rs`: detect `Unsupported` once per
   sweep, not per project.
5. MCP `get_forecast` description mentions refusals. `docs/PREDICTIVE.md` created (the gate, the
   flat band, the definition seam); `docs/ALERTS.md` reference fixed.
6. `tests_forecast.rs`: fixtures lengthened to ≥4 observed days where they expect alerts; add a
   refusal case.

## Out of scope
Escalation/derived thresholds (M4 — do not edit its escalation pass beyond consuming `refused`).

## Gates
`cargo build/test/clippy` for lighttrack-core, -store, -store-pg, -store-firestore, -api, -mcp;
SQLite conformance.

## Evaluation
Before: min fit 2 points; zero-fill precedes first observation; 0 confidence fields; sweep warns
per project on non-SQLite. After: alerts with `span_days < 4` = 0 (test); `refused[]` present with
reasons; `Forecast` surface declared on all three backends; one warn per sweep at most.
