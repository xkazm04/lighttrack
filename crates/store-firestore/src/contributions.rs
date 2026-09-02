//! `collective_contributions` collection — the contributor-side ledger (M22).
//!
//! Append-only: the document id is the record's own id, so a write is never an overwrite of
//! somebody else's row, and there is no delete path at all.
//!
//! **The paging is done client-side, on purpose.** Firestore's `orderBy` on `created_at` plus a
//! `WHERE hub_url_hash = …` equality needs a *composite index* somebody has to declare, and the
//! same is true of the keyset predicate the SQL backends express as `(created_at, id) <`. The hash
//! gate must not fail on a fresh project because nobody ran `gcloud firestore indexes create`, so
//! this module reads with only the automatic single-field indexes and orders in memory. The ledger
//! is bounded by how often an instance contributes (at most once per schedule interval per hub),
//! which is orders of magnitude smaller than `events` — the surface where that trade would not be
//! acceptable.

use serde_json::{json, Value};

use lighttrack_core::{ContributionRecord, ContributionStatus};
use lighttrack_store::codec::{decode_event_cursor, json_or_null, val_or_null};
use lighttrack_store::collective::contributions_limit;
use lighttrack_store::{Result, StoreError};

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "collective_contributions";

fn fields(c: &ContributionRecord) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(c.id));
    m.insert("hub_url_hash".into(), json!(c.hub_url_hash));
    m.insert(
        "contributor_id_as_acked".into(),
        json!(c.contributor_id_as_acked),
    );
    m.insert("schema_version".into(), json!(c.schema_version as i64));
    m.insert("generated_at".into(), json!(fmt_ts(c.generated_at)));
    m.insert("entries_count".into(), json!(c.entries_count as i64));
    m.insert(
        "projects_included".into(),
        json!(c.projects_included as i64),
    );
    m.insert(
        "projects_excluded".into(),
        json!(c.projects_excluded as i64),
    );
    m.insert("digest_sha256".into(), json!(c.digest_sha256));
    m.insert("ack".into(), json!(json_or_null(&c.ack)?));
    m.insert("status".into(), json!(c.status.as_str()));
    m.insert("created_at".into(), json!(fmt_ts(c.created_at)));
    Ok(m)
}

