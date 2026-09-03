//! Prompt registry — named, versioned prompts fetched at runtime by label (e.g. `production`).
//!
//! A new version auto-enqueues the prompt's linked benchmark (reusing the job queue); promoting a
//! label to a version is **blocked** (409) unless the latest run that scored *that version* actually
//! generated with it, did not regress against the benchmark's baseline, and — when the project
//! requires it — was judged by a judge that has been checked against a human. `force` overrides
//! all but the last. The policy itself lives in [`crate::prompts_gate`]; this module owns the
//! routes. A promotion also moves the label's ledger, which the canary sweep
//! ([`crate::prompt_canary_sweep`]) later reads to know what to fall back to.

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use lighttrack_core::{new_id, JudgeTrustVerdict, Prompt, PromptVersion, REASON_PROMOTE};

use crate::benchmarks::load_benchmark_authorized;
use crate::benchmarks_target::validate_target_matrix;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::jobs_enqueue::enqueue_bench_run;
use crate::judges;
use crate::prompts_gate::{gate_promotion, version_scored_run};
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

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

/// Longest accepted registry name or label.
const MAX_IDENT_LEN: usize = 128;

/// Is this a registry identifier we are willing to store? A prompt name is a URL path segment
/// (`/prompts/<name>`), the key a benchmark's `prompt_ref` matches on and the head of every
/// `<name>@v<n>` attribution tag; a label is a query value (`?label=`) and a ledger key. Neither
/// door validated them, so `""`, `"a b"` and `"a/b"` were all accepted — and a prompt named `a/b`
/// could be created but never fetched, its route reading as two segments. Blank, whitespace and
/// the four characters with URL meaning are refused; everything else (including `@`, which the
/// tag parser already tolerates) passes. Returns the operator-facing reason.
fn validate_ident(kind: &str, s: &str) -> Result<(), ApiError> {
    let len = s.chars().count();
    if len == 0 || len > MAX_IDENT_LEN {
        return Err(ApiError::bad_request(format!(
            "{kind} must be 1-{MAX_IDENT_LEN} characters (got {len})"
        )));
    }
    if let Some(bad) = s
        .chars()
        .find(|c| c.is_whitespace() || matches!(c, '/' | '?' | '#' | '%'))
    {
        return Err(ApiError::bad_request(format!(
            "{kind} may not contain whitespace or '/', '?', '#', '%' (found {bad:?})"
        )));
    }
    Ok(())
}

pub(crate) async fn create_prompt(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreatePromptReq>,
) -> Result<Json<CreatedPrompt>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    ensure_can_admin(&p)?;
    validate_ident("prompt name", &req.name)?;

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
        canary: None,
        label_history: Vec::new(),
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
    let sc = p.scope_owned();
    let existing = spawn_db(move || store.list_prompt_versions(sc.as_deref().into(), &id)).await?;
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
    let sc = p.scope_owned();
    let v = spawn_db(move || store.list_prompt_versions(sc.as_deref().into(), &prompt.id)).await?;
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
        let owner = prompt.project_id.clone();
        let v = spawn_db(move || store.list_prompt_versions(TenantScope::Project(&owner), &id))
            .await?
            .iter()
            .map(|x| x.version)
            .max()
            .ok_or_else(|| ApiError::not_found(format!("'{name}' has no versions")))?;
        (v, None)
    };

    let store = st.store.clone();
    let id = prompt.id.clone();
    let owner = prompt.project_id.clone();
    let pv = spawn_db(move || store.get_prompt_version(TenantScope::Project(&owner), &id, version))
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
    /// The benchmark whose regression check gates this prompt's promotions. An explicit `null`
    /// unlinks; an **absent** field is a 400. The two used to be the same `None`, so `PUT {}` —
    /// a body with nothing in it — silently removed the quality gate from the prompt.
    #[serde(default, deserialize_with = "present")]
    benchmark_id: Option<Option<String>>,
}

