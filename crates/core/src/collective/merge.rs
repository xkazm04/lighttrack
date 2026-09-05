//! Pure aggregation: build an instance's digest from its run scorecards, and merge stored
//! contributions into the public leaderboard.
//!
//! **Variance & confidence intervals (approximate — read this before trusting the numbers).**
//! Quality is a mean of per-case scores, but a digest only ever carries aggregates, so the merge works
//! with second-order summaries, not raw cases:
//!   - *Digest side.* A bucket's `quality_variance` is the **case-weighted population variance of the
//!     contributing runs' mean scores** — dispersion *between runs*, computed from `Σw·q²/Σw − (Σw·q/Σw)²`.
//!     It is `None` for a single-run bucket (variance undefined).
//!   - *Merge side, within-source.* The leaderboard treats each contributor's `quality_variance` as
//!     the score dispersion and pools it case-weighted: `V = Σ(nᵢ·vᵢ)/Σnᵢ` over entries with a known
//!     variance, then `SE_within = √(V / N_known)`. This part is still an approximation — it uses
//!     between-run variance as a stand-in for case-level variance. When fewer than
//!     [`VARIANCE_COVERAGE_MIN`] of the cases carry a known variance, no CI is published at all (an
//!     honest "insufficient variance data" marker) rather than a fabricated number.
//!   - *Merge side, between-source.* Pooling alone made the interval shrink with total evidence
//!     whether or not the contributors agreed, so five sources that **disagreed** got a *narrower*
//!     interval than five that agreed. A random-effects term fixes the direction: `SE_between² =
//!     τ̂²·Σpᵢ²` over the winsorized per-source weights (see [`super::spread`] for the estimator, its
//!     behaviour at k=2, and why it is not DerSimonian–Laird). The published half-width is
//!     `1.96·√(SE_within² + SE_between²)`.
//!   - *Disagreement is visible either way.* Every multi-source row publishes `source_spread` — the
//!     weighted SD across its sources' means — **even when no CI could be formed**, so a row built
//!     entirely from v1 contributions still shows whether its sources agree.
//!
//! Ranking is always by the point estimate `quality`; the CI, `source_spread` and the
//! `low_confidence` flag are annotation, never a reordering.

use std::collections::{BTreeMap, BTreeSet};

use super::rigor::{sort_levels, weakest_determinism, Coverage, RowRigor};
use super::spread::{between_sources, Between};
use super::types::{CollectiveEntry, LeaderboardRow, ModelDigestEntry, RunStat};

/// Minimum fraction of a row's cases that must carry a known variance before a CI is estimated.
const VARIANCE_COVERAGE_MIN: f64 = 0.5;

/// **Bounded unilateral influence.** The largest share of a merged row's weight any single source may
/// hold, once the row has ≥2 sources. Flat case-weighting takes `n_cases` at face value, so the row
/// goes to whoever types the biggest number; winsorizing the top source's weight to this share means a
/// contributor can *lead* a row but never *own* it. 0.8 is deliberately generous — a genuinely large
/// contribution still outweighs everyone else combined 4:1, so sample size keeps mattering — and the
/// residual is closed at ingest, where implausible case counts are rejected outright. Every row
/// discloses its realized `max_source_share`.
pub const MAX_SOURCE_WEIGHT_SHARE: f64 = 0.8;

/// z for a two-sided 95% interval.
const Z_95: f64 = 1.96;

/// One case-weighted observation folded into an [`Acc`] — a run (digest) or a stored entry (merge).
struct Sample {
    /// The weight this sample carries in the pooled means. Equal to `cases` on the digest side (one
    /// instance pooling its own runs); on the merge side it is the **winsorized** case count, so one
    /// source cannot exceed [`MAX_SOURCE_WEIGHT_SHARE`] of the row.
    weight: f64,
    quality: f64,
    pass_rate: f64,
    cost: f64,
    cases: u32,
    p50: Option<u64>,
    p95: Option<u64>,
    runs: u32,
    /// A pre-computed variance the sample brings (merge side); `None` on the digest side, where the
    /// bucket variance is derived from the spread of run qualities instead.
    variance: Option<f64>,
    /// Coarse judge family behind this sample (`anthropic|openai|google|unknown|mixed`), if recorded.
    judge_provider: Option<String>,
    /// Rubric-shape fingerprint behind this sample, if recorded.
    rubric_fingerprint: Option<String>,
    /// Determinism stamp behind this sample, if recorded.
    determinism: Option<String>,
    /// Frozen-dataset coverage this sample brings (a single run on the digest side, an already-folded
    /// contributor bucket on the merge side).
    frozen: Coverage,
    /// Significance-tested coverage this sample brings.
    tested: Coverage,
    /// Digest side only: the dataset version this run was pinned to, so a bucket can tell whether its
    /// runs sat on ONE pin. Never published — see the `rigor` module's fingerprinting argument.
    dataset_version: Option<u32>,
}

