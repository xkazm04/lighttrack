//! Tests for the SQLite collective-entries backend, in their own file so `collective.rs` stays
//! inside the module-size budget (the `tests_maintenance` / `tests_metrics` pattern).

use chrono::Utc;
use rusqlite::Connection;

use lighttrack_core::{merge_leaderboard, CollectiveEntry, Coverage, DEFAULT_LOW_CONFIDENCE_CASES};

use super::collective::*;
use crate::codec::fmt_ts;
use crate::CollectiveFilter;

fn conn() -> Connection {
    let c = Connection::open_in_memory().unwrap();
    crate::sqlite::schema::apply(&c).expect("schema");
    c
}

fn entry(contrib: &str, model: &str, q: f64, cases: u32) -> CollectiveEntry {
    CollectiveEntry {
        contributor_id: contrib.into(),
        provider: "anthropic".into(),
        model: model.into(),
        task_type: "qa".into(),
        quality: q,
        pass_rate: q,
        avg_cost_usd: 0.003,
        p50_latency_ms: Some(900),
        p95_latency_ms: Some(2100),
        n_runs: 1,
        n_cases: cases,
        quality_variance: None,
        judge_provider: None,
        rubric_fingerprint: None,
        determinism: None,
        frozen_dataset: Coverage::Unknown,
        significance_tested: Coverage::Unknown,
        received_at: Utc::now(),
    }
}

#[test]
fn upsert_is_idempotent_on_pk() {
    let c = conn();
    upsert(&c, &entry("contrib-a", "haiku", 0.7, 10)).unwrap();
    // Same (contributor, provider, model, task) → updates in place, not a second row.
    upsert(&c, &entry("contrib-a", "haiku", 0.9, 40)).unwrap();
    let all = list(&c).unwrap();
    assert_eq!(all.len(), 1);
    assert!((all[0].quality - 0.9).abs() < 1e-9);
    assert_eq!(all[0].n_cases, 40);
}

#[test]
fn delete_replaces_a_contributors_set() {
    let c = conn();
    upsert(&c, &entry("a", "haiku", 0.7, 10)).unwrap();
    upsert(&c, &entry("a", "sonnet", 0.8, 10)).unwrap();
    upsert(&c, &entry("b", "haiku", 0.6, 10)).unwrap();
    let removed = delete(&c, "a").unwrap();
    assert_eq!(removed, 2);
    let all = list(&c).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].contributor_id, "b");
}

#[test]
fn purge_before_drops_only_expired_rows() {
    let c = conn();
    let mut old = entry("a", "haiku", 0.7, 10);
    old.received_at = Utc::now() - chrono::Duration::days(120);
    upsert(&c, &old).unwrap();
    upsert(&c, &entry("b", "haiku", 0.8, 10)).unwrap();
    let removed = purge_before(&c, Utc::now() - chrono::Duration::days(90)).unwrap();
    assert_eq!(removed, 1);
    let all = list(&c).unwrap();
    assert_eq!(all.len(), 1);
    assert_eq!(all[0].contributor_id, "b");
}

#[test]
fn round_trips_into_a_merged_leaderboard() {
    let c = conn();
    upsert(&c, &entry("a", "sonnet", 0.8, 50)).unwrap();
    upsert(&c, &entry("b", "sonnet", 0.9, 50)).unwrap();
    let rows = merge_leaderboard(&list(&c).unwrap(), DEFAULT_LOW_CONFIDENCE_CASES);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].model, "sonnet");
    assert_eq!(rows[0].n_contributors, 2);
    assert_eq!(rows[0].n_cases, 100);
    assert!((rows[0].quality - 0.85).abs() < 1e-9);
    assert_eq!(rows[0].p50_latency_ms, Some(900));
}

#[test]
fn quality_variance_round_trips() {
    let c = conn();
    let mut e = entry("a", "sonnet", 0.8, 50);
    e.quality_variance = Some(0.0081);
    upsert(&c, &e).unwrap();
    let got = list(&c).unwrap();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].quality_variance, Some(0.0081));
    // A NULL (v1) variance also round-trips as None.
    upsert(&c, &entry("b", "haiku", 0.7, 20)).unwrap();
    let b = list(&c)
        .unwrap()
        .into_iter()
        .find(|r| r.model == "haiku")
        .unwrap();
    assert!(b.quality_variance.is_none());
}

#[test]
fn rigor_round_trips_and_v2_rows_read_back_as_unknown() {
    let c = conn();
    let mut e = entry("a", "sonnet", 0.8, 50);
    e.determinism = Some("exact".into());
    e.frozen_dataset = Coverage::All;
    e.significance_tested = Coverage::Mixed;
    upsert(&c, &e).unwrap();
    let got = list(&c).unwrap();
    assert_eq!(got[0].determinism.as_deref(), Some("exact"));
    assert_eq!(got[0].frozen_dataset, Coverage::All);
    assert_eq!(got[0].significance_tested, Coverage::Mixed);
    // A v1/v2 contribution stores NULLs and reads back as Unknown — no backfill needed.
    upsert(&c, &entry("b", "haiku", 0.7, 20)).unwrap();
    let b = list(&c)
        .unwrap()
        .into_iter()
        .find(|r| r.model == "haiku")
        .unwrap();
    assert!(b.determinism.is_none());
    assert_eq!(b.frozen_dataset, Coverage::Unknown);
    assert_eq!(b.significance_tested, Coverage::Unknown);
}

