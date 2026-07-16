//! Batch event ingest: `POST /v1/events/batch`.
//!
//! Accepts a JSON array of events and returns a per-item result in the same order — multi-status
//! semantics under a single HTTP 200 (an item can be accepted, rejected for a limit breach, or invalid
//! while its siblings succeed, so no single status code fits the whole request). Each item runs the
//! exact same pipeline as the single-event path — auth, project scoping, validation, PII redaction,
//! cost-fill, and limit admission with alert fan-out — by sharing `events::prepare_event` /
//! `on_admission`. The store call is one critical section (see [`Store::insert_events_checked`]) so a
//! batch cannot bypass a cap: admission for each item counts the previously-accepted items ahead of it.

use axum::{extract::State, http::HeaderMap, Json};
use serde::Serialize;

use lighttrack_core::{LimitStatus, LlmEvent};
use lighttrack_store::{Admission, StoreError};

use crate::error::ApiError;
use crate::events::{breach_reason, on_admission, prepare_event, same_logical_event};
use crate::events_validate;
use crate::guards::{authenticate, resolve_ingest_project};
use crate::state::{spawn_db, AppState};

/// One item's outcome, tagged so a client can branch on `status`:
/// - `accepted`: stored (with its resolved `cost_usd`) — or an acknowledged **replay** of an
///   already-stored event (`duplicate: true`), so a retried batch never double-counts;
/// - `rejected`: an enforcing limit breach turned it away (not stored) — the same condition the
///   single-event path returns 429 for;
/// - `invalid`: it failed validation or a store constraint — the batch analogue of a 400/409.
///   Never stored.
///
/// Every variant carries `index` (the item's position in the request array), so positional
/// correlation is explicit rather than load-bearing-but-unstated, and non-accepted variants carry a
/// stable machine-readable `code` (the same taxonomy the single-event path returns:
/// `bad_request` | `conflict` | `rate_limited` | `internal`) so a client can branch without
/// substring-matching English prose.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum BatchItem {
    Accepted {
        index: usize,
        id: String,
        cost_usd: Option<f64>,
        /// `true` when this item was a replay of an already-recorded event (same id, same logical
        /// payload): the original outcome is acknowledged, nothing is double-counted.
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        duplicate: bool,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        breached: Vec<LimitStatus>,
    },
    Rejected {
        index: usize,
        id: String,
        code: &'static str,
        reason: String,
        breached: Vec<LimitStatus>,
    },
    Invalid {
        index: usize,
        /// The client-supplied event id, when the item was well-formed enough to have one.
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
        code: &'static str,
        reason: String,
    },
}

#[derive(Serialize)]
pub(crate) struct BatchResponse {
    accepted: usize,
    rejected: usize,
    invalid: usize,
    results: Vec<BatchItem>,
}

