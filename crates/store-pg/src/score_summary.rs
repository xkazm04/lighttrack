//! Verdicts summarized per value of one event [`Dimension`], Postgres (M23).
//!
//! Mirrors `store/src/sqlite/score_summary.rs` semantics exactly — same join, same normalization by
//! `max`, same window on the *verdict's* `created_at` — because this is the backend production runs
//! on, and a quality surface that answers differently on the deployment that matters is worse than
//! one that refuses.
//!
//! The `metadata` extraction carries the crate's `NULLIF(...,'')::jsonb` guard: a bare cast **raises**
//! on invalid JSON, which would fail the whole read rather than skew one bucket.

use sqlx::postgres::PgPool;
use sqlx::Row;

use chrono::{DateTime, Utc};
use lighttrack_core::{Dimension, Storage};
use lighttrack_store::{Result, ScoreSummaryRow};

use crate::util::{fmt_ts, pgerr};

/// One dimension's value on the joined **event** row; `Day` is the verdict's own day (see the
/// SQLite twin for why).
fn key_expr(d: Dimension) -> String {
    match d.storage() {
        Storage::Column(c) => format!("e.{c}"),
        Storage::MetadataKey(k) => format!("(NULLIF(e.metadata,'')::jsonb)->>'{k}'"),
        Storage::Day => "substr(s.created_at,1,10)".to_string(),
    }
}

/// The SQL and its binds, built without a database so the shape can be pinned by a unit test — the
/// only way to check a Postgres statement in a suite that must run without one.
fn build(
    project: Option<&str>,
    dim: Dimension,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    rubric_id: Option<&str>,
) -> (String, Vec<String>) {
    let key = key_expr(dim);
    let mut binds: Vec<String> = Vec::new();
    let mut conds: Vec<String> = Vec::new();
    if let Some(p) = project {
        binds.push(p.to_string());
        conds.push(format!("s.project_id = ${}", binds.len()));
    }
    binds.push(fmt_ts(since));
    conds.push(format!("s.created_at >= ${}", binds.len()));
    if let Some(u) = until {
        binds.push(fmt_ts(u));
        conds.push(format!("s.created_at < ${}", binds.len()));
    }
    if let Some(r) = rubric_id {
        binds.push(r.to_string());
        conds.push(format!("s.rubric_id = ${}", binds.len()));
    }
    // `"max"` is quoted throughout this crate: it is a reserved word in Postgres.
    let norm = "(s.value / s.\"max\")";
    let sql = format!(
        "SELECT {key} AS k, COUNT(*)::bigint, \
           COALESCE(SUM({norm}),0.0)::double precision, \
           COALESCE(SUM({norm} * {norm}),0.0)::double precision, \
           COALESCE(SUM(CASE WHEN s.pass = 1 THEN 1 ELSE 0 END),0)::bigint, \
           COALESCE(SUM(e.cost_usd),0.0)::double precision \
         FROM scores s JOIN events e ON e.id = s.event_id \
         WHERE {conds} AND s.\"max\" > 0 GROUP BY 1",
        conds = conds.join(" AND "),
    );
    (sql, binds)
}

pub(crate) async fn score_summary(
    pool: &PgPool,
    project: Option<&str>,
    dim: Dimension,
    since: DateTime<Utc>,
    until: Option<DateTime<Utc>>,
    rubric_id: Option<&str>,
) -> Result<Vec<ScoreSummaryRow>> {
    let (sql, binds) = build(project, dim, since, until, rubric_id);
    let mut q = sqlx::query(&sql);
    for b in &binds {
        q = q.bind(b.clone());
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|r| {
            let key: Option<String> = r.try_get(0).map_err(pgerr)?;
            let n: i64 = r.try_get(1).map_err(pgerr)?;
            let sum: f64 = r.try_get(2).map_err(pgerr)?;
            let sum_sq: f64 = r.try_get(3).map_err(pgerr)?;
            let passes: i64 = r.try_get(4).map_err(pgerr)?;
            let cost: f64 = r.try_get(5).map_err(pgerr)?;
            Ok(summarize(
                key,
                n.max(0) as u64,
                sum,
                sum_sq,
                passes.max(0) as u64,
                cost,
            ))
        })
        .collect()
}

/// The aggregate tuple → a row with its ~95% interval. Same arithmetic as the SQLite twin, clamped
/// at zero variance so a bucket where every verdict agreed cannot produce a NaN interval.
fn summarize(
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
        (((sum_sq - nf * mean * mean) / (nf - 1.0)).max(0.0)).sqrt() / nf.sqrt()
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

    /// Nothing from the wire reaches the SQL: the only interpolated fragment is a literal chosen by
    /// a `Dimension` variant, and the project / window / rubric are always bound.
    #[test]
    fn the_statement_binds_every_value_and_interpolates_only_enum_literals() {
        let since = Utc::now();
        let (sql, binds) = build(
            Some("p1"),
            Dimension::Prompt,
            since,
            Some(since),
            Some("rub-1"),
        );
        assert_eq!(binds.len(), 4, "project, since, until, rubric: {binds:?}");
        assert_eq!(binds[0], "p1");
        assert_eq!(binds[3], "rub-1");
        assert!(sql.contains("->>'prompt'"), "{sql}");
        assert!(sql.contains("JOIN events e ON e.id = s.event_id"), "{sql}");
        assert!(
            sql.contains("s.created_at >= $2") && sql.contains("s.created_at < $3"),
            "the window is on the verdict, not the event: {sql}"
        );
        assert!(
            sql.contains("s.\"max\" > 0"),
            "`max` is reserved in Postgres and the zero-scale guard must survive quoting: {sql}"
        );
        assert!(!sql.contains("p1"), "no value is inlined: {sql}");
    }

    /// An admin read (no project) must not leave a `$1` with nothing bound to it.
    #[test]
    fn an_unscoped_read_renumbers_its_placeholders() {
        let (sql, binds) = build(None, Dimension::Model, Utc::now(), None, None);
        assert_eq!(binds.len(), 1);
        assert!(sql.contains("s.created_at >= $1"), "{sql}");
        assert!(!sql.contains("project_id"), "{sql}");
        assert!(sql.contains("e.model AS k"), "{sql}");
    }

    #[test]
    fn the_interval_matches_the_sqlite_twin_at_the_degenerate_cases() {
        let one = summarize(Some("a@v1".into()), 1, 0.8, 0.64, 1, 0.5);
        assert_eq!((one.ci95_low, one.ci95_high), (0.8, 0.8));
        let flat = summarize(None, 10, 8.0, 6.4, 5, 1.0);
        assert!(flat.ci95_low.is_finite() && (flat.ci95_high - flat.ci95_low).abs() < 1e-9);
        assert_eq!(flat.pass_rate, 0.5);
    }
}
