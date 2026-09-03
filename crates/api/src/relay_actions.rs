//! Relay **actions** as first-class evaluation subjects (M19).
//!
//! An action's `prompt.md` lives on the device and is edited in place: no version, no fingerprint,
//! no change note. The prompt registry versions an app's prompts and gates their promotion on a
//! benchmark; the relay's own prompts — the ones LightTrack actually originates traffic with — had
//! none of that, so one could regress for months and the only evidence was a vaguely worse result.
//!
//! Two doors close that, and **neither adds a table**:
//!
//! - `GET  /v1/relay/actions` — the fingerprint ledger, derived from the settle events themselves:
//!   distinct `action_type × prompt_sha256`, with first/last seen, a run count, and the declared
//!   `action_version`. "This action's prompt changed on the 14th and the failures start on the
//!   14th" is a question you can now ask.
//! - `POST /v1/relay/actions/:action_type/dataset` — snapshot what the action actually did
//!   (`payload → input`, `result → output`) into a dataset, so a benchmark can be linked to it and
//!   the next prompt edit is gated the same way a registry prompt's is.
//!
//! `:action_type` is namespaced (`xprice/reprice-summary`), so its `/` is **percent-encoded** in
//! the path: `POST /v1/relay/actions/xprice%2Freprice-summary/dataset`.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use lighttrack_core::{input_fingerprint, new_id, Dataset, DatasetItem, LlmEvent};
use lighttrack_store::EventFilter;

use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// How many settle events one ledger read may walk, and the page it walks them in. The ledger is a
/// derived view, so its honesty depends on saying when it stopped rather than on scanning forever.
const DEFAULT_SCAN: usize = 1_000;
const MAX_SCAN: usize = 20_000;
const PAGE: usize = 1_000;

#[derive(Deserialize)]
pub(crate) struct LedgerParams {
    project: Option<String>,
    /// Maximum settle events to walk (default 1000, cap 20000).
    limit: Option<usize>,
}

/// One `action_type × prompt_sha256` pair, as the settle events record it.
#[derive(Serialize)]
pub(crate) struct ActionFingerprint {
    action_type: String,
    /// `None` for a run reported by an agent older than M19 — which is a real and different answer
    /// from "the prompt is empty", and is why it is not defaulted to a string.
    prompt_sha256: Option<String>,
    /// Every distinct `version` the action declared while running this prompt text. More than one
    /// means the label was bumped without the prompt changing; none means the action declares no
    /// version and the fingerprint is all there is.
    versions: Vec<String>,
    runs: u64,
    errors: u64,
    /// How many of those runs carry content — i.e. how many a judge can actually read. `0` on an
    /// action that has not set `report_io`.
    judgeable: u64,
    first_seen: DateTime<Utc>,
    last_seen: DateTime<Utc>,
}

/// The derived fingerprint ledger.
#[derive(Serialize)]
pub(crate) struct ActionLedger {
    actions: Vec<ActionFingerprint>,
    /// How many settle events were walked, and whether the walk hit its ceiling. A truncated
    /// ledger that did not say so would read as "this action has one prompt" when it has three.
    scanned: usize,
    truncated: bool,
}

pub(crate) async fn list_actions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LedgerParams>,
) -> Result<Json<ActionLedger>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let budget = q.limit.unwrap_or(DEFAULT_SCAN).clamp(1, MAX_SCAN);

    let mut cursor: Option<String> = None;
    let mut scanned = 0usize;
    let mut acc: Vec<(Key, Agg)> = Vec::new();
    let truncated = loop {
        let filter = EventFilter {
            tag: Some("relay".to_string()),
            cursor: cursor.clone(),
            ..Default::default()
        };
        let store = st.store.clone();
        let proj = project.clone();
        let want = PAGE.min(budget - scanned);
        let page =
            spawn_db(move || store.list_events_filtered(proj.as_deref().into(), &filter, want))
                .await?;
        scanned += page.events.len();
        for ev in &page.events {
            fold(&mut acc, ev);
        }
        cursor = page.next_cursor;
        // Out of budget with pages left is the only truncation worth reporting; running out of
        // events is just the end of the ledger.
        match &cursor {
            Some(_) if scanned >= budget => break true,
            Some(_) => {}
            None => break false,
        }
    };

    let mut actions: Vec<ActionFingerprint> = acc.into_iter().map(|(k, a)| a.finish(k)).collect();
    // Newest activity first: the question this answers is "what has been running lately".
    actions.sort_by(|a, b| {
        b.last_seen
            .cmp(&a.last_seen)
            .then_with(|| a.action_type.cmp(&b.action_type))
    });
    Ok(Json(ActionLedger {
        actions,
        scanned,
        truncated,
    }))
}

