//! `Surface::ScoreSummaries`: verdicts summarized per value of one event dimension (M23).
//!
//! Three prompt-tagged events with verdicts of different quality, and the properties a served-version
//! quality read stands on:
//!
//! * the grouping is by the **event's** `metadata.prompt`, not by anything on the verdict,
//! * `mean` is normalized by `max`, so a rubric scored out of 5 and one scored out of 1 are
//!   comparable — a backend that averaged raw values would report the out-of-5 version as five
//!   times better and a canary would revert the wrong one,
//! * a verdict with no `event_id` is **excluded**, not folded into the untagged bucket: it cannot be
//!   attributed to a version at all,
//! * the window bounds the verdict's `created_at`, and `rubric_id` narrows to one rubric — which is
//!   what makes a canary comparison paired.

use chrono::{Duration, Utc};
use serde_json::json;

use lighttrack_core::{new_id, Dimension, Score, ScoreKind};

use super::fixtures::sample_event;
use crate::{Result, ScoreSummaryRow, Store};

/// One verdict on `event`, scored `value` out of `max`, against `rubric_id`.
fn verdict(project: &str, event: Option<&str>, value: f64, max: f64, rubric_id: &str) -> Score {
    Score {
        id: new_id(),
        project_id: project.into(),
        event_id: event.map(str::to_string),
        rubric: "canary-quality".into(),
        rubric_id: Some(rubric_id.to_string()),
        kind: ScoreKind::Rubric,
        value,
        max,
        pass: Some(value / max >= 0.7),
        reasoning: None,
        detail: None,
        run_id: None,
        case_index: None,
        scored_by: "conformance".into(),
        cost_usd: Some(0.001),
        created_at: Utc::now(),
    }
}

fn row<'a>(rows: &'a [ScoreSummaryRow], key: &str) -> &'a ScoreSummaryRow {
    rows.iter()
        .find(|r| r.key.as_deref() == Some(key))
        .unwrap_or_else(|| panic!("no bucket for '{key}' in {rows:?}"))
}

pub(super) fn score_summaries(store: &dyn Store) -> Result<()> {
    let project = new_id();
    let rubric = new_id();
    let (v1, v2) = (
        format!("conf-{}@v1", &rubric[..8]),
        format!("conf-{}@v2", &rubric[..8]),
    );

    // v1 (production): two good verdicts. v2 (canary): one poor one. Plus an untagged event, so the
    // `None` bucket is exercised rather than assumed away.
    let mk = |tag: Option<&str>, cost: f64| -> Result<String> {
        let mut ev = sample_event(&project, "claude-haiku-4-5", 10, 5, cost);
        ev.metadata = match tag {
            Some(t) => json!({ "prompt": t }),
            None => json!({}),
        };
        store.insert_event(&ev)?;
        Ok(ev.id)
    };
    let e1 = mk(Some(&v1), 0.10)?;
    let e2 = mk(Some(&v1), 0.20)?;
    let e3 = mk(Some(&v2), 0.05)?;
    let e4 = mk(None, 0.01)?;

    // Deliberately different scales on the same rubric: 4/5 and 0.8/1 are the SAME quality.
    store.insert_score(&verdict(&project, Some(&e1), 4.0, 5.0, &rubric))?;
    store.insert_score(&verdict(&project, Some(&e2), 0.8, 1.0, &rubric))?;
    store.insert_score(&verdict(&project, Some(&e3), 0.4, 1.0, &rubric))?;
    store.insert_score(&verdict(&project, Some(&e4), 0.9, 1.0, &rubric))?;
    // A verdict tied to no event: it can be attributed to no version, so it must not appear.
    store.insert_score(&verdict(&project, None, 0.1, 1.0, &rubric))?;
    // A verdict against a DIFFERENT rubric on the canary's event — filtered out below, which is the
    // property that makes the comparison paired instead of a mixture of scales and criteria.
    let other = new_id();
    store.insert_score(&verdict(&project, Some(&e3), 1.0, 1.0, &other))?;

    let since = Utc::now() - Duration::hours(1);
    let rows = store.score_summary_by_dimension(
        Some(&project),
        Dimension::Prompt,
        since,
        None,
        Some(&rubric),
    )?;

    let prod = row(&rows, &v1);
    assert_eq!(prod.n, 2, "both v1 verdicts land in the v1 bucket");
    assert!(
        (prod.mean - 0.8).abs() < 1e-9,
        "the mean normalizes by `max`: 4/5 and 0.8/1 are the same quality, got {}",
        prod.mean
    );
    assert!(
        (prod.pass_rate - 1.0).abs() < 1e-9,
        "both cleared the bar: {prod:?}"
    );
    assert!(
        prod.ci95_low <= prod.mean && prod.ci95_high >= prod.mean,
        "the interval brackets its own mean: {prod:?}"
    );
    assert!(
        prod.cost_usd > 0.29 && prod.cost_usd < 0.31,
        "the bucket carries what the judged EVENTS cost ($0.10 + $0.20), got {}",
        prod.cost_usd
    );

    let canary = row(&rows, &v2);
    assert_eq!(canary.n, 1);
    assert!((canary.mean - 0.4).abs() < 1e-9, "{canary:?}");
    assert!(
        (canary.ci95_low - canary.mean).abs() < 1e-9,
        "n=1 has no spread, so the interval collapses to the mean rather than inventing one"
    );
    assert!(
        canary.mean < prod.mean,
        "the whole point: the canary version reads as worse than the one it is replacing"
    );

    let untagged = rows
        .iter()
        .find(|r| r.key.is_none())
        .expect("an untagged bucket, folded rather than dropped, so the parts sum to the whole");
    assert_eq!(untagged.n, 1);
    assert_eq!(
        rows.iter().map(|r| r.n).sum::<u64>(),
        4,
        "the event-less verdict and the other rubric's are both excluded: {rows:?}"
    );

    // Unfiltered by rubric, the canary's other-rubric verdict joins its bucket.
    let all =
        store.score_summary_by_dimension(Some(&project), Dimension::Prompt, since, None, None)?;
    assert_eq!(
        row(&all, &v2).n,
        2,
        "without a rubric filter every verdict on the version counts"
    );

    // A window that ends before anything was judged is empty — and so is another project's read.
    let past = store.score_summary_by_dimension(
        Some(&project),
        Dimension::Prompt,
        since - Duration::days(2),
        Some(since - Duration::days(1)),
        None,
    )?;
    assert!(past.is_empty(), "the window bounds the verdict's own time");
    assert!(
        store
            .score_summary_by_dimension(Some(&new_id()), Dimension::Prompt, since, None, None)?
            .is_empty(),
        "summaries are scoped to their project"
    );

    // The vocabulary is `Dimension`, not one hard-coded key: grouping by the event's model must work
    // too, or the surface is a prompt feature wearing a dimension's clothes.
    let by_model = store.score_summary_by_dimension(
        Some(&project),
        Dimension::Model,
        since,
        None,
        Some(&rubric),
    )?;
    assert_eq!(
        row(&by_model, "claude-haiku-4-5").n,
        4,
        "every attributed verdict, grouped on an event column instead of a metadata key"
    );
    Ok(())
}
