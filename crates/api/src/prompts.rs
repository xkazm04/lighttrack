//! Prompt registry — named, versioned prompts fetched at runtime by label (e.g. `production`).
//!
//! A new version auto-enqueues the prompt's linked benchmark (reusing the job queue); promoting a
//! label to a version is **blocked** when that benchmark's latest mean score has regressed against
//! its baseline — turning a prompt edit into a gated, measurable quality step.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use lighttrack_core::{new_id, Prompt, PromptVersion};

use crate::benchmarks::load_benchmark_authorized;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::jobs::enqueue_bench_run;
use crate::state::{spawn_db, AppState};

const EPS: f64 = 1e-9;

#[derive(Deserialize)]
pub(crate) struct CreatePromptReq {
    name: String,
    #[serde(default)]
    benchmark_id: Option<String>,
    /// Content of the initial version (version 1).
    content: String,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct CreatedPrompt {
    prompt: Prompt,
    version: PromptVersion,
    /// The auto-enqueued benchmark job id, if the prompt is linked to a benchmark.
    #[serde(skip_serializing_if = "Option::is_none")]
    enqueued_job: Option<String>,
}

pub(crate) async fn create_prompt(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreatePromptReq>,
) -> Result<Json<CreatedPrompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;

    // Reject a duplicate registry name within the project.
    let store = st.store.clone();
    let (pid_c, name_c) = (pid.clone(), req.name.clone());
    if spawn_db(move || store.get_prompt(&pid_c, &name_c)).await?.is_some() {
        return Err(ApiError::conflict(format!("prompt '{}' already exists", req.name)));
    }
    // Validate the linked benchmark exists and belongs to the caller, if given.
    if let Some(bid) = &req.benchmark_id {
        load_benchmark_authorized(&st, &p, bid).await?;
    }

    let now = Utc::now();
    let prompt = Prompt {
        id: new_id(),
        project_id: pid,
        name: req.name,
        benchmark_id: req.benchmark_id,
        labels: Default::default(),
        created_at: now,
        updated_at: now,
    };
    let version = PromptVersion {
        id: new_id(),
        prompt_id: prompt.id.clone(),
        version: 1,
        content: req.content,
        config: req.config,
        note: req.note,
        created_at: now,
    };
    let store = st.store.clone();
    let (p2, v2) = (prompt.clone(), version.clone());
    spawn_db(move || {
        store.create_prompt(&p2)?;
        store.create_prompt_version(&v2)
    })
    .await?;

    let enqueued_job = maybe_enqueue(&st, &prompt, version.version).await?;
    Ok(Json(CreatedPrompt { prompt, version, enqueued_job }))
}

pub(crate) async fn list_prompts(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Prompt>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_prompts(&pid)).await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct AddVersionReq {
    content: String,
    #[serde(default)]
    config: Value,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct AddedVersion {
    version: PromptVersion,
    #[serde(skip_serializing_if = "Option::is_none")]
    enqueued_job: Option<String>,
}

pub(crate) async fn add_version(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Json(req): Json<AddVersionReq>,
) -> Result<Json<AddedVersion>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let prompt = load_prompt(&st, &pid, &name).await?;

    // Next monotonic version = max existing + 1.
    let store = st.store.clone();
    let id = prompt.id.clone();
    let existing = spawn_db(move || store.list_prompt_versions(&id)).await?;
    let next = next_version(&existing);

    let version = PromptVersion {
        id: new_id(),
        prompt_id: prompt.id.clone(),
        version: next,
        content: req.content,
        config: req.config,
        note: req.note,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let v2 = version.clone();
    spawn_db(move || store.create_prompt_version(&v2)).await?;

    let enqueued_job = maybe_enqueue(&st, &prompt, version.version).await?;
    Ok(Json(AddedVersion { version, enqueued_job }))
}

pub(crate) async fn list_versions(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
) -> Result<Json<Vec<PromptVersion>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let prompt = load_prompt(&st, &pid, &name).await?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_prompt_versions(&prompt.id)).await?;
    Ok(Json(v))
}

#[derive(Deserialize)]
pub(crate) struct FetchParams {
    label: Option<String>,
    version: Option<u32>,
}

#[derive(Serialize)]
pub(crate) struct ResolvedPrompt {
    name: String,
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    content: String,
    #[serde(skip_serializing_if = "Value::is_null")]
    config: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Runtime fetch: resolve a prompt to a concrete version by explicit `?version=`, by `?label=`
/// (e.g. `production`), or — absent both — the latest version. The hot path apps call at startup.
pub(crate) async fn get_prompt(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Query(q): Query<FetchParams>,
) -> Result<Json<ResolvedPrompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let prompt = load_prompt(&st, &pid, &name).await?;

    let (version, label) = if let Some(v) = q.version {
        (v, None)
    } else if let Some(lbl) = q.label {
        let v = prompt
            .labels
            .get(&lbl)
            .copied()
            .ok_or_else(|| ApiError::not_found(format!("label '{lbl}' is not set on '{name}'")))?;
        (v, Some(lbl))
    } else {
        let store = st.store.clone();
        let id = prompt.id.clone();
        let v = spawn_db(move || store.list_prompt_versions(&id))
            .await?
            .iter()
            .map(|x| x.version)
            .max()
            .ok_or_else(|| ApiError::not_found(format!("'{name}' has no versions")))?;
        (v, None)
    };