type Key = (String, Option<String>);

#[derive(Default)]
struct Agg {
    versions: Vec<String>,
    runs: u64,
    errors: u64,
    judgeable: u64,
    first_seen: Option<DateTime<Utc>>,
    last_seen: Option<DateTime<Utc>>,
}

impl Agg {
    fn finish(self, (action_type, prompt_sha256): Key) -> ActionFingerprint {
        let fallback = Utc::now();
        ActionFingerprint {
            action_type,
            prompt_sha256,
            versions: self.versions,
            runs: self.runs,
            errors: self.errors,
            judgeable: self.judgeable,
            first_seen: self.first_seen.unwrap_or(fallback),
            last_seen: self.last_seen.unwrap_or(fallback),
        }
    }
}

/// Fold one settle event into the ledger. An event whose metadata names no `action_type` is not a
/// relay run — a caller can tag anything `relay` — and is skipped rather than bucketed under an
/// empty name.
fn fold(acc: &mut Vec<(Key, Agg)>, ev: &LlmEvent) {
    let Some(action_type) = ev.metadata.get("action_type").and_then(Value::as_str) else {
        return;
    };
    let sha = ev
        .metadata
        .get("prompt_sha256")
        .and_then(Value::as_str)
        .map(str::to_string);
    let key: Key = (action_type.to_string(), sha);
    let slot = match acc.iter_mut().find(|(k, _)| *k == key) {
        Some((_, a)) => a,
        None => {
            acc.push((key, Agg::default()));
            &mut acc.last_mut().expect("just pushed").1
        }
    };
    slot.runs += 1;
    if ev.status == lighttrack_core::Status::Error {
        slot.errors += 1;
    }
    if ev.input.is_some() && ev.output.is_some() {
        slot.judgeable += 1;
    }
    if let Some(v) = ev.metadata.get("action_version").and_then(Value::as_str) {
        if !slot.versions.iter().any(|x| x == v) {
            slot.versions.push(v.to_string());
        }
    }
    slot.first_seen = Some(slot.first_seen.map_or(ev.ts, |t| t.min(ev.ts)));
    slot.last_seen = Some(slot.last_seen.map_or(ev.ts, |t| t.max(ev.ts)));
}

#[derive(Deserialize)]
pub(crate) struct SnapshotReq {
    project_id: String,
    /// Defaults to `relay:<action_type>`.
    #[serde(default)]
    name: Option<String>,
    /// How many succeeded tasks to snapshot (default 200, cap 1000).
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Serialize)]
pub(crate) struct SnapshotResp {
    #[serde(flatten)]
    dataset: Dataset,
    items: usize,
    /// Succeeded tasks that were read but carried no usable `(payload, result)` pair. Reported
    /// rather than silently dropped: "I snapshotted 200 runs and got 3 cases" is a fact the caller
    /// has to see before they link a benchmark to it.
    skipped: usize,
}

/// Snapshot one action's succeeded runs into a dataset.
///
/// The source is the **task**, not the settle event, on purpose: a task's `payload` and `result`
/// are there whatever the action's `report_io` says, so an action can be benchmark-gated without
/// its prompt text ever being stored in the cloud. The dataset is left unfrozen — freezing is the
/// curator's decision, after they have looked at what came out.
pub(crate) async fn snapshot_dataset(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(action_type): Path<String>,
    Json(req): Json<SnapshotReq>,
) -> Result<Json<SnapshotResp>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    if action_type.trim().is_empty() {
        return Err(ApiError::bad_request("action_type is required"));
    }
    if req.project_id.trim().is_empty() {
        return Err(ApiError::bad_request("project_id is required"));
    }
    let limit = req.limit.unwrap_or(200).clamp(1, 1000);

    let store = st.store.clone();
    let (pid, at) = (req.project_id.clone(), action_type.clone());
    let tasks = spawn_db(move || {
        store.list_relay_tasks_by_action(TenantScope::Project(&pid), &at, Some("succeeded"), limit)
    })
    .await?;

    let dataset = Dataset {
        id: new_id(),
        project_id: req.project_id.clone(),
        name: req
            .name
            .clone()
            .unwrap_or_else(|| format!("relay:{action_type}")),
        version: 1,
        frozen: false,
        // Sampled from this deployment's own traffic, which is the provenance `Dataset` says ages
        // best — these cases cannot have leaked into a model's training set.
        source: Some(format!("relay:{action_type}")),
        created_at: Utc::now(),
        parent_id: None,
    };
    let store = st.store.clone();
    let d2 = dataset.clone();
    spawn_db(move || store.create_dataset(&d2)).await?;

    let mut items = 0usize;
    let mut skipped = 0usize;
    for t in &tasks {
        let (Some(input), Some(output)) = (as_text(&t.payload), as_text(&t.result)) else {
            skipped += 1;
            continue;
        };
        let item = DatasetItem {
            id: new_id(),
            dataset_id: dataset.id.clone(),
            // The fingerprint every writer of a case stamps (M24), so a later `import --dedupe`
            // into this set can see what is already here rather than re-mining it.
            input_hash: Some(input_fingerprint(&input)),
            input,
            output: Some(output),
            expected: None,
            context: None,
            tags: vec!["relay".to_string(), action_type.clone()],
            // The relay run's event is keyed by the task id as its `trace_id`, which is the join
            // back to what was scored; the task id is the durable half of that pair.
            source_event_id: Some(t.id.clone()),
            anonymization: Value::Null,
        };
        let store = st.store.clone();
        spawn_db(move || store.create_dataset_item(&item)).await?;
        items += 1;
    }
    Ok(Json(SnapshotResp {
        dataset,
        items,
        skipped,
    }))
}

