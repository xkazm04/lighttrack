//! `Surface::Forecast`: the daily (UTC) series `GET /v1/forecast` fits a trend to.
//!
//! What has to be true for a forecast to mean anything: the series is **bucketed by UTC day** (not
//! returned as one lump), it is **windowed** to `[since, until)`, and it is **ordered oldest-first**
//! — a trend fitted to unordered or unbucketed points is a confident number about nothing.

use chrono::{Duration, Utc};

use lighttrack_core::new_id;

use super::fixtures::{sample_event, tagged_event};
use crate::Scope;
use crate::{Result, Store};

pub(super) fn forecast(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    // Two events one day apart, plus one well outside the window. Timestamps are offset a few hours
    // into the day so the bucketing is not accidentally right at a midnight boundary. `received_at`
    // moves with `ts`: the series buckets on *server arrival* deliberately (a caller must not be able
    // to reshape a forecast by backdating its own events), so setting only `ts` would place every
    // fixture in today's bucket.
    let day = |offset_days: i64, cost: f64| {
        let mut e = sample_event(&pid, "m-fc", 10, 5, cost);
        e.ts = now - Duration::days(offset_days) - Duration::hours(3);
        e.received_at = e.ts;
        e
    };
    store.insert_event(&day(1, 1.0))?;
    store.insert_event(&day(1, 2.0))?;
    store.insert_event(&day(2, 4.0))?;
    store.insert_event(&day(60, 8.0))?;

    let since = now - Duration::days(7);
    let until = now + Duration::hours(1);
    let series = store.daily_usage(&pid, since, until)?;
    assert!(
        series.len() >= 2,
        "two distinct days of traffic produce at least two buckets, not one lump: {series:?}"
    );
    assert!(
        series.windows(2).all(|w| w[0].day <= w[1].day),
        "the series is oldest-day-first — a trend fitted to unordered points is meaningless"
    );
    let total: f64 = series.iter().map(|d| d.cost_usd).sum();
    assert!(
        (total - 7.0).abs() < 1e-9,
        "the window excludes the 60-day-old event (expected 7.0, got {total})"
    );
    // Each day's own bucket, not a running total: 4.0 and 3.0 must appear separately.
    let mut costs: Vec<f64> = series.iter().map(|d| d.cost_usd).collect();
    costs.sort_by(|a, b| a.partial_cmp(b).expect("finite costs"));
    assert!(
        costs.iter().any(|c| (c - 3.0).abs() < 1e-9)
            && costs.iter().any(|c| (c - 4.0).abs() < 1e-9),
        "per-day buckets, not a cumulative series: {costs:?}"
    );

    // The per-dimension series behind margin-erosion forecasting: same bucketing, split by the
    // billing dimension read out of event metadata.
    let cpid = new_id();
    let mut a = tagged_event(&cpid, "cus-fc-a", 5.0);
    a.ts = now - Duration::days(1) - Duration::hours(3);
    a.received_at = a.ts;
    let mut b = tagged_event(&cpid, "cus-fc-b", 6.0);
    b.ts = now - Duration::days(2) - Duration::hours(3);
    b.received_at = b.ts;
    store.insert_event(&a)?;
    store.insert_event(&b)?;

    let dim = store.daily_cost_by_dimension(Scope::Project(&cpid), "customer", since, until)?;
    let for_a: f64 = dim
        .iter()
        .filter(|r| r.key.as_deref() == Some("cus-fc-a"))
        .map(|r| r.cost_usd)
        .sum();
    assert!(
        (for_a - 5.0).abs() < 1e-9,
        "the series splits by dimension value (got {for_a} for cus-fc-a): {dim:?}"
    );
    assert!(
        dim.iter().any(|r| r.key.as_deref() == Some("cus-fc-b")),
        "every dimension value with traffic in the window appears"
    );
    assert!(
        dim.windows(2).all(|w| w[0].day <= w[1].day),
        "oldest-day-first here too"
    );
    Ok(())
}
