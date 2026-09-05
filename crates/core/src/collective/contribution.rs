//! The **contributor-side** ledger: what this instance sent, when, to which hub, and what came back.
//!
//! Contribution used to be a two-hop CLI push (`GET /digest` here → `POST /ingest` there) whose ack
//! was printed to a terminal and discarded. Nothing at rest said a push had ever happened, which
//! made three ordinary questions unanswerable: *has this digest already gone out* (so a no-op
//! re-push stops tripping a hub's `min_interval` 429), *which hubs hold our data* (so a withdrawal
//! can cover all of them), and *what did the hub actually accept*.
//!
//! ## What is deliberately NOT stored
//!
//! The digest body. A [`ContributionRecord`] keeps its **hash** and its counts, so the ledger can
//! answer "did this change" and "how much left the building" without itself becoming a second copy
//! of every model number this instance has ever measured. The hub already knows the body; the
//! ledger does not need to.
//!
//! The hub URL is stored **hashed** ([`hub_url_hash`]) for the same reason the contributor id is:
//! a ledger an operator shows someone (or an MCP tool drops into an agent transcript) should not
//! be the place a private hub's address leaks from. The hash is stable, so `latest_contribution`
//! is a keyed read and `withdraw --all` can still group by hub — the CLI passes the plain URL and
//! the API hashes it, exactly as the contributor id works.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::types::CollectiveDigest;

/// How a contribution attempt ended. Three outcomes, because the operator's next move differs for
/// each: `Sent` is done, `Rejected` means the hub understood and declined (fix the digest or the
/// credential), `Failed` means the push never landed (retry).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContributionStatus {
    /// The hub accepted it (2xx).
    Sent,
    /// The hub answered, and refused (4xx/5xx) — including the `429` a `min_interval` produces.
    Rejected,
    /// The push never got an answer: DNS, TLS, connect, timeout.
    Failed,
}

impl ContributionStatus {
    pub const ALL: [ContributionStatus; 3] = [
        ContributionStatus::Sent,
        ContributionStatus::Rejected,
        ContributionStatus::Failed,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ContributionStatus::Sent => "sent",
            ContributionStatus::Rejected => "rejected",
            ContributionStatus::Failed => "failed",
        }
    }

    /// Parse a stored/wire literal, or `None` outside the vocabulary.
    pub fn from_wire(s: &str) -> Option<ContributionStatus> {
        ContributionStatus::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
    }

    /// Only a `Sent` row means the hub holds this contributor's data — which is what
    /// `withdraw --all` iterates and what the hash-gate compares against.
    pub fn landed(self) -> bool {
        self == ContributionStatus::Sent
    }
}

/// One recorded contribution attempt.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct ContributionRecord {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Opaque, stable hash of the hub's base URL — see the module docs for why it is not the URL.
    pub hub_url_hash: String,
    /// The contributor id the **hub** said it filed this under. `None` when the hub's ack carried
    /// none (an older hub, or a refusal). Worth keeping apart from this instance's own preview id:
    /// a hub derives identity from the presented key, so the two can legitimately differ, and an
    /// operator chasing "why are our rows not merging" needs the hub's answer, not ours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub contributor_id_as_acked: Option<String>,
    pub schema_version: u32,
    /// When the digest this row describes was *built* (not when it was sent).
    pub generated_at: DateTime<Utc>,
    pub entries_count: u32,
    /// The consent envelope, at rest. `GET /digest` recomputes these per call, so before this row
    /// existed the record of *which* projects consented to a push lived only on the wire.
    pub projects_included: u32,
    pub projects_excluded: u32,
    /// [`digest_sha256`] of the digest body. The gate a repeat push is skipped by.
    pub digest_sha256: String,
    /// The hub's answer, verbatim (or a structured error for [`ContributionStatus::Failed`]).
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub ack: Value,
    pub status: ContributionStatus,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

/// Hub URLs are operator-typed: a trailing slash is not a different hub.
pub fn normalize_hub_url(hub: &str) -> &str {
    hub.trim().trim_end_matches('/')
}

/// Opaque, stable id for a hub: `h-` + the first 12 hex chars of SHA-256 over its normalized URL.
/// Same construction as the contributor id, and for the same reason — a stable key that is not the
/// secret it is derived from.
pub fn hub_url_hash(hub: &str) -> String {
    format!("h-{}", short_sha256(normalize_hub_url(hub).as_bytes()))
}

