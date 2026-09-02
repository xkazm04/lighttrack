//! Forward fill on Postgres: price the rows a missing rate left at `cost_usd IS NULL`.
//!
//! Same contract as the SQLite reference (`store::sqlite::price_fill`): only NULL-cost rows for one
//! `(provider, model)`, priced through the caller's [`PriceBook`] so tiers and batch/flex lanes
//! resolve exactly as they do at ingest, each write stamped `metadata.cost_source = "book_fill"` and
//! `metadata.priced_at`. A row already costed is never touched.
//!
//! Paged, one transaction per page, and the `UPDATE` repeats `cost_usd IS NULL` in its own `WHERE`:
//! between the select and the write another connection may have ingested or filled the same row,
//! and a fill must lose that race rather than overwrite a cost somebody else already established.

use serde_json::Value;
use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_core::{PricingMode, TokenUsage};
use lighttrack_store::pricing::PriceFill;
use lighttrack_store::Result;

use crate::util::pgerr;

struct Candidate {
    id: String,
    usage: TokenUsage,
    tags: Vec<String>,
    metadata: Value,
}

pub(crate) async fn fill(pool: &PgPool, f: &PriceFill<'_>) -> Result<u64> {
    let page = f.page.max(1);
    let mut filled: u64 = 0;
    loop {
        let batch = candidates(pool, f, page).await?;
        if batch.is_empty() {
            break;
        }
        let n = batch.len();
        let wrote = write_page(pool, f, batch).await?;
        filled += wrote;
        // A page that priced nothing would loop forever on the same rows: the book still cannot
        // price this key, and leaving honest NULLs alone is the right outcome.
        if wrote == 0 || n < page {
            break;
        }
    }
    Ok(filled)
}

async fn candidates(pool: &PgPool, f: &PriceFill<'_>, page: usize) -> Result<Vec<Candidate>> {
    let rows = sqlx::query(
        "SELECT id, input_tokens, output_tokens, cached_input_tokens, reasoning_tokens, \
                tags, metadata \
           FROM events \
          WHERE cost_usd IS NULL AND provider = $1 AND model = $2 \
          LIMIT $3",
    )
    .bind(f.provider.to_string())
    .bind(f.model.to_string())
    .bind(page as i64)
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;

    rows.iter()
        .map(|r| {
            let n = |i: usize| -> Result<u64> {
                Ok(r.try_get::<i64, _>(i).map_err(pgerr)?.max(0) as u64)
            };
            let opt = |i: usize| -> Result<Option<u64>> {
                Ok(r.try_get::<Option<i64>, _>(i)
                    .map_err(pgerr)?
                    .map(|v| v.max(0) as u64))
            };
            Ok(Candidate {
                id: r.try_get(0).map_err(pgerr)?,
                usage: TokenUsage {
                    input: n(1)?,
                    output: n(2)?,
                    cached_input: opt(3)?,
                    reasoning: opt(4)?,
                },
                tags: json_field(r.try_get(5).map_err(pgerr)?).unwrap_or_default(),
                metadata: json_field(r.try_get(6).map_err(pgerr)?).unwrap_or(Value::Null),
            })
        })
        .collect()
}

/// `tags` and `metadata` are TEXT here and a malformed value must skew one row's lane, never fail
/// the whole fill — the same reasoning as the `NULLIF(...)::jsonb` guard in `events/cols.rs`.
fn json_field<T: serde::de::DeserializeOwned>(raw: Option<String>) -> Option<T> {
    raw.and_then(|s| serde_json::from_str(&s).ok())
}

async fn write_page(pool: &PgPool, f: &PriceFill<'_>, batch: Vec<Candidate>) -> Result<u64> {
    let mut tx = pool.begin().await.map_err(pgerr)?;
    let mut wrote = 0u64;
    for mut c in batch {
        let mode = PricingMode::from_hints(&c.metadata, &c.tags);
        let Some(cost) = f.cost_for(&c.usage, mode) else {
            continue;
        };
        f.stamp(&mut c.metadata);
        let done = sqlx::query(
            "UPDATE events SET cost_usd = $1, metadata = $2 WHERE id = $3 AND cost_usd IS NULL",
        )
        .bind(cost)
        .bind(c.metadata.to_string())
        .bind(c.id)
        .execute(&mut *tx)
        .await
        .map_err(pgerr)?;
        wrote += done.rows_affected();
    }
    tx.commit().await.map_err(pgerr)?;
    Ok(wrote)
}