/// Case-weighted accumulator shared by digest building and leaderboard merging.
#[derive(Default)]
struct Acc {
    /// Raw cases behind the row — reported as-is (`n_cases`) and used for the display floor and the
    /// variance-coverage test. Distinct from `w`, which is what the *means* are weighted by.
    cases: u64,
    w: f64, // Σ effective weight
    q_w: f64,
    q_w2: f64, // Σ w·q²  — for the digest-side between-run variance
    p_w: f64,
    c_w: f64,
    lat_w_total: f64,
    lat_w: f64,
    p95_max: u64,
    runs: u32,
    var_w: f64, // Σ w·vᵢ over samples with a known variance — for the merge-side pooled CI
    var_weight: f64, // Σ w over samples with a known variance
    var_cases: u64, // Σ raw cases over samples with a known variance (coverage test / SE)
    contributors: BTreeSet<String>,
    judge_providers: BTreeSet<String>,
    rubric_fps: BTreeSet<String>,
    /// Rigor, folded conservatively: the weakest determinism seen (`None` once anything was
    /// unrecorded), every distinct stamp, and the two boolean facets as [`Coverage`].
    seen: bool,
    determinism: Option<String>,
    determinism_levels: BTreeSet<String>,
    frozen: Coverage,
    tested: Coverage,
    dataset_versions: BTreeSet<u32>,
}

impl Acc {
    fn add(&mut self, s: Sample, contributor: Option<&str>) {
        let w = s.weight;
        self.cases += s.cases as u64;
        self.w += w;
        self.q_w += s.quality * w;
        self.q_w2 += s.quality * s.quality * w;
        self.p_w += s.pass_rate * w;
        self.c_w += s.cost * w;
        self.runs += s.runs;
        if let Some(p) = s.p50 {
            self.lat_w_total += w;
            self.lat_w += p as f64 * w;
        }
        if let Some(p) = s.p95 {
            self.p95_max = self.p95_max.max(p);
        }
        if let Some(v) = s.variance {
            self.var_w += v * w;
            self.var_weight += w;
            self.var_cases += s.cases as u64;
        }
        if let Some(j) = s.judge_provider.filter(|j| !j.is_empty()) {
            self.judge_providers.insert(j);
        }
        if let Some(r) = s.rubric_fingerprint.filter(|r| !r.is_empty()) {
            self.rubric_fps.insert(r);
        }
        if let Some(c) = contributor {
            self.contributors.insert(c.to_string());
        }
        // Rigor folds conservatively: the first sample seeds, every later one can only weaken.
        if let Some(d) = &s.determinism {
            self.determinism_levels.insert(d.clone());
        }
        self.determinism = if self.seen {
            weakest_determinism(self.determinism.as_deref(), s.determinism.as_deref())
        } else {
            s.determinism
        };
        self.frozen = if self.seen {
            self.frozen.fold(s.frozen)
        } else {
            s.frozen
        };
        self.tested = if self.seen {
            self.tested.fold(s.tested)
        } else {
            s.tested
        };
        if let Some(v) = s.dataset_version {
            self.dataset_versions.insert(v);
        }
        self.seen = true;
    }

    /// Digest side: the bucket's frozen-dataset claim. `All` needs every run frozen **and** pinned to
    /// one version — two versions of the same frozen dataset are two different case sets, so the
    /// bucket's numbers are not from one immutable pin and the claim degrades to `Mixed`.
    fn frozen_claim(&self) -> Coverage {
        if self.frozen == Coverage::All && self.dataset_versions.len() > 1 {
            return Coverage::Mixed;
        }
        self.frozen
    }

    /// Merge side: the row's rigor, with mixture disclosed rather than flattened.
    fn row_rigor(&self) -> RowRigor {
        let mut determinism_levels: Vec<String> = self.determinism_levels.iter().cloned().collect();
        sort_levels(&mut determinism_levels);
        RowRigor {
            determinism: self.determinism.clone(),
            determinism_levels,
            frozen_dataset: self.frozen,
            significance_tested: self.tested,
        }
    }

    fn quality(&self) -> f64 {
        if self.w <= 0.0 {
            0.0
        } else {
            self.q_w / self.w
        }
    }
    fn pass_rate(&self) -> f64 {
        if self.w <= 0.0 {
            0.0
        } else {
            self.p_w / self.w
        }
    }
    fn cost(&self) -> f64 {
        if self.w <= 0.0 {
            0.0
        } else {
            self.c_w / self.w
        }
    }
    fn p50(&self) -> Option<u64> {
        (self.lat_w_total > 0.0).then(|| (self.lat_w / self.lat_w_total).round() as u64)
    }
    fn p95(&self) -> Option<u64> {
        (self.p95_max > 0).then_some(self.p95_max)
    }

    /// Digest side: case-weighted population variance of the runs' mean scores. `None` with < 2 runs.
    fn run_variance(&self) -> Option<f64> {
        if self.runs < 2 || self.w <= 0.0 {
            return None;
        }
        let mean = self.q_w / self.w;
        Some(((self.q_w2 / self.w) - mean * mean).max(0.0))
    }

    /// Merge side: approximate 95% CI half-width on the merged mean quality, or `None` when too little
    /// of the weight carries a known variance — the refusal to fabricate an interval survives the
    /// random-effects change untouched, because `between.se2()` alone would understate case-level
    /// noise just as badly as pooling alone understated disagreement.
    ///
    /// `between` is the row's between-source term; it is `0` for a single-source row.
    fn quality_ci95(&self, between: &Between) -> Option<f64> {
        if self.var_cases == 0 || self.cases == 0 {
            return None;
        }
        let coverage = self.var_cases as f64 / self.cases as f64;
        if coverage < VARIANCE_COVERAGE_MIN {
            return None;
        }
        let pooled_var = (self.var_w / self.var_weight).max(0.0);
        let se2_within = pooled_var / self.var_cases as f64;
        Some(Z_95 * (se2_within + between.se2()).sqrt())
    }
}