/// A task's payload/result as the text a case is made of. `null` and an empty string are both "no
/// case here" — a dataset item with an empty input is a case no judge can read.
fn as_text(v: &Value) -> Option<String> {
    let s = match v {
        Value::Null => return None,
        Value::String(s) => s.clone(),
        // A schemaless action's result is `{"text": …}`; unwrap it so the case is the model's
        // answer rather than the envelope around it.
        Value::Object(m) if m.len() == 1 => match m.get("text").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => v.to_string(),
        },
        other => other.to_string(),
    };
    (!s.trim().is_empty()).then_some(s)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn ev(action_type: &str, sha: Option<&str>, ts: DateTime<Utc>, judgeable: bool) -> LlmEvent {
        let mut e: LlmEvent = serde_json::from_value(json!({
            "project_id": "p", "provider": "anthropic", "model": "claude-code",
            "name": "relay-run", "tags": ["relay"],
            "metadata": { "action_type": action_type, "prompt_sha256": sha },
        }))
        .expect("event fixture");
        e.ts = ts;
        if judgeable {
            e.input = Some(json!("in"));
            e.output = Some(json!("out"));
        }
        e
    }

    /// The ledger's whole point: the same action under two prompt texts is two rows, with the
    /// window each one ran in. One row would hide exactly the change you went looking for.
    #[test]
    fn a_changed_prompt_is_a_separate_row_with_its_own_window() {
        let t0 = Utc::now() - chrono::Duration::days(3);
        let t1 = Utc::now() - chrono::Duration::days(1);
        let mut acc = Vec::new();
        for e in [
            ev("ns/act", Some("aa"), t0, false),
            ev("ns/act", Some("aa"), t1, true),
            ev("ns/act", Some("bb"), t1, false),
            // Tagged `relay` but not a relay run — no action_type, so not a row.
            ev("", None, t1, false),
        ] {
            fold(&mut acc, &e);
        }
        acc.retain(|((at, _), _)| !at.is_empty());
        assert_eq!(acc.len(), 2, "one row per prompt fingerprint");

        let (key, agg) = acc
            .into_iter()
            .find(|(k, _)| k.1.as_deref() == Some("aa"))
            .expect("aa");
        let row = agg.finish(key);
        assert_eq!(row.runs, 2);
        assert_eq!(row.judgeable, 1, "only the opted-in run can be judged");
        assert_eq!(row.first_seen, t0);
        assert_eq!(row.last_seen, t1);
    }

    /// A pre-M19 agent reports no fingerprint. That is its own bucket, not a fabricated one.
    #[test]
    fn an_unfingerprinted_run_is_its_own_row() {
        let mut acc = Vec::new();
        fold(&mut acc, &ev("ns/act", None, Utc::now(), false));
        fold(&mut acc, &ev("ns/act", Some("aa"), Utc::now(), false));
        assert_eq!(acc.len(), 2);
        assert!(acc.iter().any(|(k, _)| k.1.is_none()));
    }

    #[test]
    fn a_case_needs_text_on_both_sides() {
        assert_eq!(as_text(&json!({ "text": "hi" })).as_deref(), Some("hi"));
        assert_eq!(as_text(&json!("hi")).as_deref(), Some("hi"));
        assert_eq!(
            as_text(&json!({ "a": 1, "b": 2 })),
            Some("{\"a\":1,\"b\":2}".into())
        );
        assert!(as_text(&Value::Null).is_none());
        assert!(as_text(&json!("   ")).is_none());
        assert!(as_text(&json!({ "text": "" })).is_none());
    }
}
