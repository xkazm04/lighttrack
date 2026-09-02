//! Eval-corpus lineage (M24): how a dataset version is sampled, and where its cases come from.
//!
//! `Dataset::version` was a constant `1` for the life of the product — `create_dataset` wrote it and
//! nothing ever updated it — which quietly hollowed out two guards that read it: the paired-test
//! refusal in the runner's history module compared `1` with `1`, and a run's `dataset_pin` recorded
//! `1` forever. Freezing was terminal, so the only way to extend a golden set was to build a
//! *different* set, which is exactly the thing those guards exist to notice.
//!
//! Two types close that. A **fork** is the write path: version `n+1` of the same name, items copied,
//! unfrozen, `parent_id` pointing back — so a frozen set is a checkpoint rather than a dead end. An
//! [`ImportSpec`] is the read path: which rows become cases, chosen by a declared
//! [`SamplingStrategy`] rather than by the single hard-coded "newest N events with an input" that
//! `docs/BENCHMARK_FRAMEWORK.md` §1 has promised four of since it was written.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::Status;

/// How to choose `n` rows out of the rows a filter matched.
///
/// The four are not interchangeable and the difference is the whole point: `Recent` is the cheapest
/// and the most biased (a corpus of whatever happened last week), `Random` is the only one that
/// estimates the population, `Stratified` is the only one that keeps a rare model or a rare failure
/// mode from vanishing under the volume of the common one, and `Errors` deliberately abandons
/// representativeness to build a regression set out of what went wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SamplingStrategy {
    /// Newest first — the historical behaviour, kept as the default so an unspecified import
    /// samples exactly what it always did.
    #[default]
    Recent,
    /// Uniform over the matched rows.
    Random,
    /// A per-`(model, status)` quota, so a low-volume model is represented rather than drowned.
    Stratified,
    /// Failures only: `status <> 'success'` on the event side, `pass = false` on the score side.
    Errors,
}

impl SamplingStrategy {
    pub fn as_str(self) -> &'static str {
        match self {
            SamplingStrategy::Recent => "recent",
            SamplingStrategy::Random => "random",
            SamplingStrategy::Stratified => "stratified",
            SamplingStrategy::Errors => "errors",
        }
    }

    /// Parse a CLI/query spelling. `None` for anything else — the caller decides whether that is a
    /// 400 or a fallback, rather than this silently sampling `recent` when the operator asked for
    /// `stratified` and would read the resulting corpus as stratified.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "recent" => Some(SamplingStrategy::Recent),
            "random" => Some(SamplingStrategy::Random),
            "stratified" => Some(SamplingStrategy::Stratified),
            "errors" | "errors-only" | "errors_only" => Some(SamplingStrategy::Errors),
            _ => None,
        }
    }
}

/// Which table the cases are mined from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportSource {
    /// Production traffic. The strongest source, because cases a model has never seen cannot have
    /// leaked into its training set (see the note on [`crate::Dataset`]).
    #[default]
    Events,
    /// Judged traffic — the join that makes failure-mined regression sets possible at all, since a
    /// `pass = false` verdict lives on `scores` and the text lives on `events`.
    Scores,
}

impl ImportSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ImportSource::Events => "events",
            ImportSource::Scores => "scores",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "events" => Some(ImportSource::Events),
            "scores" => Some(ImportSource::Scores),
            _ => None,
        }
    }
}

/// What narrows the candidate rows. Every field is `None` by default and AND-combined.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportFilter {
    /// Only meaningful with [`ImportSource::Scores`]: `Some(false)` is the failure-mining question.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
}

/// One import request: where the cases come from, which ones, how many, and whether near-duplicates
/// collapse.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ImportSpec {
    #[serde(default)]
    pub from: ImportSource,
    #[serde(default)]
    pub filter: ImportFilter,
    #[serde(default)]
    pub strategy: SamplingStrategy,
    #[serde(default = "default_n")]
    pub n: usize,
    /// Collapse cases whose *normalised* input already appears in the target set
    /// ([`input_fingerprint`]). Off by default: silently dropping a case an operator asked for is
    /// worse than an over-full corpus unless they said otherwise.
    #[serde(default)]
    pub dedupe: bool,
    /// An explicit set of source events, bypassing the filter/strategy entirely.
    ///
    /// This is how a failing online verdict becomes a regression case: the scorer already knows the
    /// one event id, so re-deriving it from a filter would be a query that could match something
    /// else. Non-empty means "import exactly these", still subject to `dedupe` and the frozen check.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub event_ids: Vec<String>,
}

fn default_n() -> usize {
    50
}

/// Hard ceiling on one import, so a mis-typed `n` cannot copy a whole event table into a dataset.
pub const MAX_IMPORT_N: usize = 5_000;

impl ImportSpec {
    /// The bounded page size to actually scan/insert.
    pub fn effective_n(&self) -> usize {
        match self.n {
            0 => default_n(),
            n => n.min(MAX_IMPORT_N),
        }
    }

