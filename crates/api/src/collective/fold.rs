//! Case-weighted re-aggregation of one contributor's post-clamp entry list.
//!
//! **Why this exists.** `collective_entries` is keyed on `(contributor_id, provider, model,
//! task_type)` and a re-contribution upserts in place with last-writer-wins. So two entries in ONE
//! payload that share that key do not merge — the second silently overwrites the first and the
//! contributor's evidence for that bucket is halved with nothing disclosed. That collision is
//! reachable today with two identical keys, and clamping `task_type` to a closed vocabulary makes it
//! *common*: two invented labels both become `general`.
//!
//! A clamp that loses evidence is worse than no clamp, and a clamp that loses it silently is the
//! defect the clamp was added to remove. So the entry list is folded here — in the API, before the
//! single `replace_collective_contribution` call — deliberately **not** in the store, so no backend's
//! parity obligation grows and the rule holds identically on SQLite, Postgres and Firestore.
//!
//! **Direction of every fold is conservative**, matching `core::collective::merge`:
//!   - counts **sum**; quality / pass-rate / cost / p50 are **case-weighted means**;
//!   - p95 is the **max** (worst observed tail), never a mean;
//!   - judge family and rubric fingerprint **collapse** to `mixed` when they disagree;
//!   - determinism folds to the **weakest** stamp, and one unrecorded entry voids the claim;
//!   - the coverage tags degrade to `Mixed` on any disagreement;
//!   - variance is recombined by the law of total variance, and is dropped to `None` unless **every**
//!     folded entry carried one — a partial variance is not a variance.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use lighttrack_core::collective::rigor::weakest_determinism;
use lighttrack_core::{bucket_cost, CollectiveEntry, Coverage};

/// Fold a contributor's sanitized entries so each `(provider, model, task_type)` appears exactly
/// once. Returns the folded list — in first-appearance order, so the result is deterministic for a
/// given payload — and how many entries were **absorbed** into an earlier one, which the ingest ack
/// discloses as `folded_duplicates`.
///
/// A key that appears once passes through **untouched**: re-deriving a weighted mean over a single
/// member would reintroduce float noise into a number nothing needed to change.
pub(super) fn fold_entries(entries: Vec<CollectiveEntry>) -> (Vec<CollectiveEntry>, usize) {
    let mut groups: Vec<Acc> = Vec::with_capacity(entries.len());
    let mut absorbed = 0usize;
    for e in entries {
        match groups
            .iter_mut()
            .find(|g| g.key() == (&e.provider, &e.model, &e.task_type))
        {
            Some(g) => {
                g.push(e);
                absorbed += 1;
            }
            None => groups.push(Acc::new(e)),
        }
    }
    (groups.into_iter().map(Acc::finish).collect(), absorbed)
}

/// One `(provider, model, task_type)` bucket mid-fold. Holds the first entry as the identity carrier
/// plus the weighted sums needed to rebuild it; `n == 1` finishes by handing the entry straight back.
struct Acc {
    first: CollectiveEntry,
    n: usize,
    /// Σ cases, as the weight — `f64` because every mean below divides by it.
    w: f64,
    q_sum: f64,
    p_sum: f64,
    c_sum: f64,
    /// Σ wᵢ·(vᵢ + qᵢ²), the second-moment sum the law of total variance needs.
    q2_sum: f64,
    all_variance_known: bool,
    p50_w: f64,
    p50_sum: f64,
    p95: Option<u64>,
    n_runs: u32,
    n_cases: u32,
    judges: BTreeSet<String>,
    rubrics: BTreeSet<String>,
    determinism: Option<String>,
    frozen: Coverage,
    significance: Coverage,
    received_at: DateTime<Utc>,
}

impl Acc {
    fn new(e: CollectiveEntry) -> Acc {
        let w = e.n_cases as f64;
        let mut acc = Acc {
            w,
            q_sum: e.quality * w,
            p_sum: e.pass_rate * w,
            c_sum: e.avg_cost_usd * w,
            q2_sum: (e.quality_variance.unwrap_or(0.0) + e.quality * e.quality) * w,
            all_variance_known: e.quality_variance.is_some(),
            p50_w: e.p50_latency_ms.map_or(0.0, |_| w),
            p50_sum: e.p50_latency_ms.map_or(0.0, |v| v as f64 * w),
            p95: e.p95_latency_ms,
            n_runs: e.n_runs,
            n_cases: e.n_cases,
            judges: BTreeSet::new(),
            rubrics: BTreeSet::new(),
            determinism: e.determinism.clone(),
            frozen: e.frozen_dataset,
            significance: e.significance_tested,
            received_at: e.received_at,
            n: 1,
            first: e,
        };
        acc.note_tags_of_first();
        acc
    }

