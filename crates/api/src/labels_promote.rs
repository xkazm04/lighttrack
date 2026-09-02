//! The golden-set half of the label ledger (M11): reading a dataset's grades, and promoting a
//! graded production event into an eval case.
//!
//! Split from [`crate::labels`] rather than living beside the ledger's own CRUD, because these two
//! are about *datasets* — they take a dataset id, run the frozen check, and answer to the eval
//! corpus rather than to the ledger.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::{new_id, DatasetItem, Label, LabelFilter, LabelSubject};

use crate::datasets::load_dataset_authorized;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// `GET /v1/datasets/:id/labels` — every human verdict on this set's items.
///
/// The read `lt-runner calibrate --dataset` makes. Its own route rather than a `GET /v1/labels`
/// filter because it is a join the caller cannot express: labels are keyed by dataset-item id, and
/// composing it client-side is one request per case — which is what makes calibrating against a
/// stored set slower than reading a file, and keeps everyone on files.
pub(crate) async fn dataset_labels(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<Label>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let sc = p.scope_owned();
    Ok(Json(
        spawn_db(move || store.labels_for_dataset(sc.as_deref().into(), &ds.id)).await?,
    ))
}

#[derive(Deserialize)]
pub(crate) struct FromLabelReq {
    /// The label to promote. Its subject must be an event — a dataset item is already in a set, and
    /// a score is a verdict rather than a case.
    label_id: String,
}

/// `POST /v1/datasets/:id/items/from-label` — promote a labelled production event into a golden set.
///
/// The step that closes the loop: traffic gets graded in review, and the graded case becomes a
/// permanent eval case with its human verdict attached, instead of the grade evaporating in a
/// spreadsheet. The label is **copied onto the new item** (as a fresh `dataset_item` label), not
/// moved, so the original event keeps its own grade and the item's provenance names the event.
pub(crate) async fn item_from_label(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Json(req): Json<FromLabelReq>,
) -> Result<Json<DatasetItem>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let ds = load_dataset_authorized(&st, &p, &id).await?;
    if ds.frozen {
        return Err(ApiError::conflict("dataset is frozen"));
    }

    let label = find_label(&st, &ds.project_id, &req.label_id).await?;
    let LabelSubject::Event(event_id) = &label.subject else {
        return Err(ApiError::bad_request(
            "only a label on an event can be promoted into a dataset: a dataset_item label is \
             already in a set, and a score label is a verdict rather than a case",
        ));
    };
    let store = st.store.clone();
    let ev_id = event_id.clone();
    let owner = ds.project_id.clone();
    let ev = spawn_db(move || store.get_event(TenantScope::Project(&owner), &ev_id))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("event '{event_id}' not found")))?;

    let item = DatasetItem {
        id: new_id(),
        dataset_id: ds.id.clone(),
        input: as_text(ev.input.as_ref()).unwrap_or_default(),
        output: as_text(ev.output.as_ref()),
        // The human's verdict is what makes this a *golden* case, so the expectation the case
        // carries is the labeler's note when there is one — never a guess.
        expected: label.note.clone(),
        context: None,
        tags: vec!["from-label".to_string()],
        source_event_id: Some(event_id.clone()),
        anonymization: Value::Null,
    };
    let store = st.store.clone();
    let item2 = item.clone();
    spawn_db(move || store.create_dataset_item(&item2)).await?;

    // Copy the grade onto the new item. Without this the promoted case is an input with no ground
    // truth — which is exactly the state that made a "golden set" un-calibratable in the first place.
    let copied = Label {
        id: new_id(),
        project_id: ds.project_id.clone(),
        subject: LabelSubject::DatasetItem(item.id.clone()),
        rubric_id: label.rubric_id.clone(),
        value: label.value,
        pass: label.pass,
        dimensions: label.dimensions.clone(),
        labeler: label.labeler.clone(),
        note: label.note.clone(),
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    spawn_db(move || store.insert_label(&copied)).await?;
    Ok(Json(item))
}

/// A stored event payload as case text. A plain JSON string is the text itself; anything richer (a
/// message array, a structured request) is re-serialized rather than stringified through `Display`,
/// which would produce Rust's debug shape and make the case unrunnable.
fn as_text(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        other => serde_json::to_string(other).ok(),
    }
}

/// Look one label up by id, within a project. The store has no `get_label` — the ledger is listed,
/// not addressed — so this is a bounded scan of the project's newest page rather than a new trait
/// method every backend would have to port for one call site.
async fn find_label(st: &AppState, project: &str, id: &str) -> Result<Label, ApiError> {
    let filter = LabelFilter {
        project: Some(project.to_string()),
        limit: LabelFilter::MAX_LIMIT,
        ..Default::default()
    };
    let store = st.store.clone();
    let rows = spawn_db(move || store.list_labels(&filter)).await?;
    rows.into_iter()
        .find(|l| l.id == id)
        .ok_or_else(|| ApiError::not_found(format!("label '{id}' not found in this project")))
}