type Key = (String, String, String);

fn key_of(provider: &str, model: &str, task_type: &str) -> Key {
    (
        provider.to_string(),
        model.to_string(),
        task_type.to_string(),
    )
}

/// Collapse a set of tags into a single digest-entry value: the sole tag when they agree, `"mixed"`
/// when a bucket's runs disagree, `None` when nothing was recorded. Keeps a v2 entry singular while
/// still signalling incommensurability.
fn collapse(set: &BTreeSet<String>) -> Option<String> {
    match set.len() {
        0 => None,
        1 => set.iter().next().cloned(),
        _ => Some("mixed".to_string()),
    }
}

/// Build this instance's privacy-safe digest from its benchmark run scorecards. Buckets with fewer
/// than `min_cases` total cases are **dropped** (k-anonymity); the rest are sorted by quality desc.
///
/// Drops the withheld count on the floor. Prefer [`build_digest_counted`] on any path that shows
/// the digest to an operator: a bucket withheld by the floor and a bucket nobody measured are the
/// same absence here, and only the count separates them.
pub fn build_digest(stats: &[RunStat], min_cases: u32) -> Vec<ModelDigestEntry> {
    build_digest_counted(stats, min_cases).0
}

/// [`build_digest`], plus the number of buckets the k-anonymity floor withheld.
///
/// The count is *disclosure*, not payload: an empty board must be legible as "held back" rather
/// than read as "nobody measured this". It is deliberately **not** part of [`digest_sha256`] —
/// the hash gates whether a repeat push carries new evidence, and two digests whose entries are
/// identical carry the same evidence however many thin buckets sat behind them. Hashing it would
/// make every existing instance re-push unchanged data once.
pub fn build_digest_counted(stats: &[RunStat], min_cases: u32) -> (Vec<ModelDigestEntry>, u32) {
    let mut groups: BTreeMap<Key, Acc> = BTreeMap::new();
    for s in stats {
        if s.n_cases == 0 {
            continue;
        }
        groups
            .entry(key_of(&s.provider, &s.model, &s.task_type))
            .or_default()
            .add(
                Sample {
                    // Digest side: an instance pooling its own runs, so weight == cases (nothing to bound).
                    weight: s.n_cases as f64,
                    quality: s.quality,
                    pass_rate: s.pass_rate,
                    cost: s.cost_per_case_usd,
                    cases: s.n_cases,
                    p50: s.p50_latency_ms,
                    p95: s.p95_latency_ms,
                    runs: 1,
                    variance: None,
                    judge_provider: s.judge_provider.clone(),
                    rubric_fingerprint: s.rubric_fingerprint.clone(),
                    determinism: s.determinism.clone(),
                    frozen: Coverage::of(s.dataset_frozen),
                    tested: Coverage::of(s.significance_tested),
                    dataset_version: s.dataset_version,
                },
                None,
            );
    }
    let withheld = groups
        .values()
        .filter(|a| a.cases < min_cases as u64)
        .count() as u32;
    let mut out: Vec<ModelDigestEntry> = groups
        .into_iter()
        .filter(|(_, a)| a.cases >= min_cases as u64)
        .map(|((provider, model, task_type), a)| ModelDigestEntry {
            provider,
            model,
            task_type,
            quality: r3(a.quality()),
            pass_rate: r3(a.pass_rate()),
            // Cost is bucketed *before it leaves the instance*: a per-case cost is an unbounded
            // continuous fingerprint, so it gets a privacy treatment like every other published field.
            avg_cost_usd: super::privacy::bucket_cost(a.cost()),
            p50_latency_ms: a.p50(),
            p95_latency_ms: a.p95(),
            n_runs: a.runs,
            n_cases: a.cases as u32,
            quality_variance: a.run_variance().map(r6),
            judge_provider: collapse(&a.judge_providers),
            rubric_fingerprint: collapse(&a.rubric_fps),
            determinism: a.determinism.clone(),
            frozen_dataset: a.frozen_claim(),
            significance_tested: a.tested,
        })
        .collect();
    sort_by_quality(&mut out, |e| (e.quality, &e.provider, &e.model));
    (out, withheld)
}

/// Winsorize one row's per-source case counts so no single source exceeds
/// [`MAX_SOURCE_WEIGHT_SHARE`] of the row's weight. Only the largest element can breach the share (two
/// cannot each hold >80%), so clamping it to `share/(1-share)` times the sum of the rest is enough and
/// exact. A row with a single source is returned untouched — there is no "collective" to skew, and the
/// hub's `min_contributors` floor already decides whether such a row is publishable at all.
fn winsorized_weights(cases: &[f64]) -> Vec<f64> {
    let mut w = cases.to_vec();
    if w.len() < 2 {
        return w;
    }
    let (i, &top) = match w.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)) {
        Some(x) => x,
        None => return w,
    };
    let others: f64 = w.iter().sum::<f64>() - top;
    let ceiling = others * MAX_SOURCE_WEIGHT_SHARE / (1.0 - MAX_SOURCE_WEIGHT_SHARE);
    if top > ceiling {
        w[i] = ceiling;
    }
    w
}

