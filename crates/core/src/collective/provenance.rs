//! What a merged row may say about *its own measurement*: which rubrics stand behind it, and how old
//! its evidence is.
//!
//! The merge already folds both facts (`Acc.rubric_fps` is filled from every stored entry;
//! `CollectiveEntry::received_at` is loaded on the read path) and then publishes neither. This module
//! owns the two folds so the "mixed means at least two" rule is written once for the rubric facet the
//! same way [`super::merge`] writes it for the judge facet.
//!
//! **Why counts and not fingerprints.** A rubric fingerprint is a high-cardinality, contributor-chosen
//! hash. Publishing the *set* on every row would hand any anonymous reader a linkage key: a
//! contributor with a rare rubric could have its whole `(provider, model, task_type)` footprint traced
//! across the board, which is exactly the unilateral-identifiability the `min_contributors` floor and
//! [`bucket_cost`](super::bucket_cost) exist to prevent. The *number* of distinct rubrics — and
//! whether they agree — is what the reader needs to know the row is incommensurable, and it carries no
//! contributor identity. Same argument as cost: the fact is disclosed, the fingerprint is coarsened
//! away.
//!
//! **Why the evidence window is day-coarsened.** A nanosecond receipt time is the same kind of
//! fingerprint: two rows stamped with the identical instant came from one ingest, which links a
//! contributor's rows together without naming it. Truncating to the UTC day keeps every decision the
//! window is for ("is this board stale?", "is this quarter's evidence?") and destroys the join. The
//! published strings are still fixed-width `RFC3339(Nanos, Z)`, so lexicographic comparison is
//! chronological comparison — which is what makes the `fresh_days` filter a plain string compare.

use std::collections::BTreeSet;

use chrono::{DateTime, NaiveTime, SecondsFormat, Utc};

/// Fixed-width `RFC3339(Nanos, Z)` stamp for the UTC **day** `t` falls in (see the module note on why
/// the time-of-day is dropped). Fixed width => lexicographic order is chronological order.
pub fn day_stamp(t: DateTime<Utc>) -> String {
    let day = t.date_naive().and_time(NaiveTime::MIN).and_utc();
    day.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

/// `(oldest, newest)` day stamps across an evidence set, or `None` for an empty one — absence is
/// disclosed as absence, never as "today".
pub fn evidence_window<I>(received: I) -> Option<(String, String)>
where
    I: IntoIterator<Item = DateTime<Utc>>,
{
    let mut oldest: Option<DateTime<Utc>> = None;
    let mut newest: Option<DateTime<Utc>> = None;
    for t in received {
        oldest = Some(oldest.map_or(t, |o| o.min(t)));
        newest = Some(newest.map_or(t, |n| n.max(t)));
    }
    Some((day_stamp(oldest?), day_stamp(newest?)))
}

/// Fold a row's rubric-fingerprint set into `(n_rubrics, mixed_rubrics)`.
///
/// `n_rubrics` is `None` when nothing is recorded: a row whose sources all predate rubric
/// fingerprinting does not know how many rubrics scored it, and `Some(1)` would impute agreement it
/// never measured. `mixed_rubrics` is `Some(n)` only when at least two rubrics stand behind the row —
/// and, exactly as [`super::merge`] counts judge families, a source already tagged `mixed` disagreed
/// with itself and therefore stands for two.
pub fn rubric_agreement(fps: &BTreeSet<String>) -> (Option<u32>, Option<u32>) {
    if fps.is_empty() {
        return (None, None);
    }
    let families = fps.len() + fps.iter().filter(|f| *f == "mixed").count();
    (
        Some(families as u32),
        (families > 1).then_some(families as u32),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone};

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn unknown_rubrics_are_absence_not_agreement() {
        assert_eq!(rubric_agreement(&BTreeSet::new()), (None, None));
    }

    #[test]
    fn one_rubric_is_not_mixed() {
        assert_eq!(rubric_agreement(&set(&["ab12"])), (Some(1), None));
    }

    #[test]
    fn two_rubrics_are_mixed() {
        assert_eq!(
            rubric_agreement(&set(&["ab12", "cd34"])),
            (Some(2), Some(2))
        );
    }

    #[test]
    fn a_self_mixed_source_counts_as_two_rubrics() {
        // Mirrors the judge-family rule: a contribution tagged `mixed` already disagreed with itself,
        // so a row built from it alone must not read as single-rubric.
        assert_eq!(rubric_agreement(&set(&["mixed"])), (Some(2), Some(2)));
    }

    #[test]
    fn day_stamp_is_fixed_width_and_drops_the_time_of_day() {
        let t = Utc.with_ymd_and_hms(2026, 10, 5, 13, 47, 9).unwrap()
            + Duration::nanoseconds(123_456_789);
        assert_eq!(day_stamp(t), "2026-10-05T00:00:00.000000000Z");
        assert_eq!(day_stamp(t).len(), day_stamp(Utc::now()).len());
        // Two pushes on the same day are indistinguishable — that is the point.
        assert_eq!(day_stamp(t), day_stamp(t + Duration::minutes(5)));
    }

    #[test]
    fn window_spans_oldest_to_newest_and_orders_lexicographically() {
        let now = Utc::now();
        let w = evidence_window([now - Duration::days(200), now - Duration::days(3), now]).unwrap();
        assert_eq!(w.0, day_stamp(now - Duration::days(200)));
        assert_eq!(w.1, day_stamp(now));
        assert!(w.0 < w.1, "fixed width => string order is time order");
    }

    #[test]
    fn empty_evidence_has_no_window() {
        assert!(evidence_window([]).is_none());
    }
}