fn from_fields(m: &Fields) -> Result<ContributionRecord> {
    let raw_status = freq(m, "status")?;
    // Surfaced, not coerced: the reassuring default would be `Sent` — "the hub has your data".
    let status = ContributionStatus::from_wire(&raw_status).ok_or_else(|| {
        StoreError::Other(format!(
            "stored value {raw_status:?} in field `status` is outside the contribution vocabulary"
        ))
    })?;
    Ok(ContributionRecord {
        id: freq(m, "id")?,
        hub_url_hash: freq(m, "hub_url_hash")?,
        contributor_id_as_acked: fstr(m, "contributor_id_as_acked"),
        schema_version: fi64(m, "schema_version").unwrap_or(0) as u32,
        generated_at: parse_ts(&freq(m, "generated_at")?)?,
        entries_count: fi64(m, "entries_count").unwrap_or(0) as u32,
        projects_included: fi64(m, "projects_included").unwrap_or(0) as u32,
        projects_excluded: fi64(m, "projects_excluded").unwrap_or(0) as u32,
        digest_sha256: freq(m, "digest_sha256")?,
        ack: val_or_null(fstr(m, "ack"))?,
        status,
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}

pub(crate) fn insert(rest: &Rest, c: &ContributionRecord) -> Result<()> {
    rest.put_doc(COLL, &c.id, &fields(c)?)
}

/// One hub's rows, newest first. An equality filter alone rides the automatic single-field index;
/// the ordering is in memory — see the module docs.
fn of_hub(rest: &Rest, hub_url_hash: &str) -> Result<Vec<ContributionRecord>> {
    let filters: Vec<(&str, &str, Value)> = vec![("hub_url_hash", "EQUAL", json!(hub_url_hash))];
    let mut out: Vec<ContributionRecord> = rest
        .query(COLL, &filters, None, None)?
        .iter()
        .map(from_fields)
        .collect::<Result<Vec<_>>>()?;
    sort_newest_first(&mut out);
    Ok(out)
}

pub(crate) fn list(
    rest: &Rest,
    limit: usize,
    cursor: Option<&str>,
) -> Result<Vec<ContributionRecord>> {
    let n = contributions_limit(limit);
    let after = match cursor.map(decode_event_cursor) {
        None => None,
        Some(Some(pair)) => Some(pair),
        // Same refusal as the SQL backends: serving page one for a cursor we did not mint looks
        // exactly like "the ledger ended here".
        Some(None) => {
            return Err(StoreError::Other(
                "bad contributions cursor: not a value this API minted".into(),
            ))
        }
    };
    let mut all: Vec<ContributionRecord> = rest
        .query(COLL, &[], None, None)?
        .iter()
        .map(from_fields)
        .collect::<Result<Vec<_>>>()?;
    sort_newest_first(&mut all);
    if let Some((ts, id)) = after {
        all.retain(|c| (fmt_ts(c.created_at), c.id.clone()) < (ts.clone(), id.clone()));
    }
    all.truncate(n);
    Ok(all)
}

pub(crate) fn latest(rest: &Rest, hub_url_hash: &str) -> Result<Option<ContributionRecord>> {
    Ok(of_hub(rest, hub_url_hash)?.into_iter().next())
}

/// `(created_at, id)` descending — the same total order the SQL backends' `ORDER BY` produces, so a
/// cursor minted on one backend pages identically on another after a migration.
fn sort_newest_first(v: &mut [ContributionRecord]) {
    v.sort_by(|a, b| (fmt_ts(b.created_at), &b.id).cmp(&(fmt_ts(a.created_at), &a.id)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};
    use serde_json::json;

    fn rec(id: &str, secs_ago: i64) -> ContributionRecord {
        ContributionRecord {
            id: id.into(),
            hub_url_hash: "h-abc".into(),
            contributor_id_as_acked: None,
            schema_version: 3,
            generated_at: Utc::now(),
            entries_count: 1,
            projects_included: 1,
            projects_excluded: 0,
            digest_sha256: "deadbeef".into(),
            ack: json!({ "accepted": 1 }),
            status: ContributionStatus::Sent,
            created_at: Utc::now() - Duration::seconds(secs_ago),
        }
    }

    /// The in-memory order has to be the same total order the SQL backends' `ORDER BY` gives, or a
    /// cursor minted before a migration pages wrongly after it.
    #[test]
    fn the_in_memory_order_is_newest_first_and_breaks_ties_on_id() {
        let mut v = vec![rec("a", 10), rec("c", 0), rec("b", 5)];
        sort_newest_first(&mut v);
        assert_eq!(
            v.iter().map(|c| c.id.as_str()).collect::<Vec<_>>(),
            ["c", "b", "a"]
        );

        let at = Utc::now();
        let mut tied = vec![rec("a", 0), rec("b", 0)];
        for c in tied.iter_mut() {
            c.created_at = at;
        }
        sort_newest_first(&mut tied);
        assert_eq!(
            tied[0].id, "b",
            "ties break on id DESC, like the SQL backends"
        );
    }

    /// Every field the ledger exists to report must survive the field-map round trip — an ack read
    /// back as null would leave the record saying a push happened but not what came of it.
    #[test]
    fn a_record_round_trips_through_the_field_map() {
        let c = rec("x", 3);
        let back = from_fields(&fields(&c).expect("encode")).expect("decode");
        assert_eq!(back.id, c.id);
        assert_eq!(back.hub_url_hash, c.hub_url_hash);
        assert_eq!(back.digest_sha256, c.digest_sha256);
        assert_eq!(back.schema_version, 3);
        assert_eq!(back.entries_count, 1);
        assert_eq!(back.status, ContributionStatus::Sent);
        assert_eq!(back.ack["accepted"], 1);
        assert_eq!(
            fmt_ts(back.created_at),
            fmt_ts(c.created_at),
            "the fixed-width timestamp is what the ordering rests on"
        );
    }

    /// A status this build does not know must fail the read rather than become `Sent`.
    #[test]
    fn an_unknown_status_is_surfaced_not_coerced() {
        let mut m = fields(&rec("x", 0)).expect("encode");
        m.insert("status".into(), json!("from_a_newer_release"));
        let err = from_fields(&m).expect_err("must not coerce");
        assert!(err.to_string().contains("from_a_newer_release"), "{err}");
    }
}
