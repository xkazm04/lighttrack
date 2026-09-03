//! `Surface::Labels` and `Surface::Calibrations`: the human verdict ledger, and the trust lookup
//! every gate makes.
//!
//! Two properties are pinned here rather than in a backend's own tests, because both are invisible
//! from outside and both fail *silently*:
//!
//! 1. **The dataset join actually joins.** `labels_for_dataset` has to reach labels through the
//!    items of a dataset. A backend that returned `[]` would produce a calibration measured on
//!    nothing — which reports κ = 0, "untrusted", and sends an operator hunting a judge regression
//!    that never happened.
//! 2. **`latest_calibration` is exact on the rubric, `NULL` included.** Borrowing one rubric's κ
//!    for another (or the freeform one for a rubric) is the uncalibrated gate wearing a trusted
//!    badge, and it is exactly what a sloppy `rubric_id = ?` on a NULL would produce.

use chrono::{Duration, Utc};

use lighttrack_core::{
    new_id, CalibrationRecord, Dataset, DatasetItem, JudgeTrust, Label, LabelFilter, LabelSubject,
    ScoreDim,
};

use crate::Scope;
use crate::{Result, Store};

pub(super) fn sample_label(project: &str, subject: LabelSubject, value: f64) -> Label {
    Label {
        id: new_id(),
        project_id: project.to_string(),
        subject,
        rubric_id: None,
        value,
        pass: None,
        dimensions: Vec::new(),
        labeler: "conformance".to_string(),
        note: None,
        created_at: Utc::now(),
    }
}

pub(super) fn sample_calibration(project: &str, judge: &str) -> CalibrationRecord {
    CalibrationRecord {
        id: new_id(),
        project_id: project.to_string(),
        judge: judge.to_string(),
        rubric_id: None,
        dataset_id: None,
        dataset_version: None,
        kappa: 0.8,
        pearson: 0.9,
        mae: 0.05,
        rmse: 0.07,
        n: 12,
        kappa_bar: 0.6,
        trusted: true,
        created_at: Utc::now(),
    }
}

pub(super) fn labels(store: &dyn Store, pid: &str) -> Result<()> {
    let event_id = new_id();
    let mut l = sample_label(pid, LabelSubject::Event(event_id.clone()), 0.85);
    l.rubric_id = Some(new_id());
    l.pass = Some(true);
    l.note = Some("graded by hand".to_string());
    l.dimensions = vec![ScoreDim {
        key: "accuracy".to_string(),
        value: 0.9,
        weight: 1.0,
        ..Default::default()
    }];
    store.insert_label(&l)?;

    let got = one_label(store, pid, &l.id)?;
    assert_eq!(got.subject, LabelSubject::Event(event_id), "subject");
    assert_eq!(got.rubric_id, l.rubric_id, "rubric round-trips");
    assert_eq!(got.pass, Some(true), "an explicit human call round-trips");
    assert_eq!(got.labeler, "conformance", "the provenance is the point");
    assert_eq!(got.note.as_deref(), Some("graded by hand"));
    assert_eq!(
        got.dimensions.len(),
        1,
        "the per-dimension breakdown round-trips"
    );
    assert!((got.dimensions[0].value - 0.9).abs() < 1e-9);

    subject_filter(store, pid, &l)?;
    dataset_join(store, pid)?;
    Ok(())
}

/// A subject filter must narrow to **one** subject — kind and id together. A backend that matched
/// only on the id would attach an event's grade to a dataset item that happened to share one.
fn subject_filter(store: &dyn Store, pid: &str, existing: &Label) -> Result<()> {
    let other = sample_label(pid, LabelSubject::Score(new_id()), 0.2);
    store.insert_label(&other)?;

    let page = store.list_labels(&LabelFilter {
        project: Some(pid.to_string()),
        subject: Some(existing.subject.clone()),
        ..Default::default()
    })?;
    assert!(
        page.iter().any(|x| x.id == existing.id),
        "the subject's own label must be in its page"
    );
    assert!(
        !page.iter().any(|x| x.id == other.id),
        "a label on a different subject must never appear in a narrowed page"
    );

    // Same id, different kind: nothing.
    let shadow = match &existing.subject {
        LabelSubject::Event(id) => LabelSubject::DatasetItem(id.clone()),
        s => LabelSubject::Event(s.id().to_string()),
    };
    let page = store.list_labels(&LabelFilter {
        project: Some(pid.to_string()),
        subject: Some(shadow),
        ..Default::default()
    })?;
    assert!(
        page.is_empty(),
        "the subject kind is half the key: an id alone must not match across kinds"
    );

    let by_rubric = store.list_labels(&LabelFilter {
        project: Some(pid.to_string()),
        rubric_id: existing.rubric_id.clone(),
        ..Default::default()
    })?;
    assert!(by_rubric.iter().any(|x| x.id == existing.id));
    assert!(
        !by_rubric.iter().any(|x| x.id == other.id),
        "a rubric-less label must not answer a rubric-narrowed question"
    );
    Ok(())
}

