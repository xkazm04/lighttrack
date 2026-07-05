//! Collective Model Intelligence — privacy-safe aggregation of benchmark results into a shareable
//! model leaderboard.
//!
//! The network effect: every LightTrack instance runs benchmarks on *its own real tasks*. This module
//! turns those runs into a **privacy-safe digest** — aggregate `(provider, model, task_type)` →
//! quality / cost / latency, carrying **no raw text, no project ids, no customer data**, only public
//! model identities and aggregate numbers. Instances opt in to contribute their digest to a shared hub
//! (another LightTrack acting as the public leaderboard); the hub merges contributions so every
//! operator sees real-world model performance instead of vendor benchmarks.
//!
//! Two privacy guarantees are enforced here, in pure code, so they hold for every backend:
//!   1. **Aggregate-only inputs.** A digest is built from benchmark *run scorecards* ([`RunStat`]),
//!      which already carry no prompt/response text — we never touch `events`.
//!   2. **k-anonymity.** A `(provider, model, task_type)` bucket is published only when it aggregates
//!      at least `min_cases` cases, so a rare/unique task can't be fingerprinted to one operator.
//!
//! The coarse `task_type` is always one of a fixed vocabulary ([`task_type_from`]); a custom benchmark
//! name is classified into a bucket, never published verbatim.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current digest wire-format version. Bump when [`ModelDigestEntry`] changes shape.
pub const DIGEST_SCHEMA_VERSION: u32 = 1;

/// Default k-anonymity floor: a bucket needs at least this many cases to be published.
pub const DEFAULT_MIN_CASES: u32 = 5;

/// Contributor id used when the operator sets no stable id.
pub const ANON_CONTRIBUTOR: &str = "anonymous";

/// The fixed task-type vocabulary a benchmark is classified into. Publishing only these labels (never
/// a raw benchmark name) keeps the digest from leaking project-specific naming.
pub const TASK_TYPES: &[&str] = &[
    "summarization",
    "qa",
    "extraction",
    "classification",
    "translation",
    "coding",
    "reasoning",
    "rag",
    "generation",
    "general",
];

/// One benchmark run reduced to the fields a digest needs. Built by the API from a `(Benchmark,
/// BenchmarkRun)` pair — only aggregate numbers + the (public) model identity + a coarse task-type
/// bucket, never any case text.
#[derive(Debug, Clone)]
pub struct RunStat {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    /// Mean quality score, normalized 0..1.
    pub quality: f64,
    /// Fraction of cases that passed, 0..1.
    pub pass_rate: f64,
    /// Cost per case in USD (generation + judge).
    pub cost_per_case_usd: f64,
    pub n_cases: u32,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
}

/// A published digest entry: one `(provider, model, task_type)` bucket aggregated across an instance's
/// runs. Purely aggregate — safe to share.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDigestEntry {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    pub pass_rate: f64,
    /// Mean cost per case, USD.
    pub avg_cost_usd: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_latency_ms: Option<u64>,
    pub n_runs: u32,
    pub n_cases: u32,
}

/// A full digest an instance contributes to a hub. The `contributor_id` is **opaque** (a hash, derived
/// in the API layer) so the hub can update a contributor's entries idempotently without learning who
/// it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveDigest {
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    #[serde(default = "anon_contributor")]
    pub contributor_id: String,
    #[serde(default = "Utc::now")]
    pub generated_at: DateTime<Utc>,
    /// The k-anonymity floor used to build this digest (for auditability).
    #[serde(default)]
    pub min_cases: u32,
    #[serde(default)]
    pub entries: Vec<ModelDigestEntry>,
}

/// A stored, hub-side digest entry: a [`ModelDigestEntry`] tagged with its contributor + receipt time.
/// This is what `collective_entries` persists; the merge reads it back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectiveEntry {
    pub contributor_id: String,
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    pub pass_rate: f64,
    pub avg_cost_usd: f64,
    pub p50_latency_ms: Option<u64>,
    pub p95_latency_ms: Option<u64>,
    pub n_runs: u32,
    pub n_cases: u32,
    pub received_at: DateTime<Utc>,
}

/// One row of the merged public leaderboard: a `(provider, model, task_type)` aggregated across all
/// contributors.
#[derive(Debug, Clone, Serialize)]
pub struct LeaderboardRow {
    pub provider: String,
    pub model: String,
    pub task_type: String,
    pub quality: f64,
    pub pass_rate: f64,
    pub avg_cost_usd: f64,
    pub p50_latency_ms: Option<u64>,
    pub n_contributors: u32,
    pub n_runs: u32,
    pub n_cases: u32,
}

fn default_schema_version() -> u32 {
    DIGEST_SCHEMA_VERSION
}
fn anon_contributor() -> String {
    ANON_CONTRIBUTOR.to_string()
}

