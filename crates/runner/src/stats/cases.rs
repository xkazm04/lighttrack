//! Case identity, and pairing two score vectors **by it**.
//!
//! Every paired claim this repo makes used to pair two vectors by position, having checked only that
//! they were the same length. That is not the same check. A compare run's per-case vector is
//! *compacted* — an errored cell is skipped before its score is pushed — so a vector index is a
//! position among *judged* cases, not a case index, and two targets that each errored on a different
//! case finish with equal lengths and misaligned positions. The circuit breaker
//! (`compare::TargetHealth`) makes differing case sets ordinary rather than exotic.
//!
//! Pairing the wrong cases does not merely weaken the test, it inverts it: differencing two
//! *different* cases **adds** between-case variance to the deltas while the paired standard error
//! still claims it was removed, so the answer comes back wrong and overconfident.
//!
//! The fix is the shape `calibrate_batch` already had by accident: carry the pairing in the type.
//! A [`CaseScore`] knows which case it measured, and [`paired_deltas_by_case`] aligns on the
//! **intersection** of two case sets — refusing outright would delete the `best` line from most real
//! matrices and buy no correctness, because the retained cases are genuinely matched. What it must
//! never do is pair silently: the retained and dropped counts travel with the deltas so the reduced
//! n reaches the power disclosure, and an [`Unpairable`] carries the reason instead of a bare `None`.

use std::collections::{BTreeMap, BTreeSet};

/// One case's score, carrying the case it was measured on. The id is the **1-based case index** the
/// run report already writes on every logged case (`{"case": i + 1, "score": …}`) — the identity was
/// persisted all along and thrown away on the way back in.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CaseScore {
    pub(crate) case: u32,
    pub(crate) score: f64,
}

impl CaseScore {
    pub(crate) fn new(case: u32, score: f64) -> CaseScore {
        CaseScore { case, score }
    }
}

/// The bare scores, for the summary statistics that are about one vector rather than a pairing.
pub(crate) fn values(xs: &[CaseScore]) -> Vec<f64> {
    xs.iter().map(|c| c.score).collect()
}

/// Why two case-identified vectors could not be paired. A reason, never a bare absence: "the case
/// sets do not overlap", "a report names one case twice" and "they share a single case" are three
/// different facts about a run, and a caller that prints them identically has hidden two of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Unpairable {
    /// One or both sides scored no cases at all.
    Empty,
    /// The two case sets are disjoint — nothing was judged in both.
    Disjoint,
    /// A case id appears more than once on one side. Which score was meant is unknowable, and
    /// guessing is how this class of defect starts; the run report writes `i + 1`, so this can only
    /// come from a hand-edited or corrupted report.
    DuplicateCaseIds,
    /// They share cases, but too few to test: `n < 2` has no spread, so there is no stderr.
    TooFewShared(usize),
}

impl Unpairable {
    /// The refusal in the reporting vocabulary the summary already uses.
    pub(crate) fn reason(self) -> String {
        match self {
            Unpairable::Empty => "one of the two produced no scored cases at all".to_string(),
            Unpairable::Disjoint => {
                "no case was judged in both, so the pair cannot be tested — untested, neither \
                 sufficient nor insufficient"
                    .to_string()
            }
            Unpairable::DuplicateCaseIds => {
                "a case id appears twice in one of the two reports, so which scores pair is \
                 unknowable — refused rather than guessed"
                    .to_string()
            }
            Unpairable::TooFewShared(n) => format!(
                "only {n} case was judged in both — a paired test needs at least 2 to have any \
                 spread to test"
            ),
        }
    }
}

/// Per-case differences over the cases both sides scored, with the arithmetic that says how much of
/// each side was left out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PairedByCase {
    /// `run − baseline` for each shared case, in case-id order.
    pub(crate) deltas: Vec<f64>,
    /// Cases judged by both. Equals `deltas.len()`, and is the **real** n of the paired test — the
    /// figure that must reach the power disclosure, not either side's own case count.
    pub(crate) retained: usize,
    /// Cases judged by exactly one of the two, and therefore dropped from the test.
    pub(crate) dropped: usize,
}

impl PairedByCase {
    /// True when the pairing covered less than either side measured. The delta is valid *for the
    /// retained cases*; a target that errors on the hard ones leaves an easier intersection, so this
    /// generalises less — a caveat to state, never a reason to hide the result.
    pub(crate) fn is_subset(&self) -> bool {
        self.dropped > 0
    }

