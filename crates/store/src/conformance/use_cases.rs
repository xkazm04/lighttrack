//! Conformance for the use-case registry surface.
//!
//! The registry's value is not CRUD — it is the *difference* between what a project declared and
//! what its events actually did. So the section below registers, corrects, and then asserts that
//! difference is readable: shadow usage (observed, never declared), the uninstrumented bucket, and
//! a model running under a use case that never declared it.

use chrono::Utc;

use lighttrack_core::{new_id, UseCase, UseCaseKind, UseCaseStatus};

use crate::{Result, Store};

pub(super) fn sample(project: &str, key: &str, models: &[&str]) -> UseCase {
    UseCase {
        id: new_id(),
        project_id: project.to_string(),
        key: key.to_string(),
        name: format!("use case {key}"),
        description: Some("registered by the conformance suite".into()),
        kind: UseCaseKind::Judge,
        status: UseCaseStatus::Active,
        component: Some("conformance".into()),
        expected_models: models.iter().map(|m| m.to_string()).collect(),
        owner: None,
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

pub(super) fn use_cases(store: &dyn Store, pid: &str) -> Result<()> {
    let key = format!("uc.{}", &new_id()[..8]);
    let declared_model = "openai/gpt-5.4";
    let drifted_model = "zai-org/GLM-5.3-Flash";

    store.upsert_use_case(&sample(pid, &key, &[declared_model]))?;
    let got = store
        .get_use_case(pid, &key)?
        .expect("a registered use case is readable");
    assert_eq!(got.key, key);
    assert_eq!(got.expected_models, vec![declared_model]);
    assert_eq!(got.kind, UseCaseKind::Judge);

    // Identity is (project, key): registering again corrects rather than conflicts, or the obvious
    // way to fix a description becomes "delete the row the events attribute to".
    let mut corrected = sample(pid, &key, &[declared_model, drifted_model]);
    corrected.name = "corrected".into();
    store.upsert_use_case(&corrected)?;
    let listed: Vec<UseCase> = store
        .list_use_cases(pid)?
        .into_iter()
        .filter(|u| u.key == key)
        .collect();
    assert_eq!(listed.len(), 1, "a correction must not duplicate the row");
    assert_eq!(listed[0].name, "corrected");

    // An absent declaration is not a violation — the distinction that keeps a drift report from
    // flagging every project that never filled the field in.
    let bare_key = format!("bare.{}", &new_id()[..8]);
    store.upsert_use_case(&sample(pid, &bare_key, &[]))?;
    let bare = store.get_use_case(pid, &bare_key)?.expect("bare");
    assert_eq!(bare.declares(declared_model), None);
    assert_eq!(listed[0].declares(declared_model), Some(true));
    assert_eq!(listed[0].declares("someone/else"), Some(false));

    // Now the half that makes it a monitoring layer.
    let shadow = format!("shadow.{}", &new_id()[..8]);
    for (name, model) in [
        (Some(key.as_str()), declared_model),
        (Some(shadow.as_str()), declared_model),
        (None, declared_model),
    ] {
        let mut ev = super::fixtures::sample_event(pid, model, 10, 5, 0.001);
        ev.name = name.map(str::to_string);
        store.insert_event(&ev)?;
    }

    let observed = store.observed_use_case_names(pid, None)?;
    let count = |k: &str| observed.iter().find(|(n, _, _)| n == k).map(|(_, c, _)| *c);

    assert!(
        count(&key).unwrap_or(0) >= 1,
        "declared traffic is observed"
    );
    // Shadow usage: it ran, and nothing declared it.
    assert!(count(&shadow).unwrap_or(0) >= 1, "shadow usage is observed");
    assert!(
        store.get_use_case(pid, &shadow)?.is_none(),
        "shadow usage is by definition unregistered"
    );
    // The uninstrumented bucket must be reported, not dropped: in a project that has not
    // instrumented yet it is the largest and most actionable row there is.
    assert!(
        count("").unwrap_or(0) >= 1,
        "calls with no name must still be counted, got {observed:?}"
    );

    assert!(store.delete_use_case(pid, &bare_key)?);
    assert!(
        !store.delete_use_case(pid, &bare_key)?,
        "delete reports whether anything was there"
    );
    Ok(())
}