/// The content hash a repeat push is gated on.
///
/// **Not** a hash of the serialized [`CollectiveDigest`]: `generated_at` moves on every build, so
/// hashing the whole struct would make every digest "changed" and defeat the gate entirely. What is
/// hashed is what a hub would actually *store* — the schema version, the k-anonymity floor, the
/// consent envelope, and every entry's numbers — canonicalized field by field (entries sorted by
/// their key, floats at fixed precision) so the value does not depend on map ordering or on a
/// float's shortest-repr drifting between releases.
pub fn digest_sha256(d: &CollectiveDigest) -> String {
    let mut h = Sha256::new();
    h.update(
        format!(
            "v{}|k{}|in{}|ex{}\n",
            d.schema_version, d.min_cases, d.projects_included, d.projects_excluded
        )
        .as_bytes(),
    );
    let mut keys: Vec<usize> = (0..d.entries.len()).collect();
    keys.sort_by(|&a, &b| {
        let (x, y) = (&d.entries[a], &d.entries[b]);
        (&x.provider, &x.model, &x.task_type).cmp(&(&y.provider, &y.model, &y.task_type))
    });
    for i in keys {
        let e = &d.entries[i];
        h.update(
            format!(
                "{}|{}|{}|{:.9}|{:.9}|{:.9}|{:?}|{:?}|{}|{}|{}|{}|{}|{}|{}|{}\n",
                e.provider,
                e.model,
                e.task_type,
                e.quality,
                e.pass_rate,
                e.avg_cost_usd,
                e.p50_latency_ms,
                e.p95_latency_ms,
                e.n_runs,
                e.n_cases,
                e.quality_variance
                    .map(|v| format!("{v:.9}"))
                    .unwrap_or_default(),
                e.judge_provider.as_deref().unwrap_or(""),
                e.rubric_fingerprint.as_deref().unwrap_or(""),
                e.determinism.as_deref().unwrap_or(""),
                e.frozen_dataset.as_str(),
                e.significance_tested.as_str(),
            )
            .as_bytes(),
        );
    }
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

fn short_sha256(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    h.finalize()
        .iter()
        .take(6)
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::collective::{Coverage, ModelDigestEntry};

    fn entry(model: &str, quality: f64) -> ModelDigestEntry {
        ModelDigestEntry {
            provider: "openai".into(),
            model: model.into(),
            task_type: "qa".into(),
            quality,
            pass_rate: 0.9,
            avg_cost_usd: 0.001,
            p50_latency_ms: Some(120),
            p95_latency_ms: None,
            n_runs: 3,
            n_cases: 30,
            quality_variance: Some(0.01),
            judge_provider: Some("anthropic".into()),
            rubric_fingerprint: Some("abc123".into()),
            determinism: Some("exact".into()),
            frozen_dataset: Coverage::All,
            significance_tested: Coverage::Unknown,
        }
    }

    fn digest(entries: Vec<ModelDigestEntry>) -> CollectiveDigest {
        CollectiveDigest {
            schema_version: 3,
            contributor_id: "c-abc".into(),
            generated_at: Utc::now(),
            min_cases: 5,
            projects_included: 2,
            projects_excluded: 1,
            buckets_withheld: 0,
            entries,
        }
    }

    /// The gate's whole point: two builds of the same data must hash the same, even though the
    /// second was built a moment later. Hashing the serialized struct would fail here, because
    /// `generated_at` moves.
    #[test]
    fn the_hash_ignores_when_the_digest_was_built() {
        let a = digest(vec![entry("gpt-4o", 0.8)]);
        let mut b = digest(vec![entry("gpt-4o", 0.8)]);
        b.generated_at = Utc::now() + chrono::Duration::days(3);
        b.contributor_id = "c-somebody-else".into();
        assert_eq!(digest_sha256(&a), digest_sha256(&b));
    }

    /// …and any change a hub would actually store must move it, or a stale board never refreshes.
    #[test]
    fn every_stored_field_moves_the_hash() {
        let base = digest(vec![entry("gpt-4o", 0.8)]);
        let h = digest_sha256(&base);

        let mut changed = digest(vec![entry("gpt-4o", 0.81)]);
        assert_ne!(digest_sha256(&changed), h, "a quality change must show");

        changed = digest(vec![entry("gpt-4o-mini", 0.8)]);
        assert_ne!(digest_sha256(&changed), h, "a model change must show");

        changed = digest(vec![entry("gpt-4o", 0.8), entry("o3", 0.7)]);
        assert_ne!(digest_sha256(&changed), h, "a new bucket must show");

        changed = digest(vec![entry("gpt-4o", 0.8)]);
        changed.min_cases = 10;
        assert_ne!(
            digest_sha256(&changed),
            h,
            "a different floor is a different digest"
        );

        changed = digest(vec![entry("gpt-4o", 0.8)]);
        changed.projects_excluded = 0;
        assert_ne!(
            digest_sha256(&changed),
            h,
            "the consent envelope is part of it"
        );
    }

    /// Entry order is an artefact of how the digest was assembled, not of its content.
    #[test]
    fn entry_order_does_not_change_the_hash() {
        let a = digest(vec![entry("gpt-4o", 0.8), entry("o3", 0.7)]);
        let b = digest(vec![entry("o3", 0.7), entry("gpt-4o", 0.8)]);
        assert_eq!(digest_sha256(&a), digest_sha256(&b));
    }

    #[test]
    fn hub_hash_is_opaque_stable_and_slash_insensitive() {
        let h = hub_url_hash("https://hub.example");
        assert_eq!(h, hub_url_hash("https://hub.example/"));
        assert_eq!(h, hub_url_hash("  https://hub.example///  "));
        assert!(!h.contains("hub.example"), "the URL must not survive: {h}");
        assert!(h.starts_with("h-") && h.len() == 14, "{h}");
        assert_ne!(h, hub_url_hash("https://other.example"));
    }

    #[test]
    fn the_status_vocabulary_round_trips() {
        for s in ContributionStatus::ALL {
            assert_eq!(ContributionStatus::from_wire(s.as_str()), Some(s));
        }
        assert_eq!(ContributionStatus::from_wire("from_a_newer_release"), None);
        assert!(ContributionStatus::Sent.landed());
        assert!(!ContributionStatus::Rejected.landed());
    }
}
