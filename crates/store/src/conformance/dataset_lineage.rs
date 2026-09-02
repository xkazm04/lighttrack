//! `Surface::DatasetLineage`: forking a dataset into its next version, mining rows into one, and
//! reading a name's version history (M24).
//!
//! Three properties are pinned here rather than in a backend's own tests, because each one fails in
//! a way that looks like success:
//!
//! 1. **A fork increments `version` and links `parent_id`.** A backend that copied the row at
//!    version 1 would leave the runner's paired-test guard comparing 1 with 1 — reporting two
//!    different corpora as comparable, which is the exact bug M24 exists to end.
//! 2. **The copy is complete, labels included.** A fork that dropped a human verdict leaves a case
//!    that still *looks* golden; the next calibration measures the judge against fewer pairs and
//!    calls the difference drift.
//! 3. **A frozen dataset refuses an import.** Appending to the corpus a finished run was scored
//!    against silently rewrites that run's meaning, and `Ok(0)` would read as "nothing matched".

use chrono::Utc;
use serde_json::{json, Value};

use lighttrack_core::{
    new_id, Dataset, DatasetItem, ImportSource, ImportSpec, LabelSubject, LlmEvent,
    SamplingStrategy,
};

use super::labels::sample_label;
use crate::{Result, Store, StoreError};

pub(super) fn dataset_lineage(store: &dyn Store, pid: &str) -> Result<()> {
    let name = format!("lineage-{}", new_id());
    let v1 = seed_v1(store, pid, &name)?;

    let v2 = store.fork_dataset(Some(pid), &v1.id)?;
    assert_eq!(
        v2.version, 2,
        "a fork is the NEXT version, not a copy of v1"
    );
    assert_eq!(
        v2.parent_id.as_deref(),
        Some(v1.id.as_str()),
        "without the parent link a v2 is just another row that shares a name"
    );
    assert_eq!(v2.name, v1.name, "a fork keeps the name — that is its key");
    assert!(!v2.frozen, "a fork exists to be extended");
    assert_ne!(v2.id, v1.id);
    assert!(
        store.get_dataset(&v1.id)?.expect("v1 survives").frozen,
        "forking must not unfreeze the parent — a finished run was scored against it"
    );

    copied_items_and_labels(store, &v1, &v2)?;
    versions_list(store, pid, &name, &v1, &v2)?;
    frozen_refuses_import(store, pid, &v1)?;
    imports(store, pid, &v2)?;

    // Fork again: the second fork must land past the highest version the NAME carries, not past its
    // own source's — two v2s a version pin cannot tell apart is the failure this rules out.
    let v3 = store.fork_dataset(Some(pid), &v1.id)?;
    assert_eq!(
        v3.version, 3,
        "a second fork of v1 is v3: versions are unique per name, or a pin means nothing"
    );

    assert!(
        store.fork_dataset(Some(pid), &new_id()).is_err(),
        "forking a dataset that does not exist is an error, never an empty new version"
    );
    assert!(
        store.fork_dataset(Some(&new_id()), &v1.id).is_err(),
        "another project's dataset is not forkable from this scope"
    );
    Ok(())
}

/// A frozen v1 with one labelled item — the shape a golden set is actually in when someone needs to
/// extend it.
fn seed_v1(store: &dyn Store, pid: &str, name: &str) -> Result<Dataset> {
    let ds = Dataset {
        id: new_id(),
        project_id: pid.to_string(),
        name: name.to_string(),
        version: 1,
        frozen: false,
        source: Some("manual".to_string()),
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
        tags: vec!["golden".to_string()],
        source_event_id: None,
        anonymization: json!({ "method": "regex", "redactions": 0 }),
        input_hash: None,
    };
    store.create_dataset_item(&item)?;
    let mut l = sample_label(pid, LabelSubject::DatasetItem(item.id.clone()), 0.9);
    l.labeler = "conformance-lineage".to_string();
    store.insert_label(&l)?;
    store.set_dataset_frozen(&ds.id, true)?;
    Ok(Dataset { frozen: true, ..ds })
}

fn copied_items_and_labels(store: &dyn Store, v1: &Dataset, v2: &Dataset) -> Result<()> {
    let src = store.list_dataset_items(&v1.id)?;
    let copied = store.list_dataset_items(&v2.id)?;
    assert_eq!(copied.len(), src.len(), "a fork copies every case");
    assert_eq!(copied[0].input, src[0].input);
    assert_eq!(
        copied[0].expected, src[0].expected,
        "the golden reference is the case"
    );
    assert_ne!(
        copied[0].id, src[0].id,
        "a copy is a new row, not the same one"
    );
    assert_eq!(copied[0].dataset_id, v2.id);

    let carried = store.labels_for_dataset(&v2.id)?;
    assert!(
        carried.iter().any(|l| l.labeler == "conformance-lineage"),
        "the human verdict must survive the fork — a golden case whose label did not is an \
         ungraded string the next calibration quietly measures against"
    );
    assert!(
        store
            .labels_for_dataset(&v1.id)?
            .iter()
            .any(|l| l.labeler == "conformance-lineage"),
        "…and it is copied, not moved: the frozen parent keeps its own grades"
    );
    Ok(())
}