#[test]
fn replace_makes_the_set_exactly_what_was_sent() {
    let c = conn();
    upsert(&c, &entry("a", "haiku", 0.7, 10)).unwrap();
    upsert(&c, &entry("a", "sonnet", 0.8, 10)).unwrap();
    upsert(&c, &entry("b", "haiku", 0.6, 10)).unwrap();
    let ack = replace(&c, "a", &[entry("a", "opus", 0.9, 30)], None).unwrap();
    assert_eq!((ack.deleted, ack.inserted, ack.purged), (2, 1, 0));
    assert!(ack.atomic, "sqlite replaces in one transaction");
    let mut mine: Vec<_> = list(&c)
        .unwrap()
        .into_iter()
        .filter(|e| e.contributor_id == "a")
        .collect();
    mine.sort_by(|x, y| x.model.cmp(&y.model));
    assert_eq!(mine.len(), 1, "no survivor from the previous set");
    assert_eq!(mine[0].model, "opus");
    // Another contributor's set is untouched.
    assert_eq!(
        list(&c)
            .unwrap()
            .iter()
            .filter(|e| e.contributor_id == "b")
            .count(),
        1
    );
}

/// The property the whole method exists for: a failure part-way through must leave the previous
/// set intact, not a half-replaced one. Proven by running the body inside a transaction that is
/// then rolled back — the same transaction `replace` commits on success.
#[test]
fn a_rolled_back_replace_leaves_the_previous_set_intact() {
    let c = conn();
    upsert(&c, &entry("a", "haiku", 0.7, 10)).unwrap();
    upsert(&c, &entry("a", "sonnet", 0.8, 10)).unwrap();
    {
        let tx = c.unchecked_transaction().unwrap();
        apply_replace(&tx, "a", &[entry("a", "opus", 0.9, 30)], None).unwrap();
        tx.rollback().unwrap();
    }
    let mut models: Vec<String> = list(&c).unwrap().into_iter().map(|e| e.model).collect();
    models.sort();
    assert_eq!(models, vec!["haiku".to_string(), "sonnet".to_string()]);
}

#[test]
fn replace_sweeps_retention_on_the_same_pass() {
    let c = conn();
    let mut old = entry("stale", "haiku", 0.7, 10);
    old.received_at = Utc::now() - chrono::Duration::days(120);
    upsert(&c, &old).unwrap();
    let cutoff = Utc::now() - chrono::Duration::days(90);
    let ack = replace(&c, "a", &[entry("a", "opus", 0.9, 30)], Some(cutoff)).unwrap();
    assert_eq!(ack.purged, 1, "the expired row went with the same pass");
    let all = list(&c).unwrap();
    assert_eq!(
        all.len(),
        1,
        "the freshly written set survived its own sweep"
    );
    assert_eq!(all[0].contributor_id, "a");
}

#[test]
fn latest_receipt_is_per_contributor() {
    let c = conn();
    let mut old = entry("a", "haiku", 0.7, 10);
    old.received_at = Utc::now() - chrono::Duration::days(3);
    upsert(&c, &old).unwrap();
    let newer = entry("a", "sonnet", 0.8, 10);
    upsert(&c, &newer).unwrap();
    // A *later* entry from someone else must not become `a`'s receipt.
    let mut theirs = entry("b", "haiku", 0.6, 10);
    theirs.received_at = Utc::now() + chrono::Duration::days(1);
    upsert(&c, &theirs).unwrap();
    let got = latest_receipt(&c, "a").unwrap().unwrap();
    assert_eq!(fmt_ts(got), fmt_ts(newer.received_at));
    assert!(latest_receipt(&c, "nobody").unwrap().is_none());
}

#[test]
fn list_filtered_applies_the_retention_cutoff() {
    let c = conn();
    let mut old = entry("a", "haiku", 0.7, 10);
    old.received_at = Utc::now() - chrono::Duration::days(120);
    upsert(&c, &old).unwrap();
    upsert(&c, &entry("b", "sonnet", 0.8, 10)).unwrap();
    let cutoff = Utc::now() - chrono::Duration::days(90);
    let kept = list_filtered(
        &c,
        &CollectiveFilter {
            received_after: Some(cutoff),
        },
    )
    .unwrap();
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].contributor_id, "b");
    // An empty filter is the unfiltered list — the fast path must not silently drop rows.
    assert_eq!(
        list_filtered(&c, &CollectiveFilter::default())
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn judge_and_rubric_tags_round_trip() {
    let c = conn();
    let mut e = entry("a", "sonnet", 0.8, 50);
    e.judge_provider = Some("openai".into());
    e.rubric_fingerprint = Some("ab12cd34".into());
    upsert(&c, &e).unwrap();
    let got = list(&c).unwrap();
    assert_eq!(got[0].judge_provider.as_deref(), Some("openai"));
    assert_eq!(got[0].rubric_fingerprint.as_deref(), Some("ab12cd34"));
}