/// Classify a benchmark `name` (with an optional explicit `hint`, e.g. a tag) into the fixed
/// [`TASK_TYPES`] vocabulary. Keyword match on the lowercased text; defaults to `general`. Always
/// returns one of [`TASK_TYPES`], so the published bucket never carries custom naming.
pub fn task_type_from(name: &str, hint: Option<&str>) -> String {
    let hay = format!("{} {}", name, hint.unwrap_or("")).to_lowercase();
    // Most specific → least; first hit wins.
    let table: &[(&str, &[&str])] = &[
        ("summarization", &["summ", "tldr", "abstract"]),
        ("translation", &["translat", "localiz", "i18n"]),
        ("extraction", &["extract", "parse", "ner", "entit"]),
        ("classification", &["classif", "categor", "intent", "sentiment", "moderation"]),
        ("coding", &["code", "coding", "program", "sql", "bug", "refactor"]),
        ("rag", &["rag", "retriev", "grounded", "citation"]),
        ("reasoning", &["reason", "math", "logic", "plan", "agent"]),
        ("qa", &["qa", "question", "answer", "faq", "support"]),
        ("generation", &["generat", "writ", "draft", "compose", "creative"]),
    ];
    for (label, keys) in table {
        if keys.iter().any(|k| hay.contains(k)) {
            return (*label).to_string();
        }
    }
    "general".to_string()
}

/// One case-weighted observation folded into an [`Acc`] — a run (digest) or a stored entry (merge).
struct Sample {
    quality: f64,
    pass_rate: f64,
    cost: f64,
    cases: u32,
    p50: Option<u64>,
    p95: Option<u64>,
    runs: u32,
}

/// Case-weighted accumulator shared by digest building and leaderboard merging.
#[derive(Default)]
struct Acc {
    cases: u64,
    q_w: f64,
    p_w: f64,
    c_w: f64,
    lat_cases: u64,
    lat_w: f64,
    p95_max: u64,
    runs: u32,
    contributors: BTreeSet<String>,
}

impl Acc {
    fn add(&mut self, s: Sample, contributor: Option<&str>) {
        let w = s.cases as f64;
        self.cases += s.cases as u64;
        self.q_w += s.quality * w;
        self.p_w += s.pass_rate * w;
        self.c_w += s.cost * w;
        self.runs += s.runs;
        if let Some(p) = s.p50 {
            self.lat_cases += s.cases as u64;
            self.lat_w += p as f64 * w;
        }
        if let Some(p) = s.p95 {
            self.p95_max = self.p95_max.max(p);
        }
        if let Some(c) = contributor {
            self.contributors.insert(c.to_string());
        }
    }

    fn quality(&self) -> f64 {
        if self.cases == 0 { 0.0 } else { self.q_w / self.cases as f64 }
    }
    fn pass_rate(&self) -> f64 {
        if self.cases == 0 { 0.0 } else { self.p_w / self.cases as f64 }
    }
    fn cost(&self) -> f64 {
        if self.cases == 0 { 0.0 } else { self.c_w / self.cases as f64 }
    }
    fn p50(&self) -> Option<u64> {
        (self.lat_cases > 0).then(|| (self.lat_w / self.lat_cases as f64).round() as u64)
    }
    fn p95(&self) -> Option<u64> {
        (self.p95_max > 0).then_some(self.p95_max)
    }
}

type Key = (String, String, String);

fn key_of(provider: &str, model: &str, task_type: &str) -> Key {
    (provider.to_string(), model.to_string(), task_type.to_string())
}

/// Build this instance's privacy-safe digest from its benchmark run scorecards. Buckets with fewer
/// than `min_cases` total cases are **dropped** (k-anonymity); the rest are sorted by quality desc.
pub fn build_digest(stats: &[RunStat], min_cases: u32) -> Vec<ModelDigestEntry> {
    let mut groups: BTreeMap<Key, Acc> = BTreeMap::new();
    for s in stats {
        if s.n_cases == 0 {
            continue;
        }
        groups.entry(key_of(&s.provider, &s.model, &s.task_type)).or_default().add(
            Sample {
                quality: s.quality,
                pass_rate: s.pass_rate,
                cost: s.cost_per_case_usd,
                cases: s.n_cases,
                p50: s.p50_latency_ms,
                p95: s.p95_latency_ms,
                runs: 1,
            },
            None,
        );
    }
    let mut out: Vec<ModelDigestEntry> = groups
        .into_iter()
        .filter(|(_, a)| a.cases >= min_cases as u64)
        .map(|((provider, model, task_type), a)| ModelDigestEntry {
            provider,
            model,
            task_type,
            quality: r3(a.quality()),
            pass_rate: r3(a.pass_rate()),
            avg_cost_usd: r6(a.cost()),
            p50_latency_ms: a.p50(),
            p95_latency_ms: a.p95(),
            n_runs: a.runs,
            n_cases: a.cases as u32,
        })
        .collect();
    sort_by_quality(&mut out, |e| (e.quality, &e.provider, &e.model));
    out
}