    /// `true` when this spec names its rows outright rather than describing them.
    pub fn is_explicit(&self) -> bool {
        !self.event_ids.is_empty()
    }

    /// The spec a failing online verdict enqueues for one event.
    pub fn for_event(event_id: &str) -> Self {
        ImportSpec {
            from: ImportSource::Scores,
            filter: ImportFilter {
                pass: Some(false),
                ..Default::default()
            },
            strategy: SamplingStrategy::Errors,
            n: 1,
            dedupe: true,
            event_ids: vec![event_id.to_string()],
        }
    }

    /// The `source` string a dataset built by this spec records — provenance in the shape
    /// [`crate::Dataset::source`] already uses (`events:recent`).
    pub fn source_tag(&self) -> String {
        format!("{}:{}", self.from.as_str(), self.strategy.as_str())
    }
}

/// The normalised form two "near-duplicate" inputs share: case-folded, whitespace-collapsed,
/// trimmed.
///
/// Deliberately *not* an embedding or an edit distance. A stored fingerprint has to be computable in
/// SQL on both backends and stable across processes, and the duplicates a sampled corpus actually
/// accumulates are re-sends of the same prompt with different spacing — not paraphrases. A cheap,
/// exact, explainable rule that catches those beats an approximate one no operator can predict.
pub fn normalize_input(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.to_lowercase())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A stable hex fingerprint of [`normalize_input`], stored beside a dataset item so dedupe is an
/// index lookup rather than a scan of every stored case's text.
pub fn input_fingerprint(s: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(normalize_input(s).as_bytes());
    format!("{:x}", h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategies_round_trip_through_their_wire_spelling() {
        for s in [
            SamplingStrategy::Recent,
            SamplingStrategy::Random,
            SamplingStrategy::Stratified,
            SamplingStrategy::Errors,
        ] {
            assert_eq!(SamplingStrategy::parse(s.as_str()), Some(s));
        }
        assert_eq!(
            SamplingStrategy::parse("errors-only"),
            Some(SamplingStrategy::Errors)
        );
        assert_eq!(
            SamplingStrategy::parse("newest"),
            None,
            "no silent fallback"
        );
        assert_eq!(ImportSource::parse("scores"), Some(ImportSource::Scores));
        assert_eq!(ImportSource::parse("labels"), None);
    }

    /// A default spec must sample exactly what the pre-M24 builder sampled, or upgrading changes
    /// what every existing caller's corpus contains without anyone asking for it.
    #[test]
    fn the_default_spec_is_the_historical_behaviour() {
        let spec = ImportSpec::default();
        assert_eq!(spec.from, ImportSource::Events);
        assert_eq!(spec.strategy, SamplingStrategy::Recent);
        assert!(!spec.dedupe);
        assert!(!spec.is_explicit());
        assert_eq!(spec.source_tag(), "events:recent");
    }

    #[test]
    fn n_is_defaulted_and_bounded() {
        assert_eq!(
            ImportSpec {
                n: 0,
                ..Default::default()
            }
            .effective_n(),
            50
        );
        assert_eq!(
            ImportSpec {
                n: MAX_IMPORT_N * 10,
                ..Default::default()
            }
            .effective_n(),
            MAX_IMPORT_N
        );
        assert_eq!(
            ImportSpec {
                n: 7,
                ..Default::default()
            }
            .effective_n(),
            7
        );
    }

    #[test]
    fn a_spec_deserializes_from_the_smallest_useful_json() {
        let spec: ImportSpec = serde_json::from_str(r#"{"from":"scores","filter":{"pass":false}}"#)
            .expect("minimal spec");
        assert_eq!(spec.from, ImportSource::Scores);
        assert_eq!(spec.filter.pass, Some(false));
        assert_eq!(spec.n, 50, "an unspecified n is the default, not zero");
    }

    /// The near-duplicates a sampled corpus actually accumulates: same prompt, different spacing or
    /// case. Two genuinely different questions must not collide.
    #[test]
    fn the_fingerprint_folds_spacing_and_case_but_not_meaning() {
        assert_eq!(
            input_fingerprint("Summarise   THIS\n order"),
            input_fingerprint("summarise this order")
        );
        assert_ne!(
            input_fingerprint("summarise this order"),
            input_fingerprint("summarise that order")
        );
        assert_eq!(normalize_input("  A  b  "), "a b");
    }

    #[test]
    fn an_event_spec_names_exactly_one_row() {
        let spec = ImportSpec::for_event("ev1");
        assert!(spec.is_explicit());
        assert_eq!(spec.event_ids, vec!["ev1".to_string()]);
        assert_eq!(spec.filter.pass, Some(false));
        assert!(
            spec.dedupe,
            "a regression set must not accumulate one case twice"
        );
    }
}