    let store = st.store.clone();
    let id = prompt.id.clone();
    let pv = spawn_db(move || store.get_prompt_version(&id, version))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("'{name}' has no version {version}")))?;
    Ok(Json(ResolvedPrompt {
        name: prompt.name,
        version: pv.version,
        label,
        content: pv.content,
        config: pv.config,
        note: pv.note,
    }))
}

#[derive(Deserialize)]
pub(crate) struct PromoteReq {
    label: String,
    version: u32,
    /// Override the regression gate (e.g. an intentional rollout despite a dip).
    #[serde(default)]
    force: bool,
}

/// Point a label at a version. Blocked (409) when the prompt's linked benchmark has regressed
/// against its baseline, unless `force` is set.
pub(crate) async fn promote(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Json(req): Json<PromoteReq>,
) -> Result<Json<Prompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut prompt = load_prompt(&st, &pid, &name).await?;

    // The target version must exist.
    let store = st.store.clone();
    let (id, ver) = (prompt.id.clone(), req.version);
    if spawn_db(move || store.get_prompt_version(&id, ver)).await?.is_none() {
        return Err(ApiError::not_found(format!("'{name}' has no version {}", req.version)));
    }

    // Regression gate: compare the linked benchmark's latest scored run against its baseline.
    if let Some(bid) = prompt.benchmark_id.clone() {
        let bench = load_benchmark_authorized(&st, &p, &bid).await?;
        let store = st.store.clone();
        let runs = spawn_db(move || store.list_benchmark_runs(&bid)).await?;
        let latest = runs.iter().find_map(|r| r.mean_score);
        if let Some(reason) = gate_promotion(latest, bench.baseline_score, req.force) {
            return Err(ApiError::conflict(reason));
        }
    }

    prompt.labels.insert(req.label, req.version);
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    spawn_db(move || store.update_prompt(&p2)).await?;
    Ok(Json(prompt))
}

/// Load a prompt by `(project, name)`, scoped to the path project, or 404.
async fn load_prompt(st: &AppState, pid: &str, name: &str) -> Result<Prompt, ApiError> {
    let store = st.store.clone();
    let (pid, name2) = (pid.to_string(), name.to_string());
    spawn_db(move || store.get_prompt(&pid, &name2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("prompt '{name}' not found")))
}

/// Auto-enqueue the prompt's linked benchmark (if any) for the just-created version, tagging the job
/// payload with the prompt + version for traceability. Returns the job id when enqueued.
async fn maybe_enqueue(
    st: &AppState,
    prompt: &Prompt,
    version: u32,
) -> Result<Option<String>, ApiError> {
    match &prompt.benchmark_id {
        Some(bid) => {
            let job = enqueue_bench_run(
                st,
                bid,
                serde_json::json!({ "prompt_id": prompt.id, "version": version }),
            )
            .await?;
            Ok(Some(job.id))
        }
        None => Ok(None),
    }
}

/// Next monotonic version for a prompt = highest existing + 1 (1 when there are none yet).
fn next_version(existing: &[PromptVersion]) -> u32 {
    existing.iter().map(|v| v.version).max().unwrap_or(0) + 1
}

/// The regression gate that turns promotion into a measurable quality step. Given the linked
/// benchmark's most recent scored mean and its `baseline`, decide whether to block. Returns
/// `Some(reason)` (→ 409) when promotion must be refused, `None` when it may proceed.
///
/// - `force` overrides everything.
/// - No `baseline` → nothing to compare against, allow.
/// - `baseline` set but no scored run yet → block (an unverified promotion defeats the gate).
/// - latest mean below baseline → block as a regression.
fn gate_promotion(latest_mean: Option<f64>, baseline: Option<f64>, force: bool) -> Option<String> {
    if force {
        return None;
    }
    let baseline = baseline?;
    match latest_mean {
        Some(mean) if mean + EPS < baseline => Some(format!(
            "promotion blocked: benchmark mean {mean:.3} regressed below baseline {baseline:.3} (pass force=true to override)"
        )),
        None => Some(
            "promotion blocked: linked benchmark has no scored run yet (run it before promoting, or pass force=true)"
                .to_string(),
        ),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::new_id;

    fn pv(version: u32) -> PromptVersion {
        PromptVersion {
            id: new_id(),
            prompt_id: "p".into(),
            version,
            content: "c".into(),
            config: Value::Null,
            note: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn next_version_increments_from_max() {
        assert_eq!(next_version(&[]), 1, "first version is 1");
        // Order-independent: max + 1, not count + 1.
        assert_eq!(next_version(&[pv(2), pv(1), pv(3)]), 4);
    }

    #[test]
    fn gate_allows_when_no_baseline_or_forced() {
        assert!(gate_promotion(Some(0.1), None, false).is_none(), "no baseline → allow");
        assert!(gate_promotion(None, Some(0.9), true).is_none(), "force overrides a block");
        assert!(gate_promotion(Some(0.1), Some(0.9), true).is_none(), "force overrides a regression");
    }

    #[test]
    fn gate_blocks_regression_and_unscored() {
        assert!(gate_promotion(None, Some(0.8), false).is_some(), "baseline but no run → block");
        assert!(gate_promotion(Some(0.79), Some(0.8), false).is_some(), "below baseline → block");
        assert!(gate_promotion(Some(0.8), Some(0.8), false).is_none(), "meeting baseline → allow");
        assert!(gate_promotion(Some(0.95), Some(0.8), false).is_none(), "above baseline → allow");
    }
}