/// The join `lt-runner calibrate --dataset` is built on: dataset → its items → their labels.
fn dataset_join(store: &dyn Store, pid: &str) -> Result<()> {
    let ds = Dataset {
        id: new_id(),
        project_id: pid.to_string(),
        name: format!("conf-golden-{}", new_id()),
        version: 1,
        frozen: false,
        source: None,
        created_at: Utc::now(),
        parent_id: None,
    };
    store.create_dataset(&ds)?;
    let item = DatasetItem {
        id: new_id(),
        dataset_id: ds.id.clone(),
        input: "what is 2+2".to_string(),
        output: Some("4".to_string()),
        expected: Some("4".to_string()),
        context: None,
        tags: Vec::new(),
        source_event_id: None,
        anonymization: serde_json::Value::Null,
        input_hash: None,
    };
    store.create_dataset_item(&item)?;
    let l = sample_label(pid, LabelSubject::DatasetItem(item.id.clone()), 0.95);
    store.insert_label(&l)?;

    // A label on an item of ANOTHER dataset, to prove the join narrows rather than returning
    // every dataset-item label in the database.
    let other_item = DatasetItem {
        id: new_id(),
        dataset_id: new_id(),
        ..item.clone()
    };
    let stray = sample_label(pid, LabelSubject::DatasetItem(other_item.id.clone()), 0.1);
    store.insert_label(&stray)?;

    let found = store.labels_for_dataset(Scope::Operator, &ds.id)?;
    assert!(
        found.iter().any(|x| x.id == l.id),
        "a label on an item of this dataset must be reachable from the dataset id — a backend that \
         answered [] here would calibrate a judge against nothing and report it as untrusted"
    );
    assert!(
        !found.iter().any(|x| x.id == stray.id),
        "…and a label on another dataset's item must not be"
    );
    assert!(
        store
            .labels_for_dataset(Scope::Operator, &new_id())?
            .is_empty(),
        "a dataset nobody has labelled has no labels"
    );
    Ok(())
}

fn one_label(store: &dyn Store, pid: &str, id: &str) -> Result<Label> {
    let page = store.list_labels(&LabelFilter {
        project: Some(pid.to_string()),
        ..Default::default()
    })?;
    Ok(page
        .into_iter()
        .find(|x| x.id == id)
        .expect("an inserted label must appear in its project's listing"))
}

pub(super) fn calibrations(store: &dyn Store, pid: &str) -> Result<()> {
    let judge = format!("anthropic/conf-{}", new_id());
    let rubric = new_id();

    assert_eq!(
        store
            .latest_calibration(pid, Some(&rubric), &judge)?
            .map(|_| ()),
        None,
        "a pair nobody has measured has no record — which is what makes trust `unknown` rather \
         than `untrusted`"
    );

    let mut freeform = sample_calibration(pid, &judge);
    freeform.created_at = Utc::now() - Duration::minutes(5);
    store.insert_calibration(&freeform)?;

    // The rule the whole surface turns on: a rubric NEVER inherits the freeform measurement.
    assert!(
        store
            .latest_calibration(pid, Some(&rubric), &judge)?
            .is_none(),
        "a rubric must not inherit the freeform judge's kappa — that is the uncalibrated gate \
         wearing a trusted badge"
    );
    let got = store
        .latest_calibration(pid, None, &judge)?
        .expect("the freeform record must be findable by `rubric_id IS NULL`");
    assert_eq!(got.id, freeform.id);
    assert_eq!(got.n, 12, "the sample size round-trips");
    assert!(
        (got.kappa_bar - 0.6).abs() < 1e-9,
        "so does the bar it used"
    );
    assert_eq!(got.trust(), JudgeTrust::Trusted);

    // …nor from a sibling rubric, nor from another judge.
    let mut rubric_rec = sample_calibration(pid, &judge);
    rubric_rec.rubric_id = Some(rubric.clone());
    rubric_rec.trusted = false;
    rubric_rec.kappa = 0.1;
    rubric_rec.dataset_id = Some(new_id());
    rubric_rec.dataset_version = Some(3);
    store.insert_calibration(&rubric_rec)?;
    let got = store
        .latest_calibration(pid, Some(&rubric), &judge)?
        .expect("the rubric's own record");
    assert_eq!(got.id, rubric_rec.id);
    assert_eq!(got.trust(), JudgeTrust::Untrusted);
    assert_eq!(
        got.dataset_version,
        Some(3),
        "what was measured round-trips"
    );
    assert!(
        store
            .latest_calibration(pid, Some(&new_id()), &judge)?
            .is_none(),
        "a sibling rubric's trust is not this rubric's"
    );
    assert!(
        store
            .latest_calibration(pid, Some(&rubric), "someone/else")?
            .is_none(),
        "another judge's trust is not this judge's"
    );

    newest_wins(store, pid, &judge, &rubric)?;
    Ok(())
}

/// Append-only, newest-first: a re-measurement supersedes without erasing, because the history is
/// what the drift check reads.
fn newest_wins(store: &dyn Store, pid: &str, judge: &str, rubric: &str) -> Result<()> {
    let mut fresh = sample_calibration(pid, judge);
    fresh.rubric_id = Some(rubric.to_string());
    fresh.kappa = 0.77;
    fresh.created_at = Utc::now() + Duration::seconds(1);
    store.insert_calibration(&fresh)?;

    let got = store
        .latest_calibration(pid, Some(rubric), judge)?
        .expect("latest");
    assert_eq!(got.id, fresh.id, "the newest record decides");
    assert!((got.kappa - 0.77).abs() < 1e-9);

    let history = store.list_calibrations(Scope::Project(pid), 100, None)?;
    assert!(
        history.iter().filter(|c| c.judge == judge).count() >= 3,
        "a re-measurement appends: the earlier records must still be there for a drift check"
    );
    assert!(
        history
            .windows(2)
            .all(|w| w[0].created_at >= w[1].created_at),
        "the history is newest-first"
    );
    assert!(
        store.list_calibrations(Scope::Project(pid), 1, None)?.len() <= 1,
        "the page size is honoured"
    );
    Ok(())
}