    fn note_tags_of_first(&mut self) {
        if let Some(j) = self.first.judge_provider.clone() {
            self.judges.insert(j);
        }
        if let Some(r) = self.first.rubric_fingerprint.clone() {
            self.rubrics.insert(r);
        }
    }

    fn key(&self) -> (&String, &String, &String) {
        (
            &self.first.provider,
            &self.first.model,
            &self.first.task_type,
        )
    }

    fn push(&mut self, e: CollectiveEntry) {
        let w = e.n_cases as f64;
        self.n += 1;
        self.w += w;
        self.q_sum += e.quality * w;
        self.p_sum += e.pass_rate * w;
        self.c_sum += e.avg_cost_usd * w;
        self.q2_sum += (e.quality_variance.unwrap_or(0.0) + e.quality * e.quality) * w;
        self.all_variance_known &= e.quality_variance.is_some();
        if let Some(v) = e.p50_latency_ms {
            self.p50_w += w;
            self.p50_sum += v as f64 * w;
        }
        self.p95 = match (self.p95, e.p95_latency_ms) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        // Saturating: the plausibility gate bounds each entry, and a sum that wrapped would hand the
        // row to whoever split their claim across enough entries.
        self.n_runs = self.n_runs.saturating_add(e.n_runs);
        self.n_cases = self.n_cases.saturating_add(e.n_cases);
        if let Some(j) = e.judge_provider {
            self.judges.insert(j);
        }
        if let Some(r) = e.rubric_fingerprint {
            self.rubrics.insert(r);
        }
        self.determinism =
            weakest_determinism(self.determinism.as_deref(), e.determinism.as_deref());
        self.frozen = self.frozen.fold(e.frozen_dataset);
        self.significance = self.significance.fold(e.significance_tested);
        self.received_at = self.received_at.max(e.received_at);
    }

    fn finish(self) -> CollectiveEntry {
        if self.n == 1 {
            return self.first;
        }
        let w = self.w.max(1.0);
        let quality = self.q_sum / w;
        CollectiveEntry {
            quality,
            pass_rate: self.p_sum / w,
            // Re-bucketed after the weighted mean, for the same reason ingest buckets in the first
            // place: the published cost figure must be coarse however it was arrived at.
            avg_cost_usd: bucket_cost(self.c_sum / w),
            p50_latency_ms: (self.p50_w > 0.0).then(|| (self.p50_sum / self.p50_w).round() as u64),
            p95_latency_ms: self.p95,
            n_runs: self.n_runs,
            n_cases: self.n_cases,
            // Law of total variance: E[v] + Var(means) = Σwᵢ(vᵢ+qᵢ²)/W − q̄². Clamped at 0 so float
            // cancellation cannot publish a negative variance.
            quality_variance: self
                .all_variance_known
                .then(|| (self.q2_sum / w - quality * quality).max(0.0)),
            judge_provider: collapse(&self.judges),
            rubric_fingerprint: collapse(&self.rubrics),
            determinism: self.determinism,
            frozen_dataset: self.frozen,
            significance_tested: self.significance,
            received_at: self.received_at,
            ..self.first
        }
    }
}

