//! What a hub will believe from a contributor.
//!
//! Every entry [`super::ingest`] receives passes through [`sanitize_entry`], which is the whole of
//! the hub's trust policy in one place: identity normalization, closed vocabularies, `[0,1]` clamps,
//! and the plausibility rules that reject a count rather than clamping it.

use chrono::Utc;

use lighttrack_core::{CollectiveEntry, ModelAliases};

/// Why a contributed entry was refused. Kept apart in the ack so a contributor can tell "you sent
/// junk" from "your numbers are not believable".
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Reject {
    /// No usable model identity (empty provider / model / task_type, or zero cases).
    Malformed,
    /// Structurally fine but not a believable benchmark result — see [`implausible`].
    Implausible,
}

/// The largest per-bucket case count a hub will believe from one contributor. A single
/// `(model, task_type)` bucket with more than a million scored cases is a typo or an attack, not a
/// benchmark; accepting it hands the merged row to whoever types the biggest number.
const MAX_CASES_PER_ENTRY: u32 = 1_000_000;

/// The largest per-case cost a hub will believe. $1000 for one case is not a price, it is noise.
const MAX_COST_PER_CASE_USD: f64 = 1_000.0;

/// The plausibility rules, written down in one place so they can be documented verbatim:
///   - every published number is finite (no NaN/∞ smuggled through JSON);
///   - `n_runs ≥ 1` — a bucket with no runs produced no cases;
///   - `n_cases ≥ n_runs` — a run scores at least one case, so more runs than cases is impossible;
///   - `n_cases ≤ MAX_CASES_PER_ENTRY`;
///   - `avg_cost_usd ≤ MAX_COST_PER_CASE_USD`.
///
/// Quality/pass-rate are *clamped* rather than rejected (a `[0,1]` overshoot is a rounding artifact);
/// counts are *rejected*, because a count is the weight the merge trusts.
pub(super) fn implausible(e: &lighttrack_core::ModelDigestEntry) -> bool {
    !e.quality.is_finite()
        || !e.pass_rate.is_finite()
        || !e.avg_cost_usd.is_finite()
        || e.n_runs == 0
        || e.n_cases < e.n_runs
        || e.n_cases > MAX_CASES_PER_ENTRY
        || e.avg_cost_usd > MAX_COST_PER_CASE_USD
}

/// Validate/clamp one contributed entry. The model identity is **normalized** through `aliases` so
/// equivalent spellings merge into one leaderboard row.
pub(super) fn sanitize_entry(
    contributor: &str,
    e: lighttrack_core::ModelDigestEntry,
    now: chrono::DateTime<Utc>,
    aliases: &ModelAliases,
) -> Result<CollectiveEntry, Reject> {
    let provider = e.provider.trim();
    let model = e.model.trim();
    let task_type = e.task_type.trim().to_string();
    if provider.is_empty() || model.is_empty() || task_type.is_empty() || e.n_cases == 0 {
        return Err(Reject::Malformed);
    }
    if implausible(&e) {
        return Err(Reject::Implausible);
    }
    let (provider, model) = aliases.normalize(provider, model);
    Ok(CollectiveEntry {
        contributor_id: contributor.to_string(),
        provider,
        model,
        task_type,
        quality: e.quality.clamp(0.0, 1.0),
        pass_rate: e.pass_rate.clamp(0.0, 1.0),
        // Re-bucketed hub-side for the same reason the k-floor is re-enforced hub-side: what the
        // contributor did to its own numbers is its business, what gets published is the hub's.
        avg_cost_usd: lighttrack_core::bucket_cost(e.avg_cost_usd.max(0.0)),
        p50_latency_ms: e.p50_latency_ms,
        p95_latency_ms: e.p95_latency_ms,
        n_runs: e.n_runs,
        n_cases: e.n_cases,
        // v2: carry the variance if present; a negative value is nonsense, so drop it to None.
        quality_variance: e.quality_variance.filter(|v| v.is_finite() && *v >= 0.0),
        // v2: clamp the judge tag to the known vocabulary; anything else is `unknown`.
        judge_provider: e
            .judge_provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(canon_judge),
        rubric_fingerprint: e
            .rubric_fingerprint
            .map(|r| r.trim().chars().take(32).collect::<String>())
            .filter(|s| !s.is_empty()),
        // v3 rigor: closed vocabularies, clamped hub-side. An unrecognized determinism label becomes
        // "not recorded" rather than a fourth level — a poster must not be able to widen the rigor
        // vocabulary, which is exactly what would turn it into a fingerprinting channel.
        determinism: e
            .determinism
            .as_deref()
            .and_then(lighttrack_core::canon_determinism),
        frozen_dataset: e.frozen_dataset,
        significance_tested: e.significance_tested,
        received_at: now,
    })
}

