//! Rolling-window usage: the project total, one scope dimension, and the per-value breakdown that
//! answers "who is spending" before a cap fires.

use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_core::LimitScope;
use lighttrack_store::{Result, ScopeUsage, StoreError, Usage};

use super::cols::{map_usage, RECEIVED, USAGE_COLS};
use crate::util::{fmt_ts, pgerr};

/// The SQL expression yielding one scope dimension's value for a row — columns for the three
/// original dimensions, a `jsonb` extraction for the two that ride in `metadata` (`api_key_id` is
/// server-stamped at ingest, `customer_id` is the billing linkage). The `NULLIF(metadata,'')::jsonb`
/// cast is the same one [`USAGE_COLS`] uses; see the note there.
///
/// Fixed literals chosen by the enum discriminant — never user input, so safe to interpolate. Values
/// are always bound.
pub(crate) fn scope_expr(kind: &str) -> Option<&'static str> {
    match kind {
        "provider" => Some("provider"),
        "model" => Some("model"),
        "name" => Some("name"),
        "api_key" => Some("(NULLIF(metadata,'')::jsonb)->>'api_key_id'"),
        "customer" => Some("(NULLIF(metadata,'')::jsonb)->>'customer_id'"),
        _ => None,
    }
}

pub(crate) async fn usage_since(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Usage> {
    let row = sqlx::query(&format!(
        "SELECT {USAGE_COLS} FROM events WHERE project_id = $1 AND {RECEIVED} >= $2"
    ))
    .bind(project.to_string())
    .bind(fmt_ts(since))
    .fetch_one(pool)
    .await
    .map_err(pgerr)?;
    map_usage(&row)
}

/// Rolling usage restricted to one scope dimension (provider / model / use-case name / API key /
/// billing customer). A NULL dimension never matches, mirroring the SQLite reference.
pub(crate) async fn usage_since_scoped(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
    scope: &LimitScope,
) -> Result<Usage> {
    let expr = scope_expr(scope.kind_str()).unwrap_or("NULL");
    let sql = format!(
        // `{RECEIVED}`, not `ts`: a scoped window is still a window, and a backdated client clock
        // must not slide spend out of it. `{expr}` generalizes the dimension to the two that ride
        // in `metadata` as well as the three that are columns.
        "SELECT {USAGE_COLS} FROM events \
         WHERE project_id = $1 AND {RECEIVED} >= $2 AND {expr} = $3"
    );
    let row = sqlx::query(&sql)
        .bind(project.to_string())
        .bind(fmt_ts(since))
        .bind(scope.value().to_string())
        .fetch_one(pool)
        .await
        .map_err(pgerr)?;
    map_usage(&row)
}

/// Rolling usage since `since` grouped by every distinct value of one scope dimension — the
/// pre-breach "who is spending" view. Rows carrying no value on the dimension fold into a single
/// `NULL` bucket rather than being dropped, so the parts still sum to the project total.
pub(crate) async fn usage_by_scope(
    pool: &PgPool,
    project: &str,
    since: chrono::DateTime<chrono::Utc>,
    kind: &str,
) -> Result<Vec<ScopeUsage>> {
    let expr = scope_expr(kind)
        .ok_or_else(|| StoreError::Other(format!("unknown scope dimension '{kind}'")))?;
    let sql = format!(
        "SELECT {expr} AS k, {USAGE_COLS} FROM events \
         WHERE project_id = $1 AND ts >= $2 GROUP BY k ORDER BY 2 DESC"
    );
    let rows = sqlx::query(&sql)
        .bind(project.to_string())
        .bind(fmt_ts(since))
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
    rows.iter()
        .map(|r| {
            Ok(ScopeUsage {
                value: r.try_get(0).map_err(pgerr)?,
                usage: Usage {
                    cost_usd: r.try_get(1).map_err(pgerr)?,
                    calls: r.try_get(2).map_err(pgerr)?,
                    tokens: r.try_get(3).map_err(pgerr)?,
                    unpriced_calls: r.try_get(4).map_err(pgerr)?,
                    client_cost_usd: r.try_get(5).map_err(pgerr)?,
                },
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_expr_knows_the_column_dimensions_and_the_metadata_ones() {
        assert_eq!(scope_expr("provider"), Some("provider"));
        assert_eq!(scope_expr("model"), Some("model"));
        assert_eq!(scope_expr("name"), Some("name"));
        // These two ride in `metadata`, so they are extractions rather than columns.
        assert!(scope_expr("api_key").is_some_and(|e| e.contains("api_key_id")));
        assert!(scope_expr("customer").is_some_and(|e| e.contains("customer_id")));
        assert_eq!(scope_expr("nonsense"), None);
    }

    /// The gap this closes: a dimension added to `LimitScope` but not taught to `scope_expr` falls
    /// back to the literal `NULL` in the admission query, so `NULL = value` matches nothing and a
    /// configured cap silently never fires. Enumerating `KINDS` makes that a build-time failure.
    #[test]
    fn every_core_scope_dimension_has_an_expression() {
        for kind in LimitScope::KINDS {
            assert!(scope_expr(kind).is_some(), "no scope_expr for '{kind}'");
        }
    }
}
