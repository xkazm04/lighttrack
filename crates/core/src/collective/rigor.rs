//! Benchmark **rigor** — the "how much should I trust this number" metadata that rides a digest entry
//! and its merged leaderboard row.
//!
//! A pinned, exact-determinism run against a frozen dataset whose verdict was significance-tested is
//! not the same evidence as a sampled run against a mutable dataset, and a leaderboard that merges
//! them as equals is lying by omission. Three facets travel:
//!   - **determinism** — the run's weakest reproducibility stamp (`exact` | `best-effort` | `sampled`,
//!     the vocabulary the runner already stamps into a run report);
//!   - **frozen dataset** — whether the cases were immutable *and* pinned at one version;
//!   - **significance-tested** — whether the run's verdict carried an interval (`n ≥ 2` + a `ci95`),
//!     rather than a bare point estimate compared to a baseline.
//!
//! **Why the fields are shaped like this (the fingerprinting argument).** A unique rigor combination is
//! as identifying as a unique task, so rigor is deliberately built from *closed, tiny vocabularies*:
//! three determinism levels and a four-state coverage tag, canon-clamped at ingest so no contributor
//! can inject a free-form label. In particular the dataset **version integer never leaves the
//! instance** — "v37" says nothing to a reader (my v3 and your v3 are different datasets) while being a
//! sharp per-contributor fingerprint, so it is consumed here and published only as its one useful
//! consequence: whether the bucket's runs all sat on one immutable pin. The merge-side floors do the
//! rest: rigor is aggregated across sources before publication, and the hub's `min_contributors`
//! k-anonymity floor is applied *before* any rigor filter, exactly as it is for `?provider=`, so no
//! filter combination can strip a row down to a lone source.

use serde::{Deserialize, Serialize};

/// The determinism vocabulary, **strongest first**. Anything outside it is "not recorded" (`None`) —
/// never a fourth level, which would only add cardinality to the fingerprint surface.
pub const DETERMINISM_LEVELS: &[&str] = &["exact", "best-effort", "sampled"];

/// Clamp a contributed determinism stamp to [`DETERMINISM_LEVELS`]; anything else ⇒ `None`.
pub fn canon_determinism(s: &str) -> Option<String> {
    let s = s.trim().to_lowercase();
    DETERMINISM_LEVELS.iter().find(|l| **l == s).map(|l| l.to_string())
}

/// Rank, strongest first. Unknown labels sort weakest so a fold can never *strengthen* a claim.
fn rank(s: &str) -> u8 {
    match s {
        "exact" => 2,
        "best-effort" => 1,
        _ => 0,
    }
}

/// Fold two determinism stamps to the **weaker** one — a set of runs is only as reproducible as its
/// least reproducible member. `None` (unrecorded) absorbs: an unrecorded run cannot vouch for the rest.
pub fn weakest_determinism(a: Option<&str>, b: Option<&str>) -> Option<String> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if rank(a) <= rank(b) { a } else { b }.to_string()),
        _ => None,
    }
}

/// How uniformly a boolean rigor fact holds across the runs/sources behind a bucket or row.
///
/// `All` / `None` are **complete** claims: every contributor recorded the fact and they agree. Anything
/// short of that is `Mixed` — including "they agree but somebody didn't say", because a claim resting
/// on silence is not a claim. `Unknown` is nobody said anything at all.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Coverage {
    /// Every contributing run/source recorded the fact as true.
    All,
    /// Recorded values disagree, or some were recorded and some were not.
    Mixed,
    /// Every contributing run/source recorded the fact as false.
    None,
    /// Nothing recorded it.
    #[default]
    Unknown,
}

impl Coverage {
    pub fn is_unknown(&self) -> bool {
        *self == Coverage::Unknown
    }

    /// A single observation: `Some(true)`/`Some(false)` recorded, `None` unrecorded.
    pub fn of(flag: Option<bool>) -> Coverage {
        match flag {
            Some(true) => Coverage::All,
            Some(false) => Coverage::None,
            _ => Coverage::Unknown,
        }
    }

    /// Combine two coverages. Agreement survives; anything else degrades to `Mixed`, so a merged row
    /// can never claim a rigor level that only part of its evidence supports.
    pub fn fold(self, other: Coverage) -> Coverage {
        if self == other {
            self
        } else {
            Coverage::Mixed
        }
    }

    /// Parse a stored/contributed tag; anything unrecognized is `Unknown`.
    pub fn from_tag(s: &str) -> Coverage {
        match s.trim().to_lowercase().as_str() {
            "all" => Coverage::All,
            "mixed" => Coverage::Mixed,
            "none" => Coverage::None,
            _ => Coverage::Unknown,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Coverage::All => "all",
            Coverage::Mixed => "mixed",
            Coverage::None => "none",
            Coverage::Unknown => "unknown",
        }
    }