/// Clamp a contributed judge tag to the known vocabulary, so a poster can't inject arbitrary judge
/// labels. The vocabulary is `ProviderFamily` plus `mixed` — the one value a *merge* produces and no
/// single judge ever is (see `merge::collapse`), which is why it cannot come from `judge_family`.
fn canon_judge(j: &str) -> String {
    if j.trim().eq_ignore_ascii_case("mixed") {
        return "mixed".to_string();
    }
    lighttrack_core::judge_family(j).as_str().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_entry_clamps_and_drops_identityless() {
        let now = Utc::now();
        let a = ModelAliases::default();
        let good = lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(),
            model: "haiku".into(),
            task_type: "qa".into(),
            quality: 1.4,
            pass_rate: -0.2,
            avg_cost_usd: -1.0,
            p50_latency_ms: None,
            p95_latency_ms: None,
            n_runs: 2,
            n_cases: 9,
            quality_variance: Some(-0.5), // negative variance is nonsense → dropped to None
            judge_provider: Some("weird-label".into()), // unknown label → clamped to "unknown"
            rubric_fingerprint: Some("ab12cd34".into()),
            // A determinism label outside the closed vocabulary must not become a fourth level.
            determinism: Some("perfectly-reproducible".into()),
            frozen_dataset: lighttrack_core::Coverage::All,
            significance_tested: lighttrack_core::Coverage::Mixed,
        };
        let s = sanitize_entry("c-abc", good, now, &a).unwrap();
        assert_eq!(s.quality, 1.0);
        assert_eq!(s.pass_rate, 0.0);
        assert_eq!(s.avg_cost_usd, 0.0);
        assert!(s.quality_variance.is_none(), "negative variance dropped");
        assert_eq!(
            s.judge_provider.as_deref(),
            Some("unknown"),
            "unknown judge label clamped"
        );
        assert_eq!(s.rubric_fingerprint.as_deref(), Some("ab12cd34"));
        assert!(
            s.determinism.is_none(),
            "an invented determinism label is dropped, not admitted"
        );
        assert_eq!(
            s.frozen_dataset,
            lighttrack_core::Coverage::All,
            "rigor coverage survives ingest"
        );
        assert_eq!(s.significance_tested, lighttrack_core::Coverage::Mixed);
        let bad = lighttrack_core::ModelDigestEntry {
            provider: "  ".into(),
            model: "haiku".into(),
            task_type: "qa".into(),
            quality: 0.5,
            pass_rate: 0.5,
            avg_cost_usd: 0.1,
            p50_latency_ms: None,
            p95_latency_ms: None,
            n_runs: 1,
            n_cases: 5,
            quality_variance: None,
            judge_provider: None,
            rubric_fingerprint: None,
            determinism: None,
            frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        assert_eq!(
            sanitize_entry("c-abc", bad, now, &a).unwrap_err(),
            Reject::Malformed
        );
    }

    #[test]
    fn implausible_counts_are_rejected_not_clamped() {
        let now = Utc::now();
        let a = ModelAliases::default();
        let base = || lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(),
            model: "haiku".into(),
            task_type: "qa".into(),
            quality: 0.8,
            pass_rate: 0.8,
            avg_cost_usd: 0.01,
            p50_latency_ms: None,
            p95_latency_ms: None,
            n_runs: 2,
            n_cases: 100,
            quality_variance: None,
            judge_provider: None,
            rubric_fingerprint: None,
            determinism: None,
            frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        let rejected = |mutate: fn(&mut lighttrack_core::ModelDigestEntry)| {
            let mut e = base();
            mutate(&mut e);
            sanitize_entry("c", e, now, &a).unwrap_err()
        };
        // A billion cases in one bucket is a typo or an attack, never a benchmark.
        assert_eq!(rejected(|e| e.n_cases = 1_000_000_000), Reject::Implausible);
        // More runs than cases is arithmetically impossible.
        assert_eq!(rejected(|e| e.n_runs = 500), Reject::Implausible);
        assert_eq!(rejected(|e| e.n_runs = 0), Reject::Implausible);
        assert_eq!(rejected(|e| e.avg_cost_usd = 5_000.0), Reject::Implausible);
        assert_eq!(rejected(|e| e.quality = f64::NAN), Reject::Implausible);
        // The believable end of the range still lands.
        let mut e = base();
        e.n_cases = MAX_CASES_PER_ENTRY;
        assert!(
            sanitize_entry("c", e, now, &a).is_ok(),
            "the ceiling itself is accepted"
        );
    }

    #[test]
    fn ingest_normalizes_model_identity() {
        let now = Utc::now();
        let a = ModelAliases::from_json_str(
            r#"{"providers":{"azure-openai":"openai"},"models":{"gpt-4o-2024-08-06":"gpt-4o"}}"#,
        )
        .unwrap();
        let e = |provider: &str, model: &str| lighttrack_core::ModelDigestEntry {
            provider: provider.into(),
            model: model.into(),
            task_type: "qa".into(),
            quality: 0.8,
            pass_rate: 0.8,
            avg_cost_usd: 0.01,
            p50_latency_ms: None,
            p95_latency_ms: None,
            n_runs: 1,
            n_cases: 10,
            quality_variance: None,
            judge_provider: None,
            rubric_fingerprint: None,
            determinism: None,
            frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        // provider/ prefix stripped + dated variant collapsed + provider synonym mapped.
        let s = sanitize_entry("c", e("openai", "openai/gpt-4o-2024-08-06"), now, &a).unwrap();
        assert_eq!(
            (s.provider.as_str(), s.model.as_str()),
            ("openai", "gpt-4o")
        );
        let s = sanitize_entry("c", e("azure-openai", "gpt-4o"), now, &a).unwrap();
        assert_eq!(s.provider, "openai");
    }
}
