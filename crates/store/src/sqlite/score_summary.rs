//! Verdicts summarized per value of one event [`Dimension`], SQLite.
//!
//! The join nothing else in the store made: `scores → events` on `event_id`, grouped on a value
//! that lives on the *event*. Cost has been groupable by `metadata.prompt` since the registry
//! shipped; quality has not, which is why a promoted version that regressed in production was
//! visible only to whoever scrolled `/v1/scores`.
//!
//! Every interpolated fragment is a fixed literal chosen by a [`Dimension`] variant (the same
//! `key_expr` shape `sqlite/rollup.rs` uses); the project, the window and the rubric are bound.

use rusqlite::types::ToSql;
use rusqlite::{params_from_iter, Connection, Row};

use chrono::{DateTime, Utc};
use lighttrack_core::{Dimension, Storage};

use crate::codec::fmt_ts;
use crate::{Result, ScoreSummaryRow};

/// One dimension's value on the joined **event** row. `Day` is the verdict's own day here: a
/// summary keyed on the day the judging happened is the only reading of "day" that means anything
/// once the grouping is over verdicts rather than events.
fn key_expr(d: Dimension) -> String {
    match d.storage() {
        Storage::Column(c) => format!("e.{c}"),
        Storage::MetadataKey(k) => format!("json_extract(e.metadata,'$.{k}')"),
        Storage::Day => "substr(s.created_at,1,10)".to_string(),
    }
}

pub(super) fn score_summary(
    conn: &Connection,
    project: Option<&str>,
    dim: Dimension,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    rubric_id: Option<&str>,
) -> Result<Vec<ScoreSummaryRow>> {
    let key = key_expr(dim);
    // `?1` project (NULL = every project), `?2` window start; the rest are appended in push order.
    let mut args: Vec<Box<dyn ToSql>> = vec![
        Box::new(project.map(str::to_string)),
        Box::new(fmt_ts(since)),
    ];
    let mut conds = vec![
        "(?1 IS NULL OR s.project_id = ?1)".to_string(),
        "s.created_at >= ?2".to_string(),
    ];
    if let Some(u) = until {
        args.push(Box::new(fmt_ts(u)));
        conds.push(format!("s.created_at < ?{}", args.len()));
    }
    if let Some(r) = rubric_id {
        args.push(Box::new(r.to_string()));
        conds.push(format!("s.rubric_id = ?{}", args.len()));
    }

    // `max > 0` guards the normalization: a verdict recorded with a zero scale has no comparable
    // value, and dividing by it would poison the whole bucket's mean with an infinity.
    //
    // The mean and the sample variance come back from one pass (SUM(x), SUM(x*x), COUNT) rather than
    // from SQLite's own aggregate set, which has no stddev: the interval is what makes this a
    // measurement instead of two bare means, so it is computed here rather than left to the caller.
    let norm = "(s.value / s.max)";
    let sql = format!(
        "SELECT {key} AS k, COUNT(*), \
           COALESCE(SUM({norm}),0.0), COALESCE(SUM({norm} * {norm}),0.0), \
           COALESCE(SUM(CASE WHEN s.pass = 1 THEN 1 ELSE 0 END),0), \
           COALESCE(SUM(e.cost_usd),0.0) \
         FROM scores s JOIN events e ON e.id = s.event_id \
         WHERE {conds} AND s.max > 0 GROUP BY 1",
        conds = conds.join(" AND "),
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params_from_iter(args.iter()), map_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `(key, n, Σx, Σx², passes, cost)` → a summarized bucket.
fn map_row(r: &Row) -> rusqlite::Result<ScoreSummaryRow> {
    let key: Option<String> = r.get(0)?;
    let n: i64 = r.get(1)?;
    let sum: f64 = r.get(2)?;
    let sum_sq: f64 = r.get(3)?;
    let passes: i64 = r.get(4)?;
    let cost: f64 = r.get(5)?;
    Ok(summarize(
        key,
        n.max(0) as u64,
        sum,
        sum_sq,
        passes.max(0) as u64,
        cost,
    ))
}

/// Turn the aggregate tuple into a row with its ~95% interval.
///
/// Sample variance from the sums (`(Σx² − n·mean²)/(n−1)`), clamped at zero: with floating point,
/// a bucket where every verdict scored identically can land a hair below zero and `sqrt` would
/// hand back a NaN interval on the most confident bucket there is.
pub(crate) fn summarize(
    key: Option<String>,
    n: u64,
    sum: f64,
    sum_sq: f64,
    passes: u64,
    cost_usd: f64,
) -> ScoreSummaryRow {
    if n == 0 {
        return ScoreSummaryRow {
            key,
            n: 0,
            mean: 0.0,
            pass_rate: 0.0,
            ci95_low: 0.0,
            ci95_high: 0.0,
            cost_usd,
        };
    }
    let nf = n as f64;
    let mean = sum / nf;
    let stderr = if n < 2 {
        0.0
    } else {
        let var = ((sum_sq - nf * mean * mean) / (nf - 1.0)).max(0.0);
        var.sqrt() / nf.sqrt()
    };
    let half = ScoreSummaryRow::Z_95 * stderr;
    ScoreSummaryRow {
        key,
        n,
        mean,
        pass_rate: passes as f64 / nf,
        ci95_low: mean - half,
        ci95_high: mean + half,
        cost_usd,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_dimension_reads_from_the_joined_event_or_the_verdicts_day() {
        for d in Dimension::ALL {
            let e = key_expr(*d);
            assert!(
                e.starts_with("e.")
                    || e.starts_with("json_extract(e.")
                    || e.contains("s.created_at"),
                "{d:?} → {e}"
            );
        }
        assert_eq!(
            key_expr(Dimension::Prompt),
            "json_extract(e.metadata,'$.prompt')",
            "the served-version tag is the whole point of this surface"
        );
    }

    /// The interval is the gate's evidence, so its degenerate cases must be values a comparison can
    /// read rather than NaNs that make every test pass.
    #[test]
    fn the_interval_degrades_honestly_at_small_n_and_zero_spread() {
        let one = summarize(Some("a@v1".into()), 1, 0.8, 0.64, 1, 0.5);
        assert_eq!(one.mean, 0.8);
        assert_eq!(
            (one.ci95_low, one.ci95_high),
            (0.8, 0.8),
            "n=1 has no spread to estimate, so the interval collapses to the mean"
        );
        assert_eq!(one.pass_rate, 1.0);

        // Ten identical verdicts: variance is exactly zero, and float noise must not make it -1e-17.
        let flat = summarize(None, 10, 8.0, 6.4, 5, 1.0);
        assert!((flat.mean - 0.8).abs() < 1e-12);
        assert!(flat.ci95_low.is_finite() && flat.ci95_high.is_finite());
        assert!((flat.ci95_high - flat.ci95_low).abs() < 1e-9);
        assert_eq!(flat.pass_rate, 0.5);

        // A real spread widens it: 0.6 and 1.0 → mean 0.8, sample sd 0.2√2… → a non-empty interval.
        let spread = summarize(None, 2, 1.6, 0.6 * 0.6 + 1.0, 0, 0.0);
        assert!((spread.mean - 0.8).abs() < 1e-12);
        assert!(spread.ci95_high > spread.ci95_low + 0.1, "{spread:?}");
        assert_eq!(spread.pass_rate, 0.0);
    }

    #[test]
    fn an_empty_bucket_is_a_zero_not_a_nan() {
        let z = summarize(Some("k".into()), 0, 0.0, 0.0, 0, 0.0);
        assert_eq!(z.n, 0);
        assert!(z.mean.is_finite() && z.ci95_low.is_finite());
    }
}