    /// The subset caveat in words, for the `caveats` arrays the reports already carry.
    pub(crate) fn subset_caveat(&self) -> String {
        format!(
            "paired on the {} case(s) judged by both; {} case(s) were judged by only one and are \
             not in this test. The delta holds for the cases that remain — a target that errors on \
             the hard ones leaves an easier intersection",
            self.retained, self.dropped
        )
    }
}

/// Per-case differences `run[c] − baseline[c]` over the cases both sides judged, aligned on the
/// **case id** and not on the vector index.
///
/// The intersection is deliberate. Refusing whenever any target errored on a single case would
/// delete the tested `best` line from most real matrices and buy no correctness: the retained cases
/// are genuinely matched, so the test is valid — it is just over fewer of them, which is why
/// `retained`/`dropped` come back with the deltas instead of being left for the caller to guess.
///
/// See [`super::paired::paired_deltas`] for the position-pairing form, which is correct only for
/// callers whose two vectors are built from one collection.
pub(crate) fn paired_deltas_by_case(
    run: &[CaseScore],
    baseline: &[CaseScore],
) -> Result<PairedByCase, Unpairable> {
    if run.is_empty() || baseline.is_empty() {
        return Err(Unpairable::Empty);
    }
    let index = |xs: &[CaseScore]| -> Option<BTreeMap<u32, f64>> {
        let mut m = BTreeMap::new();
        for c in xs {
            if m.insert(c.case, c.score).is_some() {
                return None;
            }
        }
        Some(m)
    };
    let (Some(r), Some(b)) = (index(run), index(baseline)) else {
        return Err(Unpairable::DuplicateCaseIds);
    };
    // BTreeMap keys are ordered, so the deltas come back in case order whatever order the caller
    // built its vectors in — two runs of one matrix can never produce a different pairing.
    let deltas: Vec<f64> = r
        .iter()
        .filter_map(|(case, score)| b.get(case).map(|prev| score - prev))
        .collect();
    let retained = deltas.len();
    if retained == 0 {
        return Err(Unpairable::Disjoint);
    }
    if retained < 2 {
        return Err(Unpairable::TooFewShared(retained));
    }
    let union: BTreeSet<u32> = r.keys().chain(b.keys()).copied().collect();
    Ok(PairedByCase {
        deltas,
        retained,
        dropped: union.len() - retained,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cs(pairs: &[(u32, f64)]) -> Vec<CaseScore> {
        pairs.iter().map(|&(c, s)| CaseScore::new(c, s)).collect()
    }

    /// Identical case sets: every case pairs, nothing is dropped, and the deltas are the plain
    /// per-case differences.
    #[test]
    fn identical_case_sets_pair_completely() {
        let p = paired_deltas_by_case(&cs(&[(1, 0.8), (2, 0.9)]), &cs(&[(1, 0.7), (2, 0.7)]))
            .expect("same cases");
        assert_eq!(p.retained, 2);
        assert_eq!(p.dropped, 0);
        assert!(!p.is_subset());
        assert!((p.deltas[0] - 0.1).abs() < 1e-12 && (p.deltas[1] - 0.2).abs() < 1e-12);
    }

    /// **The defect, in the smallest form that shows it.** Two vectors of EQUAL LENGTH whose case
    /// sets are offset by one: position pairing differences case 2 against case 1 and reports a
    /// perfectly consistent gap that does not exist. By case, only the overlap survives.
    #[test]
    fn equal_lengths_are_not_a_matching_case_set() {
        // Both targets score the case's difficulty exactly, so on every SHARED case they are equal.
        let a = cs(&[(2, 0.25), (3, 0.40), (4, 0.55), (5, 0.70), (6, 0.85)]);
        let b = cs(&[(1, 0.10), (2, 0.25), (3, 0.40), (4, 0.55), (5, 0.70)]);
        assert_eq!(a.len(), b.len(), "the count check would pass");
        let p = paired_deltas_by_case(&a, &b).expect("they overlap on cases 2..5");
        assert_eq!(p.retained, 4);
        assert_eq!(p.dropped, 2, "case 1 and case 6 belong to one side only");
        assert!(
            p.deltas.iter().all(|d| d.abs() < 1e-12),
            "the shared cases are identical — the gap was an artefact of position: {:?}",
            p.deltas
        );
        // Position pairing on the same input claims a flat +0.15 on every case.
        let by_pos =
            super::super::paired::paired_deltas(&values(&a), &values(&b)).expect("n match");
        assert!(
            by_pos.iter().all(|d| (d - 0.15).abs() < 1e-9),
            "the hazard the position form carries: {by_pos:?}"
        );
    }

    /// Disjoint, empty, single-case overlap and duplicate ids are four different refusals, and each
    /// says which it is rather than coming back as one anonymous absence.
    #[test]
    fn every_refusal_names_itself() {
        assert_eq!(
            paired_deltas_by_case(&cs(&[(1, 0.5), (2, 0.5)]), &cs(&[(7, 0.5), (8, 0.5)])),
            Err(Unpairable::Disjoint)
        );
        assert_eq!(
            paired_deltas_by_case(&[], &cs(&[(1, 0.5)])),
            Err(Unpairable::Empty)
        );
        assert_eq!(
            paired_deltas_by_case(&cs(&[(1, 0.5)]), &[]),
            Err(Unpairable::Empty)
        );
        assert_eq!(paired_deltas_by_case(&[], &[]), Err(Unpairable::Empty));
        // Overlap below the testable minimum is NOT "disjoint": they do share a case, there is just
        // no spread over one difference.
        assert_eq!(
            paired_deltas_by_case(&cs(&[(1, 0.5), (2, 0.6)]), &cs(&[(2, 0.4), (9, 0.4)])),
            Err(Unpairable::TooFewShared(1))
        );
        // A case named twice is refused, never silently deduplicated to whichever came first.
        assert_eq!(
            paired_deltas_by_case(
                &cs(&[(1, 0.5), (1, 0.9), (2, 0.6)]),
                &cs(&[(1, 0.4), (2, 0.4)])
            ),
            Err(Unpairable::DuplicateCaseIds)
        );
        assert_eq!(
            paired_deltas_by_case(&cs(&[(1, 0.5), (2, 0.6)]), &cs(&[(2, 0.4), (2, 0.4)])),
            Err(Unpairable::DuplicateCaseIds)
        );
        // Each reason is distinct prose — a caller printing them cannot collapse two facts into one.
        let reasons = [
            Unpairable::Empty.reason(),
            Unpairable::Disjoint.reason(),
            Unpairable::DuplicateCaseIds.reason(),
            Unpairable::TooFewShared(1).reason(),
        ];
        for (i, a) in reasons.iter().enumerate() {
            assert!(!a.is_empty());
            assert!(reasons[i + 1..].iter().all(|b| a != b), "duplicate: {a}");
        }
    }

    /// Case order is not part of the identity: the same two sets pair the same way whatever order
    /// the caller assembled them in, so two runs of one matrix cannot disagree.
    #[test]
    fn pairing_does_not_depend_on_vector_order() {
        let sorted = paired_deltas_by_case(
            &cs(&[(1, 0.1), (2, 0.2), (3, 0.3)]),
            &cs(&[(1, 0.0), (2, 0.0), (3, 0.0)]),
        )
        .expect("ok");
        let shuffled = paired_deltas_by_case(
            &cs(&[(3, 0.3), (1, 0.1), (2, 0.2)]),
            &cs(&[(2, 0.0), (3, 0.0), (1, 0.0)]),
        )
        .expect("ok");
        assert_eq!(sorted.deltas, shuffled.deltas);
        assert_eq!(
            sorted.deltas,
            vec![0.1, 0.2, 0.3],
            "deltas are in case order"
        );
        assert_eq!(shuffled.retained, 3);
        assert_eq!(shuffled.dropped, 0);
    }

    /// The dropped count is over the UNION, so cases missing from either side are counted once each
    /// — a subset pairing must not under-report how much of the run it left out.
    #[test]
    fn dropped_counts_both_sides_of_the_union() {
        let p = paired_deltas_by_case(
            &cs(&[(1, 0.5), (2, 0.5), (3, 0.5)]),
            &cs(&[(2, 0.4), (3, 0.4), (4, 0.4), (5, 0.4)]),
        )
        .expect("overlap on 2,3");
        assert_eq!(p.retained, 2);
        assert_eq!(
            p.dropped, 3,
            "case 1 from the run, cases 4 and 5 from the baseline"
        );
        assert!(p.is_subset());
        let c = p.subset_caveat();
        assert!(
            c.contains("2 case(s) judged by both") && c.contains("3 case(s)"),
            "{c}"
        );
    }

    #[test]
    fn values_strips_the_identity_for_one_vector_statistics() {
        assert_eq!(values(&cs(&[(4, 0.25), (1, 0.75)])), vec![0.25, 0.75]);
    }
}
