//! The arithmetic behind `GET /v1/benchmarks/:id/regression` — **pure**, so it is unit-testable
//! without a router, a store or a judge.
//!
//! Deliberately **not** a `Store` method. Every input it needs (`list_run_scores`) is already
//! served by every backend, so folding the comparison here keeps SQLite and Postgres on one code
//! path: the parity gap a `score_deltas_by_dimension` trait method would have opened — one backend
//! implementing it and another silently defaulting — cannot exist for arithmetic that lives above
//! the trait.
//!
//! Two rules it will not bend:
//!
//! - **Voided is not zero.** `judge::aggregate` stores `0.0` as the placeholder for a dimension it
//!   could not measure and excludes it from the overall via `voided`. A delta against that
//!   placeholder is a fabricated −1.0 collapse, so a dimension scored on one side and voided on the
//!   other is reported as `coverage_changed` with both counts and **no** delta.
//! - **A family's zero carries its denominator.** A detector that could not fire reports
//!   `not_measured`, never `0` — the two readings call for opposite actions.

use std::collections::{BTreeMap, BTreeSet};

use lighttrack_core::Score;
use serde::Serialize;

/// How many of a dimension's reasoning strings travel with a flipped case. The judge's words are
/// the point of the row; the whole sample set is not, and a 500-case run must stay one response.
const MAX_REASONINGS_PER_FLIP: usize = 3;

/// Cross-sample agreement at or below this is the `low_agreement` family — the same constant
/// `scores_review` triages a single verdict with, so a family count and the review queue cannot
/// disagree about what a shaky verdict is.
const LOW_AGREEMENT: f64 = crate::scores_review::LOW_AGREEMENT;

/// One rubric dimension, compared across the two runs.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct DimDelta {
    pub key: String,
    /// `compared` — both sides measured it, `delta` is real.
    /// `coverage_changed` — one side never measured it (absent, or voided on every case), so there
    /// is nothing to subtract and no `delta` is reported.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_before: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_after: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<f64>,
    /// Observations that were actually measured (voided ones excluded) — the denominator each mean
    /// was taken over.
    pub n_before: usize,
    pub n_after: usize,
    pub floor_hits_before: usize,
    pub floor_hits_after: usize,
    pub voided_before: usize,
    pub voided_after: usize,
}

/// A case whose verdict changed sign, with the judge's own words for the dimension that moved most.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct FlippedCase {
    pub case_index: u32,
    /// The worst-dropping dimension of the run, when there is one — the dimension whose reasoning
    /// is most likely to say why.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dimension: Option<String>,
    /// That dimension's stored reasoning on the **new** run: what the judge said when it failed.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reasonings: Vec<String>,
}

/// One failure family's case count **with the denominator of cases that could have tripped it**.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct FamilyCount {
    pub family: &'static str,
    /// `measured` | `not_measured`. `not_measured` means `denominator == 0`: nothing in this run
    /// carried the signal, so `cases: 0` is the absence of an instrument, not the absence of a
    /// problem.
    pub status: &'static str,
    pub cases: usize,
    pub denominator: usize,
}

/// The whole comparison.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct RegressionDiff {
    /// `ok` — a per-dimension table follows.
    /// `no_dimensions` — neither run stored a dimensional verdict, named in `reason`.
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Worst-dropping dimension first, so the row an operator needs is the row they read.
    pub dimensions: Vec<DimDelta>,
    pub flipped: Vec<FlippedCase>,
    pub families_before: Vec<FamilyCount>,
    pub families_after: Vec<FamilyCount>,
    /// Cases present on one side only. Averaging across different populations is how a dataset edit
    /// reads as a regression, so they are counted out loud instead of folded in.
    pub unpaired_before: usize,
    pub unpaired_after: usize,
}

/// Running tally for one dimension on one side.
#[derive(Default)]
struct Side {
    sum: f64,
    n: usize,
    floor_hits: usize,
    voided: usize,
    present: bool,
}

impl Side {
    fn mean(&self) -> Option<f64> {
        (self.n > 0).then(|| self.sum / self.n as f64)
    }
}

fn tally(scores: &[Score]) -> BTreeMap<String, Side> {
    let mut out: BTreeMap<String, Side> = BTreeMap::new();
    for s in scores {
        let Some(d) = &s.detail else { continue };
        for dim in &d.dimensions {
            let e = out.entry(dim.key.clone()).or_default();
            e.present = true;
            if dim.voided {
                e.voided += 1;
            } else {
                e.sum += dim.value;
                e.n += 1;
            }
            if dim.floor_hit {
                e.floor_hits += 1;
            }
        }
    }
    out
}

/// The run's verdict on one case: `Some(true)` only when every observation carrying a `pass`
/// passed. A compare run writes several cells per case, and "one target passed" is not the case
/// passing.
fn case_pass(scores: &[&Score]) -> Option<bool> {
    let mut seen = false;
    let mut all = true;
    for s in scores {
        if let Some(p) = s.pass {
            seen = true;
            all &= p;
        }
    }
    seen.then_some(all)
}

fn by_case(scores: &[Score]) -> BTreeMap<u32, Vec<&Score>> {
    let mut out: BTreeMap<u32, Vec<&Score>> = BTreeMap::new();
    for s in scores {
        if let Some(i) = s.case_index {
            out.entry(i).or_default().push(s);
        }
    }
    out
}

