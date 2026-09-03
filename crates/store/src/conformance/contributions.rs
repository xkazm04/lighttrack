//! `Surface::Contributions`: the contributor-side ledger of what this instance pushed to a hub.
//!
//! Four properties, each of which a feature above the store actually rests on:
//!
//! * a row **round-trips** — including the `ack` JSON and the status vocabulary, because the ledger
//!   exists to say what the hub answered and a lossy ack is the same as no ledger;
//! * the ledger is **append-only and newest-first**, so `lt collective history` shows the last push
//!   at the top rather than the first;
//! * [`Store::latest_contribution`] is scoped to **one hub** — the hash gate compares against the
//!   last push *to that hub*, and answering with another hub's row would skip a push that never
//!   happened;
//! * the keyset page is **stable under append**, which is the whole reason it is a keyset page: the
//!   table is written to while it is being read.

use chrono::{Duration, Utc};
use serde_json::json;

use lighttrack_core::{new_id, ContributionRecord, ContributionStatus};

use crate::codec::encode_event_cursor;
use crate::{Result, Store};

fn record(hub: &str, status: ContributionStatus, at: chrono::DateTime<Utc>) -> ContributionRecord {
    ContributionRecord {
        id: new_id(),
        hub_url_hash: hub.to_string(),
        contributor_id_as_acked: Some("c-hubside".into()),
        schema_version: 3,
        generated_at: at - Duration::seconds(5),
        entries_count: 7,
        projects_included: 2,
        projects_excluded: 1,
        digest_sha256: format!("sha-{}", new_id()),
        ack: json!({ "accepted": 7, "contributor_id": "c-hubside" }),
        status,
        created_at: at,
    }
}

/// One well-formed row, shared with the refusal walk so both halves of the contract probe the same
/// shape.
pub(super) fn sample_contribution() -> ContributionRecord {
    record("h-refusal-probe", ContributionStatus::Sent, Utc::now())
}

pub(super) fn contributions(store: &dyn Store) -> Result<()> {
    // Hub ids unique to this run: the ledger is global (it has no project scope), so a shared
    // database may already hold other rows and every assertion below is written to tolerate that.
    let hub_a = format!("h-{}", new_id());
    let hub_b = format!("h-{}", new_id());
    let now = Utc::now();

    let first = record(
        &hub_a,
        ContributionStatus::Sent,
        now - Duration::minutes(10),
    );
    store.insert_contribution(&first)?;

    let mine = |store: &dyn Store, hub: &str| -> Result<Vec<ContributionRecord>> {
        Ok(store
            .list_contributions(1000, None)?
            .into_iter()
            .filter(|c| c.hub_url_hash == hub)
            .collect())
    };

    let listed = mine(store, &hub_a)?;
    assert_eq!(listed.len(), 1, "the row is listed back");
    let got = &listed[0];
    assert_eq!(got.id, first.id);
    assert_eq!(
        got.digest_sha256, first.digest_sha256,
        "the gate's hash round-trips"
    );
    assert_eq!(got.entries_count, 7);
    assert_eq!(got.projects_included, 2, "the consent envelope round-trips");
    assert_eq!(got.projects_excluded, 1);
    assert_eq!(got.schema_version, 3);
    assert_eq!(got.status, ContributionStatus::Sent);
    assert_eq!(got.contributor_id_as_acked.as_deref(), Some("c-hubside"));
    assert_eq!(
        got.ack["accepted"], 7,
        "the hub's ack is stored verbatim, not summarised: {:?}",
        got.ack
    );
    assert_eq!(
        got.generated_at.timestamp_millis(),
        first.generated_at.timestamp_millis(),
        "when the digest was BUILT is not when it was sent"
    );

    // Every status the vocabulary has must survive the round trip. A `Rejected` row read back as
    // `Sent` would tell an operator a hub holds data it refused.
    let rejected = record(
        &hub_a,
        ContributionStatus::Rejected,
        now - Duration::minutes(5),
    );
    store.insert_contribution(&rejected)?;
    let failed = record(
        &hub_a,
        ContributionStatus::Failed,
        now - Duration::minutes(1),
    );
    store.insert_contribution(&failed)?;

    let listed = mine(store, &hub_a)?;
    assert_eq!(listed.len(), 3, "the ledger appends, it does not replace");
    // Newest first: `lt collective history` shows the most recent push at the top.
    assert_eq!(listed[0].id, failed.id, "newest first: {listed:?}");
    assert_eq!(listed[1].id, rejected.id);
    assert_eq!(listed[2].id, first.id);
    assert_eq!(listed[0].status, ContributionStatus::Failed);
    assert_eq!(listed[1].status, ContributionStatus::Rejected);

    // --- the hash gate's read: newest FOR THIS HUB, never another's ---
    let latest = store
        .latest_contribution(&hub_a)?
        .expect("a hub that has been pushed to has a latest row");
    assert_eq!(
        latest.id, failed.id,
        "latest is the newest row for that hub"
    );

    store.insert_contribution(&record(&hub_b, ContributionStatus::Sent, now))?;
    let latest_a = store
        .latest_contribution(&hub_a)?
        .expect("hub a still has one");
    assert_eq!(
        latest_a.id, failed.id,
        "a push to another hub must not become hub A's latest — that would skip a push to A that \
         never happened"
    );
    let latest_b = store.latest_contribution(&hub_b)?.expect("hub b has one");
    assert_eq!(latest_b.hub_url_hash, hub_b);
    assert!(
        store
            .latest_contribution(&format!("h-{}", new_id()))?
            .is_none(),
        "a hub never pushed to has no latest row — `None`, never another hub's"
    );

    // --- the keyset page ---
    let page = store.list_contributions(2, None)?;
    assert_eq!(page.len(), 2, "limit is honoured");
    let last = page.last().expect("non-empty page");
    let cursor = encode_event_cursor(&crate::codec::fmt_ts(last.created_at), &last.id);
    let next = store.list_contributions(2, Some(&cursor))?;
    assert!(
        !next.iter().any(|c| c.id == last.id),
        "the cursor is exclusive — the page boundary row must not repeat"
    );
    for c in &next {
        assert!(
            (c.created_at, &c.id) < (last.created_at, &last.id),
            "the second page continues strictly below the cursor: {c:?}"
        );
    }

    // `limit: 0` is the default page, not an empty one — a store that read it literally would make
    // an unparameterised `GET /v1/collective/contributions` answer "nothing has been contributed".
    assert!(
        !store.list_contributions(0, None)?.is_empty(),
        "limit 0 means the default page size, not zero rows"
    );
    Ok(())
}