/// Merge stored contributions from many instances into the public leaderboard. Each
/// `(provider, model, task_type)` is case-weighted across contributors; `n_contributors` counts the
/// distinct sources. Sorted by quality desc.
pub fn merge_leaderboard(entries: &[CollectiveEntry]) -> Vec<LeaderboardRow> {
    let mut groups: BTreeMap<Key, Acc> = BTreeMap::new();
    for e in entries {
        groups.entry(key_of(&e.provider, &e.model, &e.task_type)).or_default().add(
            Sample {
                quality: e.quality,
                pass_rate: e.pass_rate,
                cost: e.avg_cost_usd,
                cases: e.n_cases,
                p50: e.p50_latency_ms,
                p95: e.p95_latency_ms,
                runs: e.n_runs,
            },
            Some(&e.contributor_id),
        );
    }
    let mut out: Vec<LeaderboardRow> = groups
        .into_iter()
        .map(|((provider, model, task_type), a)| LeaderboardRow {
            provider,
            model,
            task_type,
            quality: r3(a.quality()),
            pass_rate: r3(a.pass_rate()),
            avg_cost_usd: r6(a.cost()),
            p50_latency_ms: a.p50(),
            n_contributors: a.contributors.len() as u32,
            n_runs: a.runs,
            n_cases: a.cases as u32,
        })
        .collect();
    sort_by_quality(&mut out, |r| (r.quality, &r.provider, &r.model));
    out
}

/// Sort highest-quality first; ties broken by provider then model for stable output.
fn sort_by_quality<T, F>(v: &mut [T], key: F)
where
    F: Fn(&T) -> (f64, &String, &String),
{
    v.sort_by(|a, b| {
        let (qa, pa, ma) = key(a);
        let (qb, pb, mb) = key(b);
        qb.total_cmp(&qa).then_with(|| pa.cmp(pb)).then_with(|| ma.cmp(mb))
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
    use super::*;

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
        }
    }

    #[test]
    fn classifier_returns_fixed_vocabulary() {
        assert_eq!(task_type_from("Nightly summarization eval", None), "summarization");
        assert_eq!(task_type_from("SQL bug-fix bench", None), "coding");
        assert_eq!(task_type_from("Customer FAQ answering", None), "qa");
        assert_eq!(task_type_from("Grounded RAG citations", None), "rag");
        // Unknown → general, and always a member of the vocabulary.
        let t = task_type_from("widget-prod-xyz", None);
        assert_eq!(t, "general");
        assert!(TASK_TYPES.contains(&t.as_str()));
    }

    #[test]
    fn k_anonymity_drops_thin_buckets() {
        // 3 cases total for a (provider,model,task) below the floor of 5 → dropped.
        let d = build_digest(&[stat("openai", "gpt-x", "qa", 0.9, 0.01, 3)], 5);
        assert!(d.is_empty(), "thin bucket must be withheld");
        // Same bucket with 6 cases clears the floor.
        let d = build_digest(&[stat("openai", "gpt-x", "qa", 0.9, 0.01, 6)], 5);
        assert_eq!(d.len(), 1);
        assert_eq!(d[0].n_cases, 6);
        assert_eq!(d[0].n_runs, 1);
    }

    #[test]
    fn digest_is_case_weighted_across_runs() {
        // Two runs of the same model/task: 0.6 over 10 cases, 0.9 over 90 cases → weighted mean 0.87.
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
        // cost weighted: (0.002*10 + 0.004*90)/100 = 0.0038
        assert!((d[0].avg_cost_usd - 0.0038).abs() < 1e-9);
    }

    #[test]
    fn merge_counts_distinct_contributors() {
        let mk = |c: &str, model: &str, q: f64, cases: u32| CollectiveEntry {
            contributor_id: c.into(),
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
            received_at: Utc::now(),
        };
        let rows = merge_leaderboard(&[
            mk("a", "sonnet", 0.8, 50),
            mk("b", "sonnet", 0.9, 50),
            mk("a", "haiku", 0.7, 20),
        ]);
        // sonnet appears first (higher merged quality 0.85), from 2 contributors over 100 cases.
        assert_eq!(rows[0].model, "sonnet");
        assert!((rows[0].quality - 0.85).abs() < 1e-9);
        assert_eq!(rows[0].n_contributors, 2);
        assert_eq!(rows[0].n_cases, 100);
        // haiku from one contributor.
        let haiku = rows.iter().find(|r| r.model == "haiku").unwrap();
        assert_eq!(haiku.n_contributors, 1);
    }

    #[test]
    fn leaderboard_sorted_quality_desc() {
        let mk = |model: &str, q: f64| CollectiveEntry {
            contributor_id: "a".into(),
            provider: "p".into(),
            model: model.into(),
            task_type: "qa".into(),
            quality: q,
            pass_rate: q,
            avg_cost_usd: 0.001,
            p50_latency_ms: None,
            p95_latency_ms: None,
            n_runs: 1,
            n_cases: 10,
            received_at: Utc::now(),
        };
        let rows = merge_leaderboard(&[mk("low", 0.3), mk("high", 0.95), mk("mid", 0.6)]);
        let order: Vec<&str> = rows.iter().map(|r| r.model.as_str()).collect();
        assert_eq!(order, ["high", "mid", "low"]);
        // No latency reported anywhere → None, not a bogus 0.
        assert!(rows[0].p50_latency_ms.is_none());
    }
}