    /// The storage form: `None` for `Unknown`, so a v1/v2 row (a SQL NULL) reads back as `Unknown`
    /// without a backfill.
    pub fn to_tag(self) -> Option<String> {
        (!self.is_unknown()).then(|| self.as_str().to_string())
    }
}

/// The rigor a merged leaderboard row aggregates. Mixture is **disclosed**, never averaged into a
/// single flattering label: `determinism` is the weakest stamp behind the row and `determinism_levels`
/// lists every distinct stamp that went into it, so `exact` + `["exact","sampled"]` reads as "one of
/// these contributors sampled".
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct RowRigor {
    /// Weakest determinism stamp across the row's sources; `None` when no source recorded one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub determinism: Option<String>,
    /// Every distinct determinism stamp behind the row, sorted strongest-first. Length > 1 ⇒ the row
    /// mixes reproducibility levels.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub determinism_levels: Vec<String>,
    /// Whether the row's evidence ran against frozen, single-version datasets.
    pub frozen_dataset: Coverage,
    /// Whether the row's evidence carried significance-tested verdicts.
    pub significance_tested: Coverage,
}

impl RowRigor {
    /// `true` when the row's sources disagree on any facet — the honest headline for "this row mixes
    /// rigorous and sloppy evidence".
    pub fn is_mixed(&self) -> bool {
        self.determinism_levels.len() > 1
            || self.frozen_dataset == Coverage::Mixed
            || self.significance_tested == Coverage::Mixed
    }
}

/// Sort determinism stamps strongest-first for stable output.
pub(crate) fn sort_levels(levels: &mut [String]) {
    levels.sort_by(|a, b| rank(b).cmp(&rank(a)).then_with(|| a.cmp(b)));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn determinism_folds_to_the_weakest_and_silence_absorbs() {
        assert_eq!(weakest_determinism(Some("exact"), Some("sampled")).as_deref(), Some("sampled"));
        assert_eq!(
            weakest_determinism(Some("exact"), Some("best-effort")).as_deref(),
            Some("best-effort")
        );
        assert_eq!(weakest_determinism(Some("exact"), Some("exact")).as_deref(), Some("exact"));
        // An unrecorded run cannot vouch for the recorded ones.
        assert!(weakest_determinism(Some("exact"), None).is_none());
        assert!(weakest_determinism(None, None).is_none());
    }

    #[test]
    fn unknown_labels_never_become_a_level() {
        assert_eq!(canon_determinism(" Exact ").as_deref(), Some("exact"));
        assert_eq!(canon_determinism("best-effort").as_deref(), Some("best-effort"));
        assert!(canon_determinism("perfectly-reproducible").is_none());
        assert!(canon_determinism("").is_none());
    }

    #[test]
    fn coverage_degrades_on_any_disagreement_or_silence() {
        assert_eq!(Coverage::of(Some(true)).fold(Coverage::of(Some(true))), Coverage::All);
        assert_eq!(Coverage::of(Some(false)).fold(Coverage::of(Some(false))), Coverage::None);
        assert_eq!(Coverage::of(Some(true)).fold(Coverage::of(Some(false))), Coverage::Mixed);
        // Agreement resting on silence is not agreement.
        assert_eq!(Coverage::All.fold(Coverage::Unknown), Coverage::Mixed);
        assert_eq!(Coverage::Unknown.fold(Coverage::Unknown), Coverage::Unknown);
        assert_eq!(Coverage::Mixed.fold(Coverage::All), Coverage::Mixed);
    }

    #[test]
    fn coverage_round_trips_through_its_storage_tag() {
        for c in [Coverage::All, Coverage::Mixed, Coverage::None] {
            assert_eq!(Coverage::from_tag(&c.to_tag().unwrap()), c);
        }
        // Unknown stores as NULL and reads back as Unknown — no backfill for v1/v2 rows.
        assert!(Coverage::Unknown.to_tag().is_none());
        assert_eq!(Coverage::from_tag("whatever-the-poster-typed"), Coverage::Unknown);
    }

    #[test]
    fn row_rigor_reports_mixture() {
        let uniform = RowRigor {
            determinism: Some("exact".into()),
            determinism_levels: vec!["exact".into()],
            frozen_dataset: Coverage::All,
            significance_tested: Coverage::All,
        };
        assert!(!uniform.is_mixed());
        let mixed = RowRigor {
            determinism_levels: vec!["exact".into(), "sampled".into()],
            ..uniform.clone()
        };
        assert!(mixed.is_mixed());
        assert!(RowRigor { frozen_dataset: Coverage::Mixed, ..uniform }.is_mixed());
    }

    #[test]
    fn levels_sort_strongest_first() {
        let mut l = vec!["sampled".to_string(), "exact".into(), "best-effort".into()];
        sort_levels(&mut l);
        assert_eq!(l, ["exact", "best-effort", "sampled"]);
    }
}