fn versions_list(
    store: &dyn Store,
    pid: &str,
    name: &str,
    v1: &Dataset,
    v2: &Dataset,
) -> Result<()> {
    let vs = store.list_dataset_versions(Some(pid), name)?;
    assert_eq!(vs.len(), 2, "both versions of the name are listed");
    assert_eq!(vs[0].id, v2.id, "newest version first");
    assert_eq!(vs[1].id, v1.id);
    assert!(
        store
            .list_dataset_versions(Some(pid), &format!("no-such-{}", new_id()))?
            .is_empty(),
        "a name nobody has used has no versions"
    );
    assert!(
        store
            .list_dataset_versions(Some(&new_id()), name)?
            .is_empty(),
        "the version history is scoped to its project"
    );
    Ok(())
}

fn frozen_refuses_import(store: &dyn Store, pid: &str, frozen: &Dataset) -> Result<()> {
    match store.import_dataset_items(Some(pid), &frozen.id, &ImportSpec::default()) {
        Err(StoreError::Conflict(_)) => Ok(()),
        got => panic!(
            "importing into a frozen dataset must conflict, not silently append or report zero \
             matches; got {got:?}"
        ),
    }
}

/// The mining half: an explicit id list, a filtered sample, dedupe, and the failure-only strategy.
fn imports(store: &dyn Store, pid: &str, target: &Dataset) -> Result<()> {
    let ok = event(pid, "modelA", "success", "Summarise this order");
    let bad = event(pid, "modelB", "error", "Summarise   THIS   ORDER");
    store.insert_event(&ok)?;
    store.insert_event(&bad)?;

    let before = store.list_dataset_items(&target.id)?.len();
    let n = store.import_dataset_items(
        Some(pid),
        &target.id,
        &ImportSpec {
            n: 10,
            event_ids: vec![ok.id.clone()],
            ..Default::default()
        },
    )?;
    assert_eq!(n, 1, "an explicit id list imports exactly those rows");
    let items = store.list_dataset_items(&target.id)?;
    assert_eq!(items.len(), before + 1);
    let mined = items
        .iter()
        .find(|i| i.source_event_id.as_deref() == Some(ok.id.as_str()))
        .expect("the imported case names the event it came from");
    assert!(
        mined.input_hash.is_some(),
        "an imported case carries its fingerprint, or dedupe can never see it"
    );
    assert_eq!(
        mined.anonymization["method"], "regex",
        "production text is scrubbed on the way into a corpus, and the audit says so"
    );

    // The second event's input differs from the first only in spacing and case: with dedupe on it
    // is the same case, and the whole point of the fingerprint is that this is not a re-import.
    let dup = store.import_dataset_items(
        Some(pid),
        &target.id,
        &ImportSpec {
            n: 10,
            dedupe: true,
            event_ids: vec![bad.id.clone()],
            ..Default::default()
        },
    )?;
    assert_eq!(
        dup, 0,
        "a near-duplicate of a case already in the set must not be imported again"
    );

    // …and errors-only over the project's own traffic finds the failed call.
    let errs = store.import_dataset_items(
        Some(pid),
        &target.id,
        &ImportSpec {
            from: ImportSource::Events,
            strategy: SamplingStrategy::Errors,
            n: 10,
            ..Default::default()
        },
    )?;
    assert!(
        errs >= 1,
        "errors-only must find the error event this section inserted; got {errs}"
    );
    let after = store.list_dataset_items(&target.id)?;
    assert!(
        after
            .iter()
            .any(|i| i.source_event_id.as_deref() == Some(bad.id.as_str())),
        "the failing call is what a regression set is made of"
    );

    assert!(
        store
            .import_dataset_items(Some(&new_id()), &target.id, &ImportSpec::default())
            .is_err(),
        "another project's scope cannot import into this dataset"
    );
    Ok(())
}

fn event(pid: &str, model: &str, status: &str, input: &str) -> LlmEvent {
    let v: Value = json!({
        "id": new_id(),
        "project_id": pid,
        "provider": "anthropic",
        "model": model,
        "status": status,
        "input": input,
        "output": "done",
        "tags": ["mined"],
    });
    serde_json::from_value(v).expect("event fixture")
}
