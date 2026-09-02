//! `Surface::DatasetLineage`: forking a dataset, mining rows into one, and the version history (M24).
//!
//! Declared on this backend rather than left to refuse, because Postgres is where a deployment with
//! enough traffic to *need* a stratified or failure-mined corpus actually runs. A 501 here would
//! mean the one loop that turns production failures into permanent eval cases exists only on the
//! laptop backend.

use std::collections::HashSet;

use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_core::{new_id, Dataset, DatasetItem, ImportSource, ImportSpec, SamplingStrategy};
use lighttrack_store::dataset_import::{
    is_errors_only, prepare, stratum_quota, text_of, Candidate,
};
use lighttrack_store::{Result, StoreError};

use crate::datasets::{self, dataset_from_row, DATASET_COLS};
use crate::util::{fmt_ts, pgerr};

async fn load_scoped(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Option<Dataset>> {
    let row = sqlx::query(&format!(
        "SELECT {DATASET_COLS} FROM datasets WHERE id = $1"
    ))
    .bind(id.to_string())
    .fetch_optional(pool)
    .await
    .map_err(pgerr)?;
    let Some(d) = row.as_ref().map(dataset_from_row).transpose()? else {
        return Ok(None);
    };
    // A dataset outside the caller's project is *absent*, not forbidden: the store has no notion of
    // a principal, and the API has already decided who may see what.
    match project {
        Some(p) if d.project_id != p => Ok(None),
        _ => Ok(Some(d)),
    }
}

pub(crate) async fn fork(pool: &PgPool, project: Option<&str>, id: &str) -> Result<Dataset> {
    let src = load_scoped(pool, project, id)
        .await?
        .ok_or_else(|| StoreError::Other(format!("dataset '{id}' not found")))?;

    // Past the highest version the NAME already carries, not past the source's own: forking v1 twice
    // must not mint two v2s a version pin can no longer tell apart.
    let next: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(version), 0) + 1 FROM datasets WHERE project_id = $1 AND name = $2",
    )
    .bind(src.project_id.clone())
    .bind(src.name.clone())
    .fetch_one(pool)
    .await
    .map_err(pgerr)?;

    let forked = Dataset {
        id: new_id(),
        project_id: src.project_id.clone(),
        name: src.name.clone(),
        version: next.max(1) as u32,
        frozen: false,
        source: src.source.clone(),
        created_at: chrono::Utc::now(),
        parent_id: Some(src.id.clone()),
    };
    datasets::create(pool, &forked).await?;
    for item in datasets::list_items(pool, &src.id).await? {
        let copy = DatasetItem {
            id: new_id(),
            dataset_id: forked.id.clone(),
            ..item.clone()
        };
        datasets::create_item(pool, &copy).await?;
        copy_labels(pool, &item.id, &copy.id).await?;
    }
    Ok(forked)
}

