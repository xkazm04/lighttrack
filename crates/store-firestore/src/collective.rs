//! `collective_entries` collection — the opt-in shared leaderboard's digest rows.
//!
//! Firestore has no primary key beyond the document id, so the key that makes a re-contribution an
//! upsert rather than a second vote — `(contributor_id, provider, model, task_type)` — *is* the
//! document id (see [`doc_id`]). Timestamps stay fixed-width `RFC3339(Nanos, Z)` strings, so the
//! retention range filter is a correct chronological one and matches SQLite/Postgres exactly.
//!
//! **Atomicity is bounded, and said so.** A `:commit` applies atomically up to
//! [`Rest::MAX_BATCH`] writes; a replacement larger than that has to be chunked and is therefore no
//! longer one unit. [`replace`] reports which of the two happened in `ReplaceAck::atomic` instead of
//! claiming a guarantee it cannot keep — a hub on Firestore that wants the guarantee should keep a
//! contributor's set under the batch limit (the hub's own `MAX_ENTRIES` cap is the knob).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{CollectiveEntry, Coverage};
use lighttrack_store::{CollectiveFilter, ReplaceAck, Result};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "collective_entries";

/// Escape one key component so the joined document id is unambiguous and legal.
///
/// `/` is illegal in a Firestore document id and `_` is the separator, so both are percent-encoded
/// (and `%` itself first, or the encoding would not be reversible). Without this, a contributor
/// named `a_b` and a provider `c` would collide with contributor `a` and provider `b_c` — two
/// different sources sharing one row.
fn esc(s: &str) -> String {
    s.replace('%', "%25")
        .replace('/', "%2F")
        .replace('_', "%5F")
}

/// The document id carrying the entry's primary key.
fn doc_id(contributor: &str, provider: &str, model: &str, task_type: &str) -> String {
    format!(
        "ce_{}_{}_{}_{}",
        esc(contributor),
        esc(provider),
        esc(model),
        esc(task_type)
    )
}

fn entry_doc_id(e: &CollectiveEntry) -> String {
    doc_id(&e.contributor_id, &e.provider, &e.model, &e.task_type)
}

fn fields(e: &CollectiveEntry) -> Fields {
    let mut m = Fields::new();
    m.insert("contributor_id".into(), json!(e.contributor_id));
    m.insert("provider".into(), json!(e.provider));
    m.insert("model".into(), json!(e.model));
    m.insert("task_type".into(), json!(e.task_type));
    m.insert("quality".into(), json!(e.quality));
    m.insert("pass_rate".into(), json!(e.pass_rate));
    m.insert("avg_cost_usd".into(), json!(e.avg_cost_usd));
    m.insert(
        "p50_latency_ms".into(),
        json!(e.p50_latency_ms.map(|v| v as i64)),
    );
    m.insert(
        "p95_latency_ms".into(),
        json!(e.p95_latency_ms.map(|v| v as i64)),
    );
    m.insert("n_runs".into(), json!(e.n_runs as i64));
    m.insert("n_cases".into(), json!(e.n_cases as i64));
    m.insert("quality_variance".into(), json!(e.quality_variance));
    m.insert("judge_provider".into(), json!(e.judge_provider));
    m.insert("rubric_fingerprint".into(), json!(e.rubric_fingerprint));
    m.insert("determinism".into(), json!(e.determinism));
    m.insert("frozen_dataset".into(), json!(e.frozen_dataset.to_tag()));
    m.insert(
        "significance_tested".into(),
        json!(e.significance_tested.to_tag()),
    );
    m.insert("received_at".into(), json!(fmt_ts(e.received_at)));
    m
}

fn cov(m: &Fields, k: &str) -> Coverage {
    fstr(m, k)
        .as_deref()
        .map(Coverage::from_tag)
        .unwrap_or(Coverage::Unknown)
}

fn from_fields(m: &Fields) -> Result<CollectiveEntry> {
    Ok(CollectiveEntry {
        contributor_id: freq(m, "contributor_id")?,
        provider: freq(m, "provider")?,
        model: freq(m, "model")?,
        task_type: freq(m, "task_type")?,
        quality: ff64(m, "quality").unwrap_or(0.0),
        pass_rate: ff64(m, "pass_rate").unwrap_or(0.0),
        avg_cost_usd: ff64(m, "avg_cost_usd").unwrap_or(0.0),
        p50_latency_ms: fi64(m, "p50_latency_ms").map(|v| v as u64),
        p95_latency_ms: fi64(m, "p95_latency_ms").map(|v| v as u64),
        n_runs: fi64(m, "n_runs").unwrap_or(0) as u32,
        n_cases: fi64(m, "n_cases").unwrap_or(0) as u32,
        quality_variance: ff64(m, "quality_variance"),
        judge_provider: fstr(m, "judge_provider"),
        rubric_fingerprint: fstr(m, "rubric_fingerprint"),
        determinism: fstr(m, "determinism"),
        // An absent tag reads back as `Unknown`, exactly as a SQL NULL does.
        frozen_dataset: cov(m, "frozen_dataset"),
        significance_tested: cov(m, "significance_tested"),
        received_at: parse_ts(&freq(m, "received_at")?)?,
    })
}

pub(crate) fn upsert(rest: &Rest, e: &CollectiveEntry) -> Result<()> {
    rest.put_doc(COLL, &entry_doc_id(e), &fields(e))
}

pub(crate) fn list(rest: &Rest) -> Result<Vec<CollectiveEntry>> {
    rest.query(COLL, &[], None, None)?
        .iter()
        .map(from_fields)
        .collect()
}