/// Distinguish `"benchmark_id": null` (`Some(None)`) from a missing key (`None`, via the field's
/// `default`): serde collapses both into one `Option` otherwise.
fn present<'de, D: Deserializer<'de>>(d: D) -> Result<Option<Option<String>>, D::Error> {
    Option::<String>::deserialize(d).map(Some)
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
    let Some(benchmark_id) = req.benchmark_id else {
        return Err(ApiError::bad_request(
            "benchmark_id is required: a benchmark id links it, an explicit null unlinks it",
        ));
    };
    if let Some(bid) = &benchmark_id {
        // Authorize it the same way every other benchmark reference is, so a link cannot be used to
        // point one project's gate at another project's runs.
        load_benchmark_authorized(&st, &p, bid).await?;
    }
    prompt.benchmark_id = benchmark_id;
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
    /// Whether the judge behind the gating benchmark has ever been checked against a human, for
    /// the rubric it was judged under (M11). Always reported when there is a linked benchmark,
    /// including when it is `unknown` — a promotion whose evidence came from an unverified
    /// instrument should say so on the way through, not only when a policy stops it.
    #[serde(skip_serializing_if = "Option::is_none")]
    judge_trust: Option<JudgeTrustVerdict>,
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
    validate_ident("label", &req.label)?;
    let mut prompt = load_prompt(&st, &pid, &name).await?;

    // The target version must exist.
    let store = st.store.clone();
    let (id, ver) = (prompt.id.clone(), req.version);
    let sc = p.scope_owned();
    if spawn_db(move || store.get_prompt_version(sc.as_deref().into(), &id, ver))
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
    let mut judge_trust = None;
    if let Some(bid) = prompt.benchmark_id.clone() {
        let bench = load_benchmark_authorized(&st, &p, &bid).await?;
        // What the gate is really being asked: can the judge that produced this evidence be
        // believed for this rubric? Keyed on the benchmark's own (rubric, judge) pair, so a
        // benchmark that switched judges does not inherit the old one's calibration.
        let trust = judges::lookup(
            &st,
            &bench.project_id,
            bench.rubric_id.as_deref(),
            &bench.judge_model,
        )
        .await?;
        let project = judges::load_project(&st, &bench.project_id).await?;
        let trust_refusal = judges::policy_block(project.as_ref(), &trust);
        judge_trust = Some(trust);
        // Whether the benchmark can resolve this prompt at all decides whether a missing
        // `resolved_prompt_version` is a refusal or a caveat.
        let resolvable = benchmark_resolves(&bench.target, &name);
        let store = st.store.clone();
        let sc = p.scope_owned();
        let runs = spawn_db(move || store.list_benchmark_runs(sc.as_deref().into(), &bid)).await?;
        let latest = version_scored_run(&runs, &prompt.id, req.version);
        let outcome = gate_promotion(
            latest,
            bench.baseline_score,
            req.force,
            req.version,
            resolvable,
            trust_refusal,
        );
        if let Some(reason) = outcome.blocked() {
            return Err(ApiError::conflict(reason.to_string()));
        }
        warning = outcome.warning().map(str::to_string);
    }

    // The pointer and the ledger move together (`Prompt::set_label`), so a served version always
    // records how it got there — which is what an auto-revert later reads to find what to fall back
    // to, and what separates "someone decided this" from "the canary decided this".
    prompt.set_label(&req.label, req.version, REASON_PROMOTE);
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    spawn_db(move || store.update_prompt(&p2)).await?;
    Ok(Json(PromotedPrompt {
        prompt,
        warning,
        judge_trust,
    }))
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
pub(crate) async fn load_prompt(st: &AppState, pid: &str, name: &str) -> Result<Prompt, ApiError> {
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
                Some(&prompt.project_id),
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
    fn registry_identifiers_are_one_url_safe_token() {
        for ok in ["support-reply", "v2.final", "a@b", "prod_eu", "canary"] {
            assert!(validate_ident("prompt name", ok).is_ok(), "{ok:?}");
        }
        for bad in ["", "   ", "a b", "a/b", "a?b", "a#b", "100%", "	a"] {
            assert!(validate_ident("label", bad).is_err(), "{bad:?}");
        }
        assert!(validate_ident("label", &"x".repeat(MAX_IDENT_LEN)).is_ok());
        assert!(validate_ident("label", &"x".repeat(MAX_IDENT_LEN + 1)).is_err());
    }

    /// `PUT {}` used to unlink: the absent key and an explicit `null` both read as `None`.
    #[test]
    fn an_absent_benchmark_id_is_not_an_unlink() {
        let absent: LinkReq = serde_json::from_str("{}").unwrap();
        assert!(absent.benchmark_id.is_none(), "missing key");
        let null: LinkReq = serde_json::from_str(r#"{"benchmark_id": null}"#).unwrap();
        assert_eq!(null.benchmark_id, Some(None), "explicit unlink");
        let set: LinkReq = serde_json::from_str(r#"{"benchmark_id": "b1"}"#).unwrap();
        assert_eq!(set.benchmark_id, Some(Some("b1".into())));
    }

    #[test]
    fn next_version_increments_from_max() {
        assert_eq!(next_version(&[]), 1, "first version is 1");
        // Order-independent: max + 1, not count + 1.
        assert_eq!(next_version(&[pv(2), pv(1), pv(3)]), 4);
    }
}