/// Carry the human verdicts on a copied item forward onto its copy (M11) — copied, never moved: the
/// frozen parent is evidence a past calibration was measured on, and a golden case whose label did
/// not survive the fork is an ungraded string the next calibration silently measures against.
async fn copy_labels(pool: &PgPool, from_item: &str, to_item: &str) -> Result<()> {
    let ids: Vec<String> = sqlx::query_scalar(
        "SELECT id FROM labels WHERE subject_kind = 'dataset_item' AND subject_id = $1",
    )
    .bind(from_item.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    for label_id in ids {
        sqlx::query(
            "INSERT INTO labels \
             (id, project_id, subject_kind, subject_id, rubric_id, value, pass, dimensions, \
              labeler, note, created_at) \
             SELECT $1, project_id, 'dataset_item', $2, rubric_id, value, pass, dimensions, \
                    labeler, note, created_at \
             FROM labels WHERE id = $3",
        )
        .bind(new_id())
        .bind(to_item.to_string())
        .bind(label_id)
        .execute(pool)
        .await
        .map_err(pgerr)?;
    }
    Ok(())
}

pub(crate) async fn versions(
    pool: &PgPool,
    project: Option<&str>,
    name: &str,
) -> Result<Vec<Dataset>> {
    let rows = sqlx::query(&format!(
        "SELECT {DATASET_COLS} FROM datasets \
         WHERE ($1::text IS NULL OR project_id = $1) AND name = $2 \
         ORDER BY version DESC, created_at DESC"
    ))
    .bind(project.map(str::to_string))
    .bind(name.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(dataset_from_row).collect()
}

pub(crate) async fn import(
    pool: &PgPool,
    project: Option<&str>,
    dataset_id: &str,
    spec: &ImportSpec,
) -> Result<u32> {
    let ds = load_scoped(pool, project, dataset_id)
        .await?
        .ok_or_else(|| StoreError::Other(format!("dataset '{dataset_id}' not found")))?;
    if ds.frozen {
        return Err(StoreError::Conflict(format!(
            "dataset '{}' is frozen; fork it to add cases",
            ds.id
        )));
    }

    let candidates = select(pool, &ds, spec).await?;
    let existing = if spec.dedupe {
        let hs: Vec<String> = sqlx::query_scalar(
            "SELECT input_hash FROM dataset_items \
             WHERE dataset_id = $1 AND input_hash IS NOT NULL",
        )
        .bind(ds.id.clone())
        .fetch_all(pool)
        .await
        .map_err(pgerr)?;
        hs.into_iter().collect::<HashSet<String>>()
    } else {
        HashSet::new()
    };

    let items = prepare(&ds.id, &candidates, spec.dedupe, &existing);
    for item in &items {
        datasets::create_item(pool, item).await?;
    }
    Ok(items.len() as u32)
}

/// The `FROM … WHERE …` the count and the selection share, with its bound arguments.
struct Where {
    sql: String,
    args: Vec<String>,
}

fn build_where(ds: &Dataset, spec: &ImportSpec) -> Where {
    let mut sql = String::from("FROM events e");
    if spec.from == ImportSource::Scores {
        // An inner join: an event with no verdict is not a *passing* case, it is an unjudged one,
        // and mining it as a failure would fabricate the signal a regression set carries.
        sql.push_str(" JOIN scores s ON s.event_id = e.id");
    }
    sql.push_str(" WHERE e.project_id = $1 AND e.input IS NOT NULL AND e.input <> ''");
    let mut args = vec![ds.project_id.clone()];

    if spec.is_explicit() {
        let ph: Vec<String> = (0..spec.event_ids.len())
            .map(|i| format!("${}", args.len() + 1 + i))
            .collect();
        sql.push_str(&format!(" AND e.id IN ({})", ph.join(",")));
        args.extend(spec.event_ids.iter().cloned());
        return Where { sql, args };
    }

    if let Some(m) = &spec.filter.model {
        args.push(m.clone());
        sql.push_str(&format!(" AND e.model = ${}", args.len()));
    }
    if let Some(st) = spec.filter.status {
        args.push(st.as_str().to_string());
        sql.push_str(&format!(" AND e.status = ${}", args.len()));
    }
    if let Some(since) = spec.filter.since {
        args.push(fmt_ts(since));
        sql.push_str(&format!(" AND e.ts >= ${}", args.len()));
    }
    if let (Some(p), ImportSource::Scores) = (spec.filter.pass, spec.from) {
        sql.push_str(if p {
            " AND s.pass IS TRUE"
        } else {
            " AND s.pass IS NOT TRUE"
        });
    }
    // Normalised, because `max` is per-rubric: a raw cutoff would mine everything from a 0..1 rubric
    // and nothing from a 0..10 one. Interpolated rather than bound because the shared arg vector is
    // `String`-typed — and guarded on `is_finite`, because a `NaN`/`inf` would render as a token
    // Postgres cannot parse and turn a filter into a syntax error.
    if let (Some(b), ImportSource::Scores) = (spec.filter.below, spec.from) {
        if b.is_finite() {
            sql.push_str(&format!(" AND (s.value / NULLIF(s.max, 0)) < {b}"));
        }
    }
    if is_errors_only(spec.strategy) {
        match spec.from {
            ImportSource::Events => sql.push_str(" AND e.status <> 'success'"),
            ImportSource::Scores => sql.push_str(" AND s.pass IS NOT TRUE"),
        }
    }
    Where { sql, args }
}

async fn select(pool: &PgPool, ds: &Dataset, spec: &ImportSpec) -> Result<Vec<Candidate>> {
    let w = build_where(ds, spec);
    let n = spec.effective_n();

    let (sql, limit) = if spec.strategy == SamplingStrategy::Stratified && !spec.is_explicit() {
        let count_sql = format!(
            "SELECT COUNT(*) FROM (SELECT 1 {} GROUP BY e.model, e.status) g",
            w.sql
        );
        let groups: i64 = bind_all(sqlx::query_scalar(&count_sql), &w.args)
            .fetch_one(pool)
            .await
            .map_err(pgerr)?;
        let quota = stratum_quota(n, groups.max(0) as usize) as i64;
        (
            format!(
                "SELECT id, input, output, tags FROM (\
                   SELECT e.id AS id, MIN(e.input) AS input, MIN(e.output) AS output, \
                          MIN(e.tags) AS tags, \
                          ROW_NUMBER() OVER (PARTITION BY e.model, e.status \
                                             ORDER BY MIN(e.ts) DESC) AS rn \
                   {} GROUP BY e.id, e.model, e.status\
                 ) q WHERE rn <= {quota} LIMIT ${}",
                w.sql,
                w.args.len() + 1
            ),
            (quota * groups.max(1)).min(lighttrack_core::MAX_IMPORT_N as i64),
        )
    } else {
        let order = match spec.strategy {
            SamplingStrategy::Random => "RANDOM()",
            _ => "MIN(e.ts) DESC",
        };
        (
            format!(
                "SELECT e.id, MIN(e.input) AS input, MIN(e.output) AS output, \
                        MIN(e.tags) AS tags {} \
                 GROUP BY e.id ORDER BY {order} LIMIT ${}",
                w.sql,
                w.args.len() + 1
            ),
            n as i64,
        )
    };

    let mut q = sqlx::query(&sql);
    for a in &w.args {
        q = q.bind(a.clone());
    }
    let rows = q.bind(limit).fetch_all(pool).await.map_err(pgerr)?;

    rows.iter()
        .map(|r| {
            let input: String = r.try_get(1).map_err(pgerr)?;
            let output: Option<String> = r.try_get(2).map_err(pgerr)?;
            let tags: Option<String> = r.try_get(3).map_err(pgerr)?;
            Ok(Candidate {
                event_id: r.try_get(0).map_err(pgerr)?,
                input: text_of(&input),
                output: output.as_deref().map(text_of),
                tags: tags
                    .and_then(|t| serde_json::from_str::<Vec<String>>(&t).ok())
                    .unwrap_or_default(),
            })
        })
        .collect()
}

/// Bind a `Vec<String>` of positional arguments onto a scalar query — the count query's half of the
/// shared `WHERE`, which cannot reuse the row query's builder because its output type differs.
fn bind_all<'q>(
    q: sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments>,
    args: &'q [String],
) -> sqlx::query::QueryScalar<'q, sqlx::Postgres, i64, sqlx::postgres::PgArguments> {
    let mut q = q;
    for a in args {
        q = q.bind(a.clone());
    }
    q
}