/// Merge stored contributions from many instances into the public leaderboard. Each
/// `(provider, model, task_type)` is case-weighted across contributors — with each source's weight
/// **winsorized** to [`MAX_SOURCE_WEIGHT_SHARE`], so a contributor claiming an enormous `n_cases`
/// cannot own a row (the realized share is disclosed as `max_source_share`). `n_contributors` counts
/// the distinct sources and `n_cases` reports the raw evidence volume, uncapped. Rows aggregating
/// fewer than `low_confidence_floor` cases are flagged (not hidden). Sorted by quality desc.
pub fn merge_leaderboard(
    entries: &[CollectiveEntry],
    low_confidence_floor: u32,
) -> Vec<LeaderboardRow> {
    let mut buckets: BTreeMap<Key, Vec<&CollectiveEntry>> = BTreeMap::new();
    for e in entries {
        buckets
            .entry(key_of(&e.provider, &e.model, &e.task_type))
            .or_default()
            .push(e);
    }
    let groups: BTreeMap<Key, (Acc, f64, Between)> = buckets
        .into_iter()
        .map(|(k, es)| {
            let raw: Vec<f64> = es.iter().map(|e| e.n_cases as f64).collect();
            let weights = winsorized_weights(&raw);
            let total: f64 = weights.iter().sum();
            let top = weights.iter().copied().fold(0.0_f64, f64::max);
            let max_share = if total > 0.0 { top / total } else { 0.0 };
            // Between-source heterogeneity over the SAME winsorized weights the mean uses, so a whale
            // can no more dominate the row's disagreement than it can its point estimate.
            let qualities: Vec<f64> = es.iter().map(|e| e.quality).collect();
            let between = between_sources(&weights, &qualities);
            let mut a = Acc::default();
            for (e, &weight) in es.iter().zip(weights.iter()) {
                a.add(
                    Sample {
                        weight,
                        quality: e.quality,
                        pass_rate: e.pass_rate,
                        cost: e.avg_cost_usd,
                        cases: e.n_cases,
                        p50: e.p50_latency_ms,
                        p95: e.p95_latency_ms,
                        runs: e.n_runs,
                        variance: e.quality_variance,
                        judge_provider: e.judge_provider.clone(),
                        rubric_fingerprint: e.rubric_fingerprint.clone(),
                        determinism: e.determinism.clone(),
                        frozen: e.frozen_dataset,
                        tested: e.significance_tested,
                        dataset_version: None,
                    },
                    Some(&e.contributor_id),
                );
            }
            (k, (a, max_share, between))
        })
        .collect();
    let mut out: Vec<LeaderboardRow> = groups
        .into_iter()
        .map(
            |((provider, model, task_type), (a, max_source_share, between))| {
                let judge_providers: Vec<String> = a.judge_providers.iter().cloned().collect();
                let mixed_judges = mixed_judges_of(&judge_providers);
                let rigor = a.row_rigor();
                let mixed_rigor = rigor.is_mixed();
                LeaderboardRow {
                    provider,
                    model,
                    task_type,
                    quality: r3(a.quality()),
                    quality_ci95: a.quality_ci95(&between).map(r3),
                    source_spread: between.spread().map(r3),
                    pass_rate: r3(a.pass_rate()),
                    avg_cost_usd: r6(a.cost()),
                    p50_latency_ms: a.p50(),
                    p95_latency_ms: a.p95(),
                    low_confidence: a.cases < low_confidence_floor as u64,
                    judge_providers,
                    mixed_judges,
                    n_contributors: a.contributors.len() as u32,
                    n_runs: a.runs,
                    n_cases: a.cases as u32,
                    max_source_share: r3(max_source_share),
                    rigor,
                    mixed_rigor,
                }
            },
        )
        .collect();
    sort_by_quality(&mut out, |r| (r.quality, &r.provider, &r.model));
    out
}

/// How many judge families a row's number was scored under, when that is more than one. A source
/// tagged `mixed` already disagreed with itself, so it stands for at least two families: a row built
/// from one such source is incommensurable exactly like a row judged by two contributors' different
/// judges, and must not read as single-judge because only one tag is on it.
fn mixed_judges_of(judge_providers: &[String]) -> Option<u32> {
    let families = judge_providers.len() + judge_providers.iter().filter(|j| *j == "mixed").count();
    (families > 1).then_some(families as u32)
}

/// Sort highest-quality first; ties broken by provider then model for stable output.
fn sort_by_quality<T, F>(v: &mut [T], key: F)
where
    F: Fn(&T) -> (f64, &String, &String),
{
    v.sort_by(|a, b| {
        let (qa, pa, ma) = key(a);
        let (qb, pb, mb) = key(b);
        qb.total_cmp(&qa)
            .then_with(|| pa.cmp(pb))
            .then_with(|| ma.cmp(mb))
    });
}

fn r3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
fn r6(x: f64) -> f64 {
    (x * 1_000_000.0).round() / 1_000_000.0
}

#[cfg(test)]
mod tests {
    use super::super::DEFAULT_LOW_CONFIDENCE_CASES;
    use super::*;
    use crate::collective::contribution::digest_sha256;
    use crate::collective::types::CollectiveDigest;
    use chrono::Utc;

    fn stat(provider: &str, model: &str, task: &str, q: f64, cost: f64, cases: u32) -> RunStat {
        RunStat {
            provider: provider.into(),
            model: model.into(),
            task_type: task.into(),
            quality: q,
            pass_rate: q,
            cost_per_case_usd: cost,
            n_cases: cases,
            p50_latency_ms: Some(800),
            p95_latency_ms: Some(1500),
            judge_provider: None,
            rubric_fingerprint: None,
            determinism: None,
            dataset_frozen: None,
            dataset_version: None,
            significance_tested: None,
        }
    }