/// Collapse a tag set the way `merge::collapse` does: one value survives, several become `mixed`,
/// none stays absent. `mixed` is the value a *fold* produces and no single contribution ever is.
fn collapse(set: &BTreeSet<String>) -> Option<String> {
    match set.len() {
        0 => None,
        1 => set.iter().next().cloned(),
        _ => Some("mixed".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(task_type: &str, q: f64, runs: u32, cases: u32) -> CollectiveEntry {
        CollectiveEntry {
            contributor_id: "c-1".into(),
            provider: "anthropic".into(),
            model: "haiku".into(),
            task_type: task_type.into(),
            quality: q,
            pass_rate: q,
            avg_cost_usd: 0.003,
            p50_latency_ms: Some(900),
            p95_latency_ms: Some(2000),
            n_runs: runs,
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
    fn a_unique_key_passes_through_byte_for_byte() {
        let one = e("qa", 0.8123456789, 1, 10);
        let (folded, absorbed) = fold_entries(vec![one.clone()]);
        assert_eq!(absorbed, 0);
        assert_eq!(folded.len(), 1);
        // Not merely close: a key that collides with nothing must not be recomputed at all.
        assert_eq!(folded[0].quality, one.quality);
        assert_eq!(folded[0].avg_cost_usd, one.avg_cost_usd);
    }

    #[test]
    fn colliding_keys_sum_their_evidence_and_weight_their_quality() {
        let (folded, absorbed) = fold_entries(vec![e("qa", 0.8, 1, 10), e("qa", 0.4, 3, 30)]);
        assert_eq!(absorbed, 1);
        assert_eq!(folded.len(), 1);
        assert_eq!((folded[0].n_cases, folded[0].n_runs), (40, 4));
        assert!((folded[0].quality - 0.5).abs() < 1e-12);
        assert!((folded[0].pass_rate - 0.5).abs() < 1e-12);
        assert_eq!(folded[0].p95_latency_ms, Some(2000));
    }

    #[test]
    fn distinct_keys_are_left_alone_in_first_appearance_order() {
        let (folded, absorbed) = fold_entries(vec![
            e("qa", 0.8, 1, 10),
            e("summarization", 0.4, 1, 10),
            e("qa", 0.6, 1, 10),
        ]);
        assert_eq!(absorbed, 1);
        assert_eq!(folded.len(), 2);
        assert_eq!(folded[0].task_type, "qa");
        assert_eq!(folded[0].n_cases, 20);
        assert_eq!(folded[1].task_type, "summarization");
    }

    #[test]
    fn tags_collapse_and_rigor_folds_to_the_weaker_claim() {
        let mut a = e("qa", 0.8, 1, 10);
        a.judge_provider = Some("anthropic".into());
        a.rubric_fingerprint = Some("ab12".into());
        a.determinism = Some("exact".into());
        a.frozen_dataset = Coverage::All;
        a.significance_tested = Coverage::All;
        let mut b = e("qa", 0.8, 1, 10);
        b.judge_provider = Some("openai".into());
        b.rubric_fingerprint = Some("ab12".into());
        b.determinism = Some("sampled".into());
        b.frozen_dataset = Coverage::None;
        b.significance_tested = Coverage::All;
        let (folded, _) = fold_entries(vec![a, b]);
        let f = &folded[0];
        assert_eq!(f.judge_provider.as_deref(), Some("mixed"));
        assert_eq!(
            f.rubric_fingerprint.as_deref(),
            Some("ab12"),
            "agreement survives"
        );
        assert_eq!(f.determinism.as_deref(), Some("sampled"), "weakest wins");
        assert_eq!(f.frozen_dataset, Coverage::Mixed);
        assert_eq!(f.significance_tested, Coverage::All);
    }

    #[test]
    fn an_unrecorded_determinism_voids_the_folded_claim() {
        let mut a = e("qa", 0.8, 1, 10);
        a.determinism = Some("exact".into());
        let b = e("qa", 0.8, 1, 10); // records none
        let (folded, _) = fold_entries(vec![a, b]);
        assert!(
            folded[0].determinism.is_none(),
            "silence cannot vouch for the recorded entry"
        );
    }

    #[test]
    fn variance_recombines_only_when_every_member_has_one() {
        // Equal weights, equal within-variance 0.04, means 0.70 and 0.90 ⇒
        // E[v] = 0.04, Var(means) = 0.01 ⇒ 0.05.
        let mut a = e("qa", 0.70, 1, 10);
        a.quality_variance = Some(0.04);
        let mut b = e("qa", 0.90, 1, 10);
        b.quality_variance = Some(0.04);
        let (folded, _) = fold_entries(vec![a.clone(), b.clone()]);
        let v = folded[0].quality_variance.expect("both members carry one");
        assert!((v - 0.05).abs() < 1e-12, "{v}");
        // One silent member ⇒ no variance at all, rather than a figure resting on half the evidence.
        let mut c = e("qa", 0.90, 1, 10);
        c.quality_variance = None;
        let (folded, _) = fold_entries(vec![a, c]);
        assert!(folded[0].quality_variance.is_none());
    }

    #[test]
    fn a_missing_p50_does_not_drag_the_folded_median_to_zero() {
        let a = e("qa", 0.8, 1, 10);
        let mut b = e("qa", 0.8, 1, 30);
        b.p50_latency_ms = None;
        b.p95_latency_ms = None;
        let (folded, _) = fold_entries(vec![a, b]);
        assert_eq!(
            folded[0].p50_latency_ms,
            Some(900),
            "weighted over the entries that reported one"
        );
        assert_eq!(folded[0].p95_latency_ms, Some(2000), "worst observed tail");
    }
}
