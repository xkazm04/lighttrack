# Predictive forecasting — and the gates that keep it honest

`GET /v1/forecast` turns the daily counters LightTrack already keeps into a forward look: *which
budgets are on track to breach, and when?* and *which customers or products are trending
unprofitable?* The scheduled sweep (`LIGHTTRACK_FORECAST_SWEEP_SECS`) runs the same computation on a
timer, so the answer reaches an operator who was not already looking.

The arithmetic is small and explainable, and lives in `crates/core/src/forecast.rs`: an **EWMA
level** (the smoothed current daily value) plus a **least-squares slope** (the day-over-day trend),
then `value(t) = level + slope·t`. No training, no hidden state — the same estimate an operator
would make by eye, made precise.

This document is mostly about the other half: **when the forecast refuses to answer**.

## Why a projection needs a gate

`Trend::fit` cannot fail. Two points make a slope, and the series handed to it is *dense* — one
value per day, absent days filled with zero. Those two facts combine badly:

> A project that started spending three days ago, read over a fourteen-day lookback, is fitted over
> eleven zeros and three real observations. The slope is steeply positive **by construction**. The
> arithmetic is correct and the conclusion — "on track to breach" — is nonsense.

Before the gates, that project got paged. Nothing in the response distinguished it from a project
with a year of history and a genuine ramp.

## The three gates

All three live in `crates/core/src/forecast_gate.rs` and are applied in exactly one place on the
alert path (`crates/api/src/forecast_alerts.rs::build_alerts`), so the handler and the sweep cannot
disagree about what is sayable.

### 1. The evidence floor

A fit may be *presented* only if it clears both halves of the floor:

| Constant | Default | What it asks |
|---|---|---|
| `MIN_OBSERVED_DAYS` | 4 | at least this many **non-zero** days (`Trend::n_nonzero`) |
| `MIN_SPAN_DAYS` | 4 | first non-zero → last non-zero spans at least this many days (`Trend::span_days`) |

Four is the smallest count at which a linear fit has any residual left to be wrong about: with two
points the line is exact by construction and r² is meaninglessly `1.0`. The span half catches the
other shape — four points crowded into two adjacent days describe a spike, not a trend.

`Trend::presentability(min_points, min_span_days)` returns `Ok(())` or a `Refusal` whose `reason` is
written to be read, not parsed: `"4 observed days needed, 2 seen"`, `"observations span 3 days, 4
needed"`.

### 2. The flat band

`FLAT_BAND` (5%) is a band around zero slope, sized **relative to the level**. A slope whose
magnitude is within `FLAT_BAND × level` per day is treated as flat by `Trend::effective_slope`, and
the `days_until_*` crossings use it. A $100/day spend drifting by $2/day is noise on a steady spend,
not a trend worth an ETA — and at a horizon measured in days, a doubling three weeks out is not a
prediction anyone should act on today.

### 3. Burn-rate corroboration

`Trend::corroborated()` requires the last three days' mean to sit above the window's own baseline
mean before a rising projection may page anyone. This rules out the ETA carried by an *old* spike
still sitting inside the lookback: the fit slopes upward, but the burn rate has already cooled.

The comparison is against the window mean rather than the EWMA `level` deliberately. The EWMA is
itself weighted toward the newest points, so on any genuinely rising series it sits slightly *above*
the trailing mean — gating on that would suppress exactly the alerts worth sending.

## The definition seam: where zero-fill may start

Filling an absent day with `0.0` asserts *"the project spent nothing that day"*. That is only true
once there was a project to observe. `crates/api/src/forecast.rs::first_observed` decides where the
assertion becomes legitimate:

* **A project that predates the window** (`created_at <= start of lookback`) could have spent on any
  day in it, so every quiet day inside is a genuine zero. Nothing is trimmed.
* **A project created inside the window** has its whole life in view. Zero-filling the calendar days
  in front of its first evidence of existence is fitting a slope through prehistory, so those days
  are **omitted from the series** rather than zero-filled.

"First evidence of existence" is the *earliest* of the project's `created_at` and its first day with
traffic — evidence beats bookkeeping, so a `created_at` backfilled after the fact cannot erase
history that plainly happened.

Limit rules have no `created_at` of their own, so the seam is project-level. The evidence floor is
the backstop for the case the seam cannot see (an old project whose spend only started on Tuesday).

## What the response says

```jsonc
{
  "spend": {
    "cost_trend": { "level": 9.1, "slope": 1.0, "n": 10,
                    "n_nonzero": 10, "span_days": 10, "r2": 0.99,
                    "recent_mean": 9.0, "window_mean": 5.5 },
    "confidence": 0.99          // r², or null under the floor
  },
  "budgets": [ { "rule_id": "…", "eta_days": 5.6, … } ],
  "margins": [ { "key": "acme", "eta_unprofitable_days": 3.2, … } ],
  "alerts":  [ { "kind": "budget_breach", "severity": "high", "message": "… (confidence r²=0.99 over 10 days)" } ],
  "refused": [ { "subject": "spend", "reason": "4 observed days needed, 2 seen" } ]
}
```

* A withheld projection has its ETA set to **`null`** and appears in `refused[]`. The JSON never
  carries a number the alert path has already decided is unsayable.
* `confidence` (and `Trend.r2`) is **withheld under the same floor**. A confidence attached to a
  projection we would refuse to show is the same lie in smaller type.
* `refused[]` is always present, possibly empty. That is what lets a reader — human or agent —
  distinguish **"no risk"** from **"no evidence"**. An empty `alerts` next to a non-empty `refused`
  means the forecast could not see, not that all is well.
* An alert message appends `(confidence r²=0.87 over 12 days)` when the fit published one.

`lookback` is clamped to `4..=90` (`horizon` stays `1..=90`). Asking for two days cannot buy a
forecast the floor would refuse anyway.

## Alert de-duplication

Forecast alerts are keyed `forecast:<project>:<kind>:<subject>:<severity>` and suppressed by the
shared `Alerter` cooldown (see `docs/ALERTS.md`). The key carries no trace of *how* the forecast was
triggered, so enabling the sweep cannot double an operator's volume — but it does carry
**severity**, because the same subject going `warning` → `high` inside one cooldown window is
precisely the message worth sending, and a severity-free key was swallowing it.

## Backend parity

The forecast needs `daily_usage` and `daily_cost_by_dimension` — the `forecast` surface in
`docs/PARITY.md`. All three backends (SQLite, Postgres, Firestore) declare it; on Postgres and
Firestore the two series are served through the rollup primitive's trait defaults.

A backend that refused them could not forecast at all, which is a property of the *deployment*, not
of a project. The sweep therefore detects `Unsupported` **once per sweep** and logs a single line
pointing at `docs/PARITY.md`, rather than one line per project per tick forever.