    fn entry(
        contrib: &str,
        model: &str,
        q: f64,
        cases: u32,
        variance: Option<f64>,
    ) -> CollectiveEntry {
        judged(contrib, model, q, cases, variance, None)
    }

    fn judged(
        contrib: &str,
        model: &str,
        q: f64,
        cases: u32,
        variance: Option<f64>,
        judge: Option<&str>,
    ) -> CollectiveEntry {
        CollectiveEntry {
            contributor_id: contrib.into(),
            provider: "anthropic".into(),
            model: model.into(),
            task_type: "qa".into(),
            quality: q,
            pass_rate: q,
            avg_cost_usd: 0.003,
            p50_latency_ms: Some(900),
            p95_latency_ms: Some(2000),
            n_runs: 1,
            n_cases: cases,
            quality_variance: variance,
            judge_provider: judge.map(str::to_string),
            rubric_fingerprint: None,
            determinism: None,
            frozen_dataset: Coverage::Unknown,
            significance_tested: Coverage::Unknown,
            received_at: Utc::now(),
        }
    }

    #[test]
    fn withheld_count_separates_held_back_from_never_measured() {
        // Both arms produce an empty entry list. Only the count tells them apart, which is the
        // whole point: an empty board must read as "held back", not as "nobody measured this".
        let (entries, withheld) =
            build_digest_counted(&[stat("openai", "gpt-x", "qa", 0.9, 0.01, 3)], 5);
        assert!(entries.is_empty());
        assert_eq!(withheld, 1, "a thin bucket is withheld and disclosed");

        let (entries, withheld) = build_digest_counted(&[], 5);
        assert!(entries.is_empty());
        assert_eq!(withheld, 0, "nothing measured withholds nothing");

        // A mixed digest discloses only the suppressed buckets, not the published ones.
        let (entries, withheld) = build_digest_counted(
            &[
                stat("openai", "gpt-x", "qa", 0.9, 0.01, 6),
                stat("openai", "gpt-y", "qa", 0.8, 0.01, 2),
            ],
            5,
        );
        assert_eq!(entries.len(), 1);
        assert_eq!(withheld, 1);
    }

    #[test]
    fn withheld_count_is_not_in_the_contribution_hash() {
        // The hash gates whether a repeat push carries new evidence. Two digests with identical
        // entries carry the same evidence however many thin buckets sat behind them.
        let mut a = digest_for_hash();
        let mut b = digest_for_hash();
        a.buckets_withheld = 0;
        b.buckets_withheld = 7;
        assert_eq!(
            digest_sha256(&a),
            digest_sha256(&b),
            "disclosure must not make an unchanged digest look new"
        );
    }

    fn digest_for_hash() -> CollectiveDigest {
        CollectiveDigest {
            schema_version: 3,
            contributor_id: "c-abc".into(),
            generated_at: Utc::now(),
            min_cases: 5,
            projects_included: 1,
            projects_excluded: 0,
            buckets_withheld: 0,
            entries: Vec::new(),
        }
    }

