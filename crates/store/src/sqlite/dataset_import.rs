//! Mining stored rows into a dataset (M24): the four sampling strategies, as SQL.
//!
//! They are queries and not a client-side filter for a reason that only shows up at scale: a
//! stratified quota and a uniform draw are statements about the *matched population*, and a caller
//! that fetches the newest page and filters it has already thrown that population away. That is why
//! `docs/BENCHMARK_FRAMEWORK.md` §1 promised four strategies and the runner shipped one.

use std::collections::HashSet;

use rusqlite::{types::Value as SqlValue, Connection, ToSql};

use lighttrack_core::{Dataset, ImportSource, ImportSpec, SamplingStrategy};

use super::datasets;
use crate::codec::fmt_ts;
use crate::dataset_import::{is_errors_only, prepare, stratum_quota, text_of, Candidate};
use crate::{Result, StoreError};

pub(super) fn import(
    conn: &Connection,
    project: Option<&str>,
    dataset_id: &str,
    spec: &ImportSpec,
) -> Result<u32> {
    let ds = super::dataset_fork::load_scoped(conn, project, dataset_id)?
        .ok_or_else(|| StoreError::Other(format!("dataset '{dataset_id}' not found")))?;
    if ds.frozen {
        // A Conflict, not an Other: appending to the corpus a finished run was scored against is the
        // same lie as unfreezing it, and the API answers 409 exactly as it does for `add_item`.
        return Err(StoreError::Conflict(format!(
            "dataset '{}' is frozen; fork it to add cases",
            ds.id
        )));
    }

    let candidates = select(conn, &ds, spec)?;
    let existing = if spec.dedupe {
        fingerprints(conn, &ds.id)?
    } else {
        HashSet::new()
    };
    let items = prepare(&ds.id, &candidates, spec.dedupe, &existing);

    let tx = conn.unchecked_transaction()?;
    for item in &items {
        datasets::create_item(conn, item)?;
    }
    tx.commit()?;
    Ok(items.len() as u32)
}

/// The fingerprints already in the target set — dedupe's lookup, one query instead of one per case.
fn fingerprints(conn: &Connection, dataset_id: &str) -> Result<HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT input_hash FROM dataset_items WHERE dataset_id = ?1 AND input_hash IS NOT NULL",
    )?;
    let v = stmt
        .query_map([dataset_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(v.into_iter().collect())
}

/// The `FROM … WHERE …` both the count and the selection share, with its bound parameters.
struct Where {
    sql: String,
    args: Vec<SqlValue>,
}

fn build_where(ds: &Dataset, spec: &ImportSpec) -> Where {
    let mut sql = String::from("FROM events e");
    if spec.from == ImportSource::Scores {
        // An inner join, and the pass filter rides on it: an event with no verdict is not a
        // *passing* case, it is an unjudged one, and mining it as a failure would fabricate the
        // signal the regression set exists to carry.
        sql.push_str(" JOIN scores s ON s.event_id = e.id");
    }
    sql.push_str(" WHERE e.project_id = ?1 AND e.input IS NOT NULL AND e.input <> ''");
    let mut args: Vec<SqlValue> = vec![ds.project_id.clone().into()];

    if spec.is_explicit() {
        let ph: Vec<String> = (0..spec.event_ids.len())
            .map(|i| format!("?{}", args.len() + 1 + i))
            .collect();
        sql.push_str(&format!(" AND e.id IN ({})", ph.join(",")));
        args.extend(spec.event_ids.iter().map(|id| SqlValue::from(id.clone())));
        return Where { sql, args };
    }

    if let Some(m) = &spec.filter.model {
        args.push(m.clone().into());
        sql.push_str(&format!(" AND e.model = ?{}", args.len()));
    }
    if let Some(st) = spec.filter.status {
        args.push(st.as_str().to_string().into());
        sql.push_str(&format!(" AND e.status = ?{}", args.len()));
    }
    if let Some(since) = spec.filter.since {
        args.push(fmt_ts(since).into());
        sql.push_str(&format!(" AND e.ts >= ?{}", args.len()));
    }
    if let Some(p) = spec.filter.pass {
        if spec.from == ImportSource::Scores {
            args.push(SqlValue::from(i64::from(p)));
            sql.push_str(&format!(" AND s.pass = ?{}", args.len()));
        }
    }
    // `errors` is a strategy, not a filter, but it means exactly one predicate on each source — and
    // stating it here rather than in four ORDER BY branches keeps the two readings identical.
    if is_errors_only(spec.strategy) {
        match spec.from {
            ImportSource::Events => sql.push_str(" AND e.status <> 'success'"),
            ImportSource::Scores => sql.push_str(" AND s.pass = 0"),
        }
    }
    Where { sql, args }
}

fn select(conn: &Connection, ds: &Dataset, spec: &ImportSpec) -> Result<Vec<Candidate>> {
    let w = build_where(ds, spec);
    let n = spec.effective_n();

    let (sql, limit) = if spec.strategy == SamplingStrategy::Stratified && !spec.is_explicit() {
        let groups: i64 = {
            let count = format!(
                "SELECT COUNT(*) FROM (SELECT 1 {} GROUP BY e.model, e.status)",
                w.sql
            );
            let mut stmt = conn.prepare(&count)?;
            stmt.query_row(rusqlite::params_from_iter(w.args.iter()), |r| r.get(0))?
        };
        let quota = stratum_quota(n, groups.max(0) as usize) as i64;
        (
            format!(
                "SELECT id, input, output, tags FROM (\
                   SELECT e.id AS id, e.input AS input, e.output AS output, e.tags AS tags, \
                          ROW_NUMBER() OVER (PARTITION BY e.model, e.status ORDER BY e.ts DESC) AS rn \
                   {} GROUP BY e.id\
                 ) WHERE rn <= {quota} LIMIT ?{}",
                w.sql,
                w.args.len() + 1
            ),
            (quota * groups.max(1)).min(lighttrack_core::MAX_IMPORT_N as i64),
        )
    } else {
        let order = match spec.strategy {
            SamplingStrategy::Random => "RANDOM()",
            // `recent`, `errors` and an explicit id list all read newest-first: for the explicit
            // list the order is irrelevant (it is bounded by the ids), and for the other two the
            // freshest failure is the one worth regressing on.
            _ => "e.ts DESC",
        };
        (
            format!(
                "SELECT e.id, e.input, e.output, e.tags {} GROUP BY e.id ORDER BY {order} LIMIT ?{}",
                w.sql,
                w.args.len() + 1
            ),
            n as i64,
        )
    };

    let mut args = w.args;
    args.push(limit.into());
    let params: Vec<&dyn ToSql> = args.iter().map(|v| v as &dyn ToSql).collect();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params.as_slice(), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<String>>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    Ok(rows
        .into_iter()
        .map(|(id, input, output, tags)| Candidate {
            event_id: id,
            input: text_of(&input),
            output: output.as_deref().map(text_of),
            tags: tags
                .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
                .unwrap_or_default(),
        })
        .collect())
}
