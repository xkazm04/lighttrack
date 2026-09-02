//! The redaction posture report — the Postgres port of `sqlite/redaction.rs`.
//!
//! Backend parity is a correctness property here more than anywhere: "is this database scrubbed"
//! answered `Unsupported` on the backend carrying production traffic would leave the question
//! unanswerable exactly where it matters.

use sqlx::postgres::PgPool;
use sqlx::Row;

use lighttrack_core::RedactionStamp;

use crate::util::{fmt_ts, pgerr};
use lighttrack_store::{RedactionPostureRow, Result};

/// Events received at or after `since`, grouped by the redaction stamp they carry, most first.
///
/// Windowed on `COALESCE(received_at, ts)` for the same reason as every other accounting read: the
/// client owns `ts` and could otherwise backdate its rows out of the operator's posture report.
///
/// The `::jsonb` cast carries the caveat documented on `events/cols.rs::USAGE_COLS` — `metadata` is
/// TEXT and an invalid value raises. `NULLIF` covers the empty string; everything this backend
/// writes is serde-serialized JSON or NULL. Unlike the admission query this one is a report, so a
/// malformed row fails one request rather than stopping ingest.
pub(crate) async fn posture(
    pool: &PgPool,
    project: Option<&str>,
    since: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<RedactionPostureRow>> {
    let (proj, bind) = match project {
        Some(p) => ("project_id = $2 AND ", Some(p.to_string())),
        None => ("", None),
    };
    let sql = format!(
        "SELECT ((NULLIF(metadata,'')::jsonb)->'redaction')::text AS stamp, COUNT(*) AS n \
         FROM events WHERE {proj}COALESCE(received_at, ts) >= $1 \
         GROUP BY stamp ORDER BY n DESC"
    );
    let mut q = sqlx::query(&sql).bind(fmt_ts(since));
    if let Some(p) = bind {
        q = q.bind(p);
    }
    let rows = q.fetch_all(pool).await.map_err(pgerr)?;
    rows.iter()
        .map(|row| {
            let stamp: Option<String> = row.try_get(0).map_err(pgerr)?;
            let n: i64 = row.try_get(1).map_err(pgerr)?;
            Ok(RedactionPostureRow {
                // An unreadable stamp degrades into the "we do not know" bucket rather than
                // erroring the report — which is the honest reading of it.
                stamp: stamp
                    .as_deref()
                    .and_then(|j| serde_json::from_str::<RedactionStamp>(j).ok()),
                events: n.max(0) as u64,
            })
        })
        .collect()
}
