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
use lighttrack_store::Admission;

use crate::error::ApiError;
use crate::events::{breach_reason, on_admission, prepare_event};
use crate::events_validate;
use crate::guards::{authenticate, resolve_ingest_project};
use crate::state::{spawn_db, AppState};

/// One item's outcome, tagged so a client can branch on `status`:
/// - `accepted`: stored (with its resolved `cost_usd`);
/// - `rejected`: an enforcing limit breach turned it away (not stored) — the same condition the
///   single-event path returns 429 for;
/// - `invalid`: it failed validation or a store constraint (e.g. duplicate id) — the batch analogue
///   of a 400/409. Never stored.
#[derive(Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum BatchItem {
    Accepted {
        id: String,
        cost_usd: Option<f64>,
        #[serde(skip_serializing_if = "Vec::is_empty")]
        breached: Vec<LimitStatus>,
    },
    Rejected {
        id: String,
        reason: String,
        breached: Vec<LimitStatus>,
    },
    Invalid {
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
    Json(mut evs): Json<Vec<LlmEvent>>,
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
    // items get their result now; valid ones are collected (with their original index) for the single
    // admission-controlled store call.
    let mut results: Vec<Option<BatchItem>> = Vec::with_capacity(evs.len());
    let mut valid: Vec<LlmEvent> = Vec::new();
    let mut valid_idx: Vec<usize> = Vec::new();
    for (i, ev) in evs.iter_mut().enumerate() {
        let pid = match resolve_ingest_project(&principal, &ev.project_id) {
            Ok(p) => p,
            // The only failure mode: an admin/dev caller left project_id blank on this item.
            Err(_) => {
                results.push(Some(BatchItem::Invalid {
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
                    reason: format!("could not resolve project policy: {e}"),
                }));
                continue;
            }
        };
        match prepare_event(&st, ev, &pid, persistence) {
            Ok(()) => {
                valid_idx.push(i);
                valid.push(ev.clone());
                results.push(None); // filled after admission
            }
            Err(msg) => results.push(Some(BatchItem::Invalid { reason: msg })),
        }
    }

    // One critical section: admission for each item counts previously-accepted items of this batch.
    let admissions: Vec<Result<Admission, _>> = if valid.is_empty() {
        Vec::new()
    } else {
        let to_insert = valid.clone();
        let store = st.store.clone();
        spawn_db(move || Ok(store.insert_events_checked(&to_insert))).await?
    };

    for (k, admission) in admissions.into_iter().enumerate() {
        let ev = &valid[k];
        let item = match admission {
            Ok(a) => {
                let breached = on_admission(&st, ev, &a);
                if a.admitted {
                    BatchItem::Accepted {
                        id: ev.id.clone(),
                        cost_usd: ev.cost_usd,
                        breached,
                    }
                } else {
                    BatchItem::Rejected {
                        id: ev.id.clone(),
                        reason: breach_reason(&breached),
                        breached,
                    }
                }
            }
            // A per-item store failure (e.g. duplicate id → Conflict) is reported as invalid rather
            // than aborting the batch; siblings already committed stay committed.
            Err(e) => BatchItem::Invalid { reason: e.to_string() },
        };
        results[valid_idx[k]] = Some(item);
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