fn families(scores: &[Score]) -> Vec<FamilyCount> {
    let mut rows = Vec::new();
    let mut count = |family: &'static str, f: &dyn Fn(&Score) -> Option<bool>| {
        let (mut cases, mut denominator) = (0usize, 0usize);
        for s in scores {
            if let Some(hit) = f(s) {
                denominator += 1;
                cases += usize::from(hit);
            }
        }
        rows.push(FamilyCount {
            family,
            status: if denominator == 0 {
                "not_measured"
            } else {
                "measured"
            },
            cases,
            denominator,
        });
    };
    count("parse_failures", &|s| {
        let d = s.detail.as_ref()?;
        // A case could only have had a parse failure if sampling was accounted for at all.
        d.samples_requested
            .map(|_| d.parse_failures.is_some_and(|n| n > 0))
    });
    count("low_agreement", &|s| {
        let d = s.detail.as_ref()?;
        d.agreement.map(|a| a <= LOW_AGREEMENT)
    });
    count("injection_suspected", &|s| {
        s.detail.as_ref()?.injection_suspected
    });
    count("floor_hit", &|s| {
        let d = s.detail.as_ref()?;
        // Only a dimension that *has* a floor can hit one.
        d.dimensions
            .iter()
            .any(|dim| dim.floor.is_some())
            .then(|| d.dimensions.iter().any(|dim| dim.floor_hit))
    });
    rows
}

/// Compare two runs' stored verdicts. `before` is the baseline run, `after` the one under scrutiny.
pub(crate) fn diff(before: &[Score], after: &[Score]) -> RegressionDiff {
    let (tb, ta) = (tally(before), tally(after));
    let keys: BTreeSet<&String> = tb.keys().chain(ta.keys()).collect();

    let mut dimensions: Vec<DimDelta> = keys
        .into_iter()
        .map(|k| {
            let (b, a) = (tb.get(k), ta.get(k));
            let def = Side::default();
            let (b, a) = (b.unwrap_or(&def), a.unwrap_or(&def));
            let (mb, ma) = (b.mean(), a.mean());
            // Measured on both sides ⇒ subtract. Measured on one ⇒ the coverage moved, and the
            // honest report of that is both counts and no number.
            let (status, delta) = match (mb, ma) {
                (Some(x), Some(y)) => ("compared", Some(y - x)),
                _ => ("coverage_changed", None),
            };
            DimDelta {
                key: k.clone(),
                status,
                mean_before: mb,
                mean_after: ma,
                delta,
                n_before: b.n,
                n_after: a.n,
                floor_hits_before: b.floor_hits,
                floor_hits_after: a.floor_hits,
                voided_before: b.voided,
                voided_after: a.voided,
            }
        })
        .collect();
    // Worst drop first; the rows with no delta sort after every compared row rather than pretending
    // to a position in the ranking. `BTreeSet` gave the keys a stable order, so ties keep it.
    dimensions.sort_by(|x, y| match (x.delta, y.delta) {
        (Some(a), Some(b)) => a.partial_cmp(&b).unwrap_or(std::cmp::Ordering::Equal),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });

    if dimensions.is_empty() {
        return RegressionDiff {
            status: "no_dimensions",
            reason: Some(
                "neither run stored a per-dimension verdict: these cases were judged under a \
                 freeform contract (criteria text + threshold), which produces one overall score \
                 and no dimensions. Attach a stored rubric (`rubric_id`) to the benchmark to get a \
                 dimensional signature."
                    .into(),
            ),
            dimensions,
            flipped: Vec::new(),
            families_before: families(before),
            families_after: families(after),
            unpaired_before: 0,
            unpaired_after: 0,
        };
    }

    let worst = dimensions
        .first()
        .filter(|d| d.status == "compared" && d.delta.is_some_and(|v| v < 0.0))
        .map(|d| d.key.clone());

    let (cb, ca) = (by_case(before), by_case(after));
    let mut flipped = Vec::new();
    for (i, after_obs) in &ca {
        let Some(before_obs) = cb.get(i) else {
            continue;
        };
        if case_pass(before_obs) == Some(true) && case_pass(after_obs) == Some(false) {
            let reasonings = worst
                .as_deref()
                .map(|key| reasonings_for(after_obs, key))
                .unwrap_or_default();
            flipped.push(FlippedCase {
                case_index: *i,
                dimension: worst.clone(),
                reasonings,
            });
        }
    }

    RegressionDiff {
        status: "ok",
        reason: None,
        dimensions,
        flipped,
        families_before: families(before),
        families_after: families(after),
        unpaired_before: cb.keys().filter(|i| !ca.contains_key(i)).count(),
        unpaired_after: ca.keys().filter(|i| !cb.contains_key(i)).count(),
    }
}

fn reasonings_for(obs: &[&Score], key: &str) -> Vec<String> {
    obs.iter()
        .filter_map(|s| s.detail.as_ref())
        .flat_map(|d| d.dimensions.iter())
        .filter(|dim| dim.key == key)
        .flat_map(|dim| dim.reasoning.iter().cloned())
        .take(MAX_REASONINGS_PER_FLIP)
        .collect()
}

#[cfg(test)]
mod tests;
