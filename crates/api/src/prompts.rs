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
use crate::benchmarks_target::validate_target_matrix;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::jobs_enqueue::enqueue_bench_run;
use crate::prompts_gate::{gate_promotion, version_scored_run};
use crate::state::{spawn_db, AppState};

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
    if spawn_db(move || store.get_prompt(&pid_c, &name_c))
        .await?
        .is_some()
    {
        return Err(ApiError::conflict(format!(
            "prompt '{}' already exists",
            req.name
        )));
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
    Ok(Json(CreatedPrompt {
        prompt,
        version,
        enqueued_job,
    }))
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
    Ok(Json(AddedVersion {
        version,
        enqueued_job,
    }))
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
    /// The prompt's stable id — returned so a client can attribute the traffic this resolution
    /// produces back to the registry entry.
    id: String,
    name: String,
    version: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    label: Option<String>,
    /// Ready-to-stamp attribution tag, `"<name>@v<version>"`. **The convention:** put this on every
    /// event produced with this prompt as `metadata.prompt` (exactly like `metadata.customer_id`),
    /// and `GET /v1/costs/prompts` answers "did v4 cost less than v3 in production?" — without it,
    /// served versions are never attributed to the traffic they produce.
    tag: String,
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
        let v =
            prompt.labels.get(&lbl).copied().ok_or_else(|| {
                ApiError::not_found(format!("label '{lbl}' is not set on '{name}'"))
            })?;
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
        id: prompt.id,
        tag: format!("{}@v{}", prompt.name, pv.version),
        name: prompt.name,
        version: pv.version,
        label,
        content: pv.content,
        config: pv.config,
        note: pv.note,
    }))
}

#[derive(Deserialize)]
pub(crate) struct LinkReq {
    /// The benchmark whose regression check gates this prompt's promotions. `null` unlinks.
    #[serde(default)]
    benchmark_id: Option<String>,
}

/// Point an existing prompt at the benchmark that gates it.
///
/// The link could only be set when the prompt was created, which made the resolvable-target setup
/// impossible to express: a benchmark with a `prompt_ref` needs the prompt to exist first, and the
/// prompt's `benchmark_id` needs the benchmark to exist first. One of the two had to become
/// settable afterwards, and this is the harmless half — the benchmark's target matrix stays
/// immutable, which is what a stored baseline depends on.
pub(crate) async fn link_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Json(req): Json<LinkReq>,
) -> Result<Json<Prompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut prompt = load_prompt(&st, &pid, &name).await?;
    if let Some(bid) = &req.benchmark_id {
        // Authorize it the same way every other benchmark reference is, so a link cannot be used to
        // point one project's gate at another project's runs.
        load_benchmark_authorized(&st, &p, bid).await?;
    }
    prompt.benchmark_id = req.benchmark_id;
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    spawn_db(move || store.update_prompt(&p2)).await?;
    Ok(Json(prompt))
}

#[derive(Deserialize)]
pub(crate) struct PromoteReq {
    label: String,
    version: u32,
    /// Override the regression gate (e.g. an intentional rollout despite a dip).
    #[serde(default)]
    force: bool,
}

/// The promoted prompt, plus anything the gate could not verify. `warning` sits alongside a
/// flattened `Prompt` rather than replacing the body, so a client that already reads a prompt here
/// keeps working.
#[derive(Serialize)]
pub(crate) struct PromotedPrompt {
    #[serde(flatten)]
    prompt: Prompt,
    /// Set when the gate allowed the promotion without being able to check that the benchmark run
    /// actually generated with this version — see [`crate::prompts_gate`].
    #[serde(skip_serializing_if = "Option::is_none")]
    warning: Option<String>,
}

/// Point a label at a version. Blocked (409) when the prompt's linked benchmark has regressed
/// against its baseline, or when the run backing the promotion did not generate with the version
/// being promoted, unless `force` is set.
pub(crate) async fn promote(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path((pid, name)): Path<(String, String)>,
    Json(req): Json<PromoteReq>,
) -> Result<Json<PromotedPrompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    let mut prompt = load_prompt(&st, &pid, &name).await?;

    // The target version must exist.
    let store = st.store.clone();
    let (id, ver) = (prompt.id.clone(), req.version);
    if spawn_db(move || store.get_prompt_version(&id, ver))
        .await?
        .is_none()
    {
        return Err(ApiError::not_found(format!(
            "'{name}' has no version {}",
            req.version
        )));
    }

    // The gate: the latest run of the linked benchmark that scored THE VERSION BEING PROMOTED must
    // have actually generated with that version, and must not have regressed against the baseline.
    let mut warning = None;
    if let Some(bid) = prompt.benchmark_id.clone() {
        let bench = load_benchmark_authorized(&st, &p, &bid).await?;
        // Whether the benchmark can resolve this prompt at all decides whether a missing
        // `resolved_prompt_version` is a refusal or a caveat.
        let resolvable = benchmark_resolves(&bench.target, &name);
        let store = st.store.clone();
        let runs = spawn_db(move || store.list_benchmark_runs(&bid)).await?;
        let latest = version_scored_run(&runs, &prompt.id, req.version);
        let outcome = gate_promotion(
            latest,
            bench.baseline_score,
            req.force,
            req.version,
            resolvable,
        );
        if let Some(reason) = outcome.blocked() {
            return Err(ApiError::conflict(reason.to_string()));
        }
        warning = outcome.warning().map(str::to_string);
    }

    prompt.labels.insert(req.label, req.version);
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    spawn_db(move || store.update_prompt(&p2)).await?;
    Ok(Json(PromotedPrompt { prompt, warning }))
}

/// Does this benchmark's target matrix name `prompt_name` in a `prompt_ref`? Only then can a run of
/// it have fetched the version's content, so only then does the gate demand proof that it did.
///
/// A `target` that no longer parses as a matrix (hand-edited, or written by a newer build) reads as
/// "cannot resolve" — the advisory path — rather than as an error on a promotion route.
fn benchmark_resolves(target: &Value, prompt_name: &str) -> bool {
    validate_target_matrix(target)
        .unwrap_or_default()
        .iter()
        .any(|t| t.prompt_ref.as_ref().is_some_and(|r| r.name == prompt_name))
}

/// Load a prompt by `(project, name)`, scoped to the path project, or 404.
async fn load_prompt(st: &AppState, pid: &str, name: &str) -> Result<Prompt, ApiError> {
    let store = st.store.clone();
    let (pid, name2) = (pid.to_string(), name.to_string());
    spawn_db(move || store.get_prompt(&pid, &name2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("prompt '{name}' not found")))
}

/// Auto-enqueue the prompt's linked benchmark (if any) for the just-created version.
///
/// The payload carries the prompt's **name** as well as its id and version: the id is provenance
/// (which registry entry this run is about), while the name is the key a target's `prompt_ref`
/// matches on — so it is what lets the runner apply this version as an override to the right target
/// of a multi-target matrix. Returns the job id when enqueued.
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
                serde_json::json!({
                    "prompt_id": prompt.id,
                    "prompt_name": prompt.name,
                    "version": version,
                }),
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
}