pub(crate) async fn post_batch(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(evs): Json<Vec<LlmEvent>>,
) -> Result<Json<BatchResponse>, ApiError> {
    let principal = authenticate(&st, &headers).await?;

    if evs.is_empty() {
        return Err(ApiError::bad_request("batch must contain at least one event"));
    }
    let cap = events_validate::max_batch();
    if evs.len() > cap {
        return Err(ApiError::bad_request(format!(
            "batch of {} events exceeds the limit of {cap} (see LIGHTTRACK_MAX_BATCH)",
            evs.len()
        )));
    }

    // Prepare every item up front (project scoping + validation + redaction + cost-fill). Invalid
    // items get their result now; valid ones are MOVED (not cloned — an 8 MiB batch of prompt/output
    // Value trees is expensive to deep-copy) into the insert set with their original index.
    let mut results: Vec<Option<BatchItem>> = Vec::with_capacity(evs.len());
    let mut valid: Vec<LlmEvent> = Vec::new();
    let mut valid_idx: Vec<usize> = Vec::new();
    for (i, mut ev) in evs.into_iter().enumerate() {
        let item_id = (!ev.id.is_empty()).then(|| ev.id.clone());
        let pid = match resolve_ingest_project(&principal, &ev.project_id) {
            Ok(p) => p,
            // The only failure mode: an admin/dev caller left project_id blank on this item.
            Err(_) => {
                results.push(Some(BatchItem::Invalid {
                    index: i,
                    id: item_id,
                    code: "bad_request",
                    reason: "project_id is required".to_string(),
                }));
                continue;
            }
        };
        // Per-item policy lookup is a cache hit after the first event of each project in the batch.
        let persistence = match crate::state::redaction_policy_for(&st, &pid).await {
            Ok(p) => p,
            Err(e) => {
                results.push(Some(BatchItem::Invalid {
                    index: i,
                    id: item_id,
                    code: "internal",
                    reason: format!("could not resolve project policy: {e}"),
                }));
                continue;
            }
        };
        match prepare_event(&st, &mut ev, &pid, persistence) {
            Ok(()) => {
                valid_idx.push(i);
                valid.push(ev);
                results.push(None); // filled after admission
            }
            Err(msg) => results.push(Some(BatchItem::Invalid {
                index: i,
                id: item_id,
                code: "bad_request",
                reason: msg,
            })),
        }
    }

    // One critical section: admission for each item counts previously-accepted items of this batch.
    // `valid` is moved into the closure and handed back — not cloned a second time just to satisfy
    // the 'static bound.
    let (valid, admissions): (Vec<LlmEvent>, Vec<Result<Admission, _>>) = if valid.is_empty() {
        (valid, Vec::new())
    } else {
        let store = st.store.clone();
        spawn_db(move || {
            let admissions = store.insert_events_checked(&valid);
            Ok((valid, admissions))
        })
        .await?
    };

    for (k, admission) in admissions.into_iter().enumerate() {
        let ev = &valid[k];
        let index = valid_idx[k];
        let item = match admission {
            Ok(a) => {
                let breached = on_admission(&st, ev, &a);
                if a.admitted {
                    BatchItem::Accepted {
                        index,
                        id: ev.id.clone(),
                        cost_usd: ev.cost_usd,
                        duplicate: false,
                        breached,
                    }
                } else {
                    BatchItem::Rejected {
                        index,
                        id: ev.id.clone(),
                        code: "rate_limited",
                        reason: breach_reason(&breached),
                        breached,
                    }
                }
            }
            // A duplicate id is a RETRY until proven otherwise: same logical payload → acknowledge
            // the original write as accepted+duplicate (nothing double-counted), so a client whose
            // batch timed out after the server committed can resend the whole batch safely.
            // A different payload under the same id is a true conflict.
            Err(StoreError::Conflict(_)) => {
                let store = st.store.clone();
                let id = ev.id.clone();
                let stored = spawn_db(move || store.get_event(&id)).await.ok().flatten();
                match stored {
                    Some(s) if same_logical_event(&s, ev) => BatchItem::Accepted {
                        index,
                        id: ev.id.clone(),
                        cost_usd: s.cost_usd,
                        duplicate: true,
                        breached: Vec::new(),
                    },
                    _ => BatchItem::Invalid {
                        index,
                        id: Some(ev.id.clone()),
                        code: "conflict",
                        reason: format!("event '{}' already exists with a different payload", ev.id),
                    },
                }
            }
            // Any other per-item store failure is reported as invalid rather than aborting the
            // batch; siblings already committed stay committed. The raw store error goes to the log,
            // not the wire (it is an internal detail on the product's most public surface).
            Err(e) => {
                eprintln!("[BATCH] item {index} store error: {e}");
                BatchItem::Invalid {
                    index,
                    id: Some(ev.id.clone()),
                    code: "internal",
                    reason: "store error (see server logs)".to_string(),
                }
            }
        };
        results[index] = Some(item);
    }

    // Every slot is now filled (valid items from admission, invalid ones from preparation).
    let results: Vec<BatchItem> = results.into_iter().map(|o| o.expect("slot filled")).collect();
    let (mut accepted, mut rejected, mut invalid) = (0, 0, 0);
    for r in &results {
        match r {
            BatchItem::Accepted { .. } => accepted += 1,
            BatchItem::Rejected { .. } => rejected += 1,
            BatchItem::Invalid { .. } => invalid += 1,
        }
    }
    Ok(Json(BatchResponse { accepted, rejected, invalid, results }))
}