    #[test]
    fn k_anonymity_drops_thin_buckets() {
        let d = build_digest(&[stat("openai", "gpt-x", "qa", 0.9, 0.01, 3)], 5);
        assert!(d.is_empty(), "thin bucket must be withheld");
        let d = build_digest(&[stat("openai", "gpt-x", "qa", 0.9, 0.01, 6)], 5);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].n_cases, 6);
        assert_eq!(d[0].n_runs, 1);
        // Single run → variance is undefined, not a fake 0.
        assert!(d[0].quality_variance.is_none());
    }

    #[test]
    fn digest_is_case_weighted_across_runs_with_variance() {
        // 0.6 over 10 cases, 0.9 over 90 cases → weighted mean 0.87.
        let d = build_digest(
            &[
                stat("anthropic", "haiku", "qa", 0.6, 0.002, 10),
                stat("anthropic", "haiku", "qa", 0.9, 0.004, 90),
            ],
            5,
        );
        assert_eq!(d.len(), 1);
        assert!((d[0].quality - 0.87).abs() < 1e-9, "got {}", d[0].quality);
        assert_eq!(d[0].n_runs, 2);
        assert_eq!(d[0].n_cases, 100);
        assert!((d[0].avg_cost_usd - 0.0038).abs() < 1e-9);
        // Case-weighted variance across the two run means: 0.1·(0.6-0.87)² + 0.9·(0.9-0.87)² = 0.0081.
        let v = d[0].quality_variance.expect("two runs → variance known");
        assert!((v - 0.0081).abs() < 1e-6, "got {v}");
    }

    #[test]
    fn merge_counts_distinct_contributors() {
        let rows = merge_leaderboard(
            &[
                entry("a", "sonnet", 0.8, 50, None),
                entry("b", "sonnet", 0.9, 50, None),
                entry("a", "haiku", 0.7, 20, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows[0].model, "sonnet");
        assert!((rows[0].quality - 0.85).abs() < 1e-9);
        assert_eq!(rows[0].n_contributors, 2);
        assert_eq!(rows[0].n_cases, 100);
        let haiku = rows.iter().find(|r| r.model == "haiku").unwrap();
        assert_eq!(haiku.n_contributors, 1);
    }

    #[test]
    fn leaderboard_sorted_quality_desc_and_surfaces_p95() {
        let rows = merge_leaderboard(
            &[
                entry("a", "low", 0.3, 50, None),
                entry("a", "high", 0.95, 50, None),
                entry("a", "mid", 0.6, 50, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        let order: Vec<&str> = rows.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(order, ["high", "mid", "low"]);
        // p95 is now surfaced (worst-observed across contributors).
        assert_eq!(rows[0].p95_latency_ms, Some(2000));
    }

    #[test]
    fn v1_null_variance_yields_no_ci() {
        // Every contributor is v1 (variance None) → no CI can be formed (insufficient variance data).
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.8, 100, None),
                entry("b", "haiku", 0.82, 100, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows.len(), 1);
        assert!(
            rows[0].quality_ci95.is_none(),
            "no variance anywhere → CI None"
        );
        assert!(!rows[0].low_confidence, "200 cases clears the floor");
    }

    #[test]
    fn ci_formed_when_variance_covers_enough_weight() {
        // Two sources, 100 cases each, variance 0.04 each → coverage 1.0. Check the arithmetic by hand:
        //   within:  V = 0.04, N_known = 200 ⇒ SE_within² = 0.04/200 = 0.0002
        //   between: p = 0.5 each, q̄ = 0.82, raw τ² = 0.0004, ×k/(k−1)=2 ⇒ τ̂² = 0.0008,
        //            Σp² = 0.5 ⇒ SE_between² = 0.0004
        //   total:   SE = √0.0006 = 0.0244949 ⇒ CI = 1.96·SE = 0.0480100…, rounded 0.048.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.80, 100, Some(0.04)),
                entry("b", "haiku", 0.84, 100, Some(0.04)),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        let ci = rows[0].quality_ci95.expect("full coverage → CI known");
        assert_eq!(ci, 0.048, "the whole interval, hand-checkable");
        // …and the disagreement that widened it is on the row: SD = √0.0008 = 0.0283 → 0.028.
        assert_eq!(rows[0].source_spread, Some(0.028));
    }

    #[test]
    fn disagreeing_sources_get_a_wider_interval_than_agreeing_ones() {
        // THE BUG THIS FIXES: with fixed-effect pooling both of these rows got the SAME interval,
        // because only the case counts entered it. Every input below is identical except the sources'
        // mean qualities.
        let ci_of = |qa: f64, qb: f64| {
            let rows = merge_leaderboard(
                &[
                    entry("a", "haiku", qa, 100, Some(0.04)),
                    entry("b", "haiku", qb, 100, Some(0.04)),
                ],
                DEFAULT_LOW_CONFIDENCE_CASES,
            );
            (
                rows[0].quality_ci95.unwrap(),
                rows[0].source_spread.unwrap(),
            )
        };
        // Perfect agreement ⇒ τ̂² = 0 ⇒ the interval is exactly the old within-source one:
        //   SE = √(0.04/200) = 0.0141421 ⇒ CI = 0.0277186 → 0.028.
        let (agree, spread_agree) = ci_of(0.82, 0.82);
        assert_eq!(agree, 0.028);
        assert_eq!(spread_agree, 0.0, "no disagreement to show");
        // A 0.24-wide gap ⇒ raw τ² = 0.0144, τ̂² = 0.0288, SE_between² = 0.0144;
        //   SE = √(0.0002 + 0.0144) = 0.1208305 ⇒ CI = 0.2368277 → 0.237.
        let (disagree, spread_disagree) = ci_of(0.70, 0.94);
        assert_eq!(disagree, 0.237);
        assert_eq!(spread_disagree, 0.17, "√0.0288 = 0.169705…");
        assert!(
            disagree > agree * 8.0,
            "disagreement dominates the interval, as it should"
        );
        // The middling gap sits between them — monotone in the disagreement, not in the case count.
        let (mid, _) = ci_of(0.78, 0.86);
        assert!(
            agree < mid && mid < disagree,
            "agree {agree} < mid {mid} < disagree {disagree}"
        );
    }

    #[test]
    fn disagreement_is_visible_even_when_no_ci_can_be_formed() {
        // Two v1 contributors: no variance anywhere, so the refusal to fabricate a CI stands — but the
        // reader can still see that the sources are 0.4 apart. Before, the row said nothing at all.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.60, 100, None),
                entry("b", "haiku", 1.00, 100, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            rows[0].quality_ci95.is_none(),
            "the variance-coverage floor is untouched"
        );
        // raw τ² = 0.04, τ̂² = 0.08 ⇒ SD = 0.2828 → 0.283.
        assert_eq!(rows[0].source_spread, Some(0.283));
        // A single-source row has no between-source evidence — that is `None`, not "they all agree".
        let rows = merge_leaderboard(
            &[entry("solo", "haiku", 0.9, 500, Some(0.04))],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            rows[0].source_spread.is_none(),
            "k=1 spread is undefined, not zero"
        );
        // …and its interval is the within-source one alone: SE = √(0.04/500) ⇒ CI = 1.96·0.0089443.
        assert_eq!(rows[0].quality_ci95, Some(0.018));
    }

    #[test]
    fn uncertainty_never_reorders_the_leaderboard() {
        // `high` disagrees violently (huge interval), `low` is unanimous. Ranking is still by the point
        // estimate — `low_confidence` and the interval stay annotations, per the existing stance.
        let rows = merge_leaderboard(
            &[
                entry("a", "high", 0.60, 100, Some(0.04)),
                entry("b", "high", 1.00, 100, Some(0.04)),
                entry("a", "low", 0.70, 100, Some(0.04)),
                entry("b", "low", 0.70, 100, Some(0.04)),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        let order: Vec<&str> = rows.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(order, ["high", "low"], "0.80 still outranks 0.70");
        assert!(rows[0].quality_ci95.unwrap() > rows[1].quality_ci95.unwrap());
    }

    #[test]
    fn ci_none_when_variance_coverage_too_thin() {
        // Only 40 of 200 cases (20%) carry a known variance → below the 50% floor → CI None.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.80, 160, None),
                entry("b", "haiku", 0.84, 40, Some(0.04)),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            rows[0].quality_ci95.is_none(),
            "thin variance coverage → no CI"
        );
    }

    #[test]
    fn digest_collapses_agreeing_judge_and_flags_mixed() {
        // Two runs, same judge provider → the digest entry keeps that provider.
        let mut a = stat("anthropic", "haiku", "qa", 0.8, 0.003, 50);
        a.judge_provider = Some("openai".into());
        let mut b = stat("anthropic", "haiku", "qa", 0.9, 0.003, 50);
        b.judge_provider = Some("openai".into());
        let d = build_digest(&[a, b], 5);
        assert_eq!(d[0].judge_provider.as_deref(), Some("openai"));
        // Runs disagreeing on judge → collapsed to "mixed".
        let mut a = stat("anthropic", "haiku", "qa", 0.8, 0.003, 50);
        a.judge_provider = Some("openai".into());
        let mut b = stat("anthropic", "haiku", "qa", 0.9, 0.003, 50);
        b.judge_provider = Some("google".into());
        let d = build_digest(&[a, b], 5);
        assert_eq!(d[0].judge_provider.as_deref(), Some("mixed"));
    }

    #[test]
    fn merge_annotates_mixed_judges_across_contributors() {
        // Same bucket judged by two different providers across contributors → mixed_judges = 2.
        let rows = merge_leaderboard(
            &[
                judged("a", "haiku", 0.8, 50, None, Some("anthropic")),
                judged("b", "haiku", 0.85, 50, None, Some("openai")),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows[0].mixed_judges, Some(2));
        assert_eq!(
            rows[0].judge_providers,
            vec!["anthropic".to_string(), "openai".to_string()]
        );
        // A single-judge bucket carries no mixed annotation.
        let rows = merge_leaderboard(
            &[judged("a", "haiku", 0.8, 50, None, Some("anthropic"))],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(rows[0].mixed_judges.is_none());
        assert_eq!(rows[0].judge_providers, vec!["anthropic".to_string()]);
        // A single source whose own bucket mixed judges is not a single-judge row: before, the one
        // `mixed` tag counted as one family and the row read as commensurable.
        let rows = merge_leaderboard(
            &[judged("a", "haiku", 0.8, 50, None, Some("mixed"))],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(
            rows[0].mixed_judges,
            Some(2),
            "mixed stands for at least two"
        );
        let rows = merge_leaderboard(
            &[
                judged("a", "haiku", 0.8, 50, None, Some("mixed")),
                judged("b", "haiku", 0.8, 50, None, Some("openai")),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows[0].mixed_judges, Some(3));
    }

    #[test]
    fn one_source_cannot_own_a_row_but_still_leads_it() {
        // 1M cases against two 100-case sources: flat pooling would hand the row to the big claim
        // (quality ≈ 0.05). Winsorized, its weight is capped at 4× the rest, so the row lands between
        // the two positions and discloses the share.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.80, 100, None),
                entry("b", "haiku", 0.82, 100, None),
                entry("whale", "haiku", 0.05, 1_000_000, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(
            rows[0].max_source_share, 0.8,
            "the documented ceiling is realized exactly"
        );
        // 0.8·0.05 + 0.1·0.80 + 0.1·0.82 = 0.202.
        assert!(
            (rows[0].quality - 0.202).abs() < 1e-3,
            "got {}",
            rows[0].quality
        );
        assert_eq!(
            rows[0].n_cases, 1_000_200,
            "raw evidence volume is reported truthfully"
        );
        assert_eq!(rows[0].n_contributors, 3);
    }

    #[test]
    fn sample_size_still_matters_and_honest_rows_are_untouched() {
        // The non-goal: 10k cases must still beat 10 by a wide margin (share ceiling 0.8, not 0.5).
        let rows = merge_leaderboard(
            &[
                entry("big", "haiku", 0.90, 10_000, None),
                entry("small", "haiku", 0.10, 10, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            (rows[0].quality - 0.74).abs() < 1e-3,
            "got {}",
            rows[0].quality
        );
        assert_eq!(rows[0].max_source_share, 0.8);
        // Sources within 4× of each other are never touched: exactly the flat case-weighted mean.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.60, 25, None),
                entry("b", "haiku", 0.90, 75, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            (rows[0].quality - 0.825).abs() < 1e-9,
            "got {}",
            rows[0].quality
        );
        assert_eq!(rows[0].max_source_share, 0.75, "no winsorization applied");
        // Beyond 4×, the ceiling bites — 90/10 becomes 80/20. That is the bound doing its job: a row
        // that is 90% one instance is that instance's private eval on a collective billboard.
        let rows = merge_leaderboard(
            &[
                entry("a", "haiku", 0.60, 10, None),
                entry("b", "haiku", 0.90, 90, None),
            ],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert!(
            (rows[0].quality - 0.84).abs() < 1e-9,
            "got {}",
            rows[0].quality
        );
        assert_eq!(rows[0].max_source_share, 0.8);
        // Within ONE contributor, sample size is still respected exactly — the digest pools its own
        // runs with no ceiling at all (10 cases at 0.6 + 90 at 0.9 ⇒ 0.87).
        let d = build_digest(
            &[
                stat("anthropic", "haiku", "qa", 0.6, 0.002, 10),
                stat("anthropic", "haiku", "qa", 0.9, 0.004, 90),
            ],
            5,
        );
        assert!((d[0].quality - 0.87).abs() < 1e-9, "got {}", d[0].quality);
        // A single-source row is left alone entirely (the contributor floor decides its fate).
        let rows = merge_leaderboard(
            &[entry("solo", "haiku", 0.5, 5000, None)],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows[0].max_source_share, 1.0);
        assert_eq!(rows[0].n_cases, 5000);
    }

    #[test]
    fn digest_rigor_folds_to_the_weakest_claim() {
        let rigorous = |q: f64| {
            let mut s = stat("anthropic", "haiku", "qa", q, 0.003, 50);
            s.determinism = Some("exact".into());
            s.dataset_frozen = Some(true);
            s.dataset_version = Some(3);
            s.significance_tested = Some(true);
            s
        };
        // Two equally rigorous runs → the bucket keeps the full claim.
        let d = build_digest(&[rigorous(0.8), rigorous(0.9)], 5);
        assert_eq!(d[0].determinism.as_deref(), Some("exact"));
        assert_eq!(d[0].frozen_dataset, Coverage::All);
        assert_eq!(d[0].significance_tested, Coverage::All);
        // One sloppy run drags every facet down — a bucket is only as good as its worst run.
        let mut sloppy = rigorous(0.9);
        sloppy.determinism = Some("sampled".into());
        sloppy.dataset_frozen = Some(false);
        sloppy.significance_tested = None;
        let d = build_digest(&[rigorous(0.8), sloppy], 5);
        assert_eq!(d[0].determinism.as_deref(), Some("sampled"));
        assert_eq!(d[0].frozen_dataset, Coverage::Mixed);
        assert_eq!(
            d[0].significance_tested,
            Coverage::Mixed,
            "silence is not agreement"
        );
        // Two *versions* of a frozen dataset are two case sets: the "one immutable pin" claim fails
        // even though every run reported frozen=true. The version integers never leave the instance.
        let mut v4 = rigorous(0.9);
        v4.dataset_version = Some(4);
        let d = build_digest(&[rigorous(0.8), v4], 5);
        assert_eq!(
            d[0].frozen_dataset,
            Coverage::Mixed,
            "version drift breaks the pin claim"
        );
        // …and the version itself is nowhere on the wire.
        let json = serde_json::to_string(&d[0]).unwrap();
        assert!(
            !json.contains("version"),
            "dataset version must not be published: {json}"
        );
    }

    #[test]
    fn merged_row_discloses_mixed_rigor_and_v2_entries_still_merge() {
        let rigorous = |c: &str| {
            let mut e = entry(c, "haiku", 0.8, 100, None);
            e.determinism = Some("exact".into());
            e.frozen_dataset = Coverage::All;
            e.significance_tested = Coverage::All;
            e
        };
        let rows = merge_leaderboard(
            &[rigorous("a"), rigorous("b")],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(rows[0].rigor.determinism.as_deref(), Some("exact"));
        assert_eq!(rows[0].rigor.determinism_levels, vec!["exact".to_string()]);
        assert_eq!(rows[0].rigor.frozen_dataset, Coverage::All);
        assert!(!rows[0].mixed_rigor);
        // A sloppy contributor is disclosed, not averaged into a flattering single label.
        let mut sloppy = rigorous("c");
        sloppy.determinism = Some("sampled".into());
        sloppy.frozen_dataset = Coverage::None;
        let rows = merge_leaderboard(&[rigorous("a"), sloppy], DEFAULT_LOW_CONFIDENCE_CASES);
        assert_eq!(
            rows[0].rigor.determinism.as_deref(),
            Some("sampled"),
            "headline is the weakest"
        );
        assert_eq!(
            rows[0].rigor.determinism_levels,
            vec!["exact".to_string(), "sampled".into()]
        );
        assert_eq!(rows[0].rigor.frozen_dataset, Coverage::Mixed);
        assert!(rows[0].mixed_rigor);
        // A v1/v2 contributor (no rigor at all) merges as Unknown — never orphaned by the v3 bump.
        let rows = merge_leaderboard(
            &[rigorous("a"), entry("legacy", "haiku", 0.8, 100, None)],
            DEFAULT_LOW_CONFIDENCE_CASES,
        );
        assert_eq!(
            rows[0].n_contributors, 2,
            "the v2 contribution still counts"
        );
        assert!(
            rows[0].rigor.determinism.is_none(),
            "one silent source voids the claim"
        );
        assert_eq!(rows[0].rigor.frozen_dataset, Coverage::Mixed);
    }

    #[test]
    fn low_confidence_flags_thin_rows_without_hiding_them() {
        let rows = merge_leaderboard(&[entry("a", "haiku", 0.9, 12, None)], 30);
        assert_eq!(rows.len(), 1, "thin row is shown, not hidden");
        assert!(rows[0].low_confidence, "12 < 30 → flagged");
        // A fat row is not flagged.
        let rows = merge_leaderboard(&[entry("a", "haiku", 0.9, 500, None)], 30);
        assert!(!rows[0].low_confidence);
    }
}