/// Retention-narrowed read. A single range filter needs only the automatic single-field index, so
/// this works on a fresh project without a composite index being declared first.
pub(crate) fn list_filtered(rest: &Rest, f: &CollectiveFilter) -> Result<Vec<CollectiveEntry>> {
    let Some(after) = f.received_after else {
        return list(rest);
    };
    let filters: Vec<(&str, &str, Value)> =
        vec![("received_at", "GREATER_THAN_OR_EQUAL", json!(fmt_ts(after)))];
    rest.query(COLL, &filters, None, None)?
        .iter()
        .map(from_fields)
        .collect()
}

fn of_contributor(rest: &Rest, contributor_id: &str) -> Result<Vec<CollectiveEntry>> {
    let filters: Vec<(&str, &str, Value)> =
        vec![("contributor_id", "EQUAL", json!(contributor_id))];
    rest.query(COLL, &filters, None, None)?
        .iter()
        .map(from_fields)
        .collect()
}

/// Newest receipt for one contributor. The max is taken client-side over that contributor's own
/// rows rather than with `orderBy` + `limit 1`: an equality filter combined with an order on a
/// *different* field needs a composite index, and a hub should not fail its rate-limit check
/// because nobody declared one.
pub(crate) fn latest_receipt(rest: &Rest, contributor_id: &str) -> Result<Option<DateTime<Utc>>> {
    Ok(of_contributor(rest, contributor_id)?
        .into_iter()
        .map(|e| e.received_at)
        .max())
}

pub(crate) fn delete(rest: &Rest, contributor_id: &str) -> Result<u64> {
    let ids: Vec<String> = of_contributor(rest, contributor_id)?
        .iter()
        .map(entry_doc_id)
        .collect();
    delete_ids(rest, &ids)?;
    Ok(ids.len() as u64)
}

pub(crate) fn purge_before(rest: &Rest, cutoff: DateTime<Utc>) -> Result<u64> {
    let filters: Vec<(&str, &str, Value)> =
        vec![("received_at", "LESS_THAN", json!(fmt_ts(cutoff)))];
    let ids: Vec<String> = rest
        .query(COLL, &filters, None, None)?
        .iter()
        .map(from_fields)
        .collect::<Result<Vec<_>>>()?
        .iter()
        .map(entry_doc_id)
        .collect();
    delete_ids(rest, &ids)?;
    Ok(ids.len() as u64)
}

fn delete_ids(rest: &Rest, ids: &[String]) -> Result<()> {
    for chunk in ids.chunks(Rest::MAX_BATCH) {
        let writes: Vec<Value> = chunk.iter().map(|id| rest.write_delete(COLL, id)).collect();
        rest.commit_batch(&writes)?;
    }
    Ok(())
}

/// Replace a contributor's whole set, plus the optional retention sweep.
///
/// The deletes of the previous set and the writes of the new one go into **one** `:commit` when they
/// fit in [`Rest::MAX_BATCH`] — atomic, like the SQL backends' transaction. When they do not, the
/// commit is chunked and `ReplaceAck::atomic` comes back `false`, because a crash between chunks
/// really can leave a partially-replaced set and pretending otherwise is worse than the gap.
///
/// The sweep is always its own commit: it spans other contributors, so folding it into the
/// replacement's batch would make an unrelated retention failure roll back a good contribution.
pub(crate) fn replace(
    rest: &Rest,
    contributor_id: &str,
    entries: &[CollectiveEntry],
    purge_before_cutoff: Option<DateTime<Utc>>,
) -> Result<ReplaceAck> {
    let previous: Vec<String> = of_contributor(rest, contributor_id)?
        .iter()
        .map(entry_doc_id)
        .collect();
    let incoming: Vec<String> = entries.iter().map(entry_doc_id).collect();
    // Only the buckets that are *gone* need deleting; the rest are overwritten in place, which both
    // halves the write count and keeps a re-push of the same set from momentarily emptying the row.
    let stale: Vec<&String> = previous
        .iter()
        .filter(|id| !incoming.contains(id))
        .collect();

    let mut writes: Vec<Value> = stale.iter().map(|id| rest.write_delete(COLL, id)).collect();
    for (e, id) in entries.iter().zip(incoming.iter()) {
        writes.push(rest.write_update(COLL, id, &fields(e)));
    }
    let atomic = writes.len() <= Rest::MAX_BATCH;
    for chunk in writes.chunks(Rest::MAX_BATCH) {
        rest.commit_batch(chunk)?;
    }

    let purged = match purge_before_cutoff {
        Some(c) => purge_before(rest, c)?,
        None => 0,
    };
    Ok(ReplaceAck {
        deleted: previous.len() as u64,
        inserted: entries.len() as u64,
        purged,
        atomic,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The document id *is* the primary key, so two different keys must never produce one id. The
    /// `_` separator makes that a real risk for components that contain `_`.
    #[test]
    fn the_doc_id_separates_components_unambiguously() {
        assert_ne!(doc_id("a_b", "c", "m", "t"), doc_id("a", "b_c", "m", "t"));
        assert_ne!(doc_id("a", "b", "m", "t"), doc_id("a", "b", "m", "t2"));
    }

    /// A model identity carries `/` in several provider namespaces, and `/` is illegal in a
    /// Firestore document id — an unescaped one would silently address a subcollection.
    #[test]
    fn a_slash_in_a_model_never_reaches_the_doc_id() {
        let id = doc_id("c1", "openrouter", "anthropic/claude-haiku-4-5", "qa");
        assert!(!id.contains('/'), "{id}");
        assert!(id.starts_with("ce_"), "{id}");
    }

    /// Escaping has to be reversible in principle — a `%` that is not itself escaped first makes
    /// `%2F` and a literal `/` indistinguishable.
    #[test]
    fn percent_is_escaped_before_the_characters_that_encode_to_it() {
        assert_ne!(esc("%2F"), esc("/"));
    }
}
