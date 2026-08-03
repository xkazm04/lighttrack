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

use lighttrack_core::{new_id, BenchmarkRun, Prompt, PromptVersion};

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

    // Regression gate: compare the linked benchmark's latest scored run FOR THE VERSION BEING
    // PROMOTED against its baseline. Previously this took the newest scored run of any version, so
    // a green run of v3 could clear v9 for production.
    if let Some(bid) = prompt.benchmark_id.clone() {
        let bench = load_benchmark_authorized(&st, &p, &bid).await?;
        let store = st.store.clone();
        let runs = spawn_db(move || store.list_benchmark_runs(&bid)).await?;
        let latest = version_scored_run(&runs, &prompt.id, req.version);
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

/// What the gate knows about the run that scored the version being promoted: its mean, and — when
/// the runner recorded one — the upper bound of the ~95% CI on that mean, plus the run's own
/// `status`. Extracted from the run rather than recomputed, so the gate and the runner cannot drift
/// apart into two different notions of "regressed".
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct GateEvidence {
    mean: Option<f64>,
    /// `report.ci95[1]`, the runner's own upper confidence bound on the mean.
    ci_upper: Option<f64>,
    /// `true` when the runner's verdict for that run was `regressed`.
    runner_regressed: bool,
}

/// The regression gate that turns promotion into a measurable quality step. Returns `Some(reason)`
/// (→ 409) when promotion must be refused, `None` when it may proceed.
///
/// - `force` overrides everything.
/// - No `baseline` → nothing to compare against, allow.
/// - `baseline` set but no scored run yet → block (an unverified promotion defeats the gate).
/// - The runner already called the run `regressed` → block, quoting its verdict. The runner's
///   verdict is the significance-aware one (paired per-case where possible, family-wise corrected),
///   so honouring it keeps ONE definition of regression in the product.
/// - Otherwise, when the run recorded a confidence bound, block only when the whole interval sits
///   below the baseline — the same rule `stats::verdict` applies. **This is deliberately weaker than
///   the old plain `mean < baseline` compare**: that version blocked on a 0.001 dip inside the noise
///   of a 3-case run, which is a false positive on the one gate that stops a deploy. A real
///   regression (a drop larger than the run's own uncertainty) still blocks, and a *noisy* run is not
///   waved through either — a wide interval means the evidence is weak in both directions, and the
///   fix is more cases, which the report says.
/// - A run with no recorded interval (legacy, or `n < 2`) keeps the plain scalar compare, so the
///   `scalar_fallback` honesty of the small-n path is preserved rather than silently upgraded.
fn gate_promotion(latest: Option<GateEvidence>, baseline: Option<f64>, force: bool) -> Option<String> {
    if force {
        return None;
    }
    let baseline = baseline?;
    let Some(ev) = latest else {
        return Some(
            "promotion blocked: linked benchmark has no scored run yet (run it before promoting, or pass force=true)"
                .to_string(),
        );
    };
    let Some(mean) = ev.mean else {
        return Some(
            "promotion blocked: linked benchmark has no scored run yet (run it before promoting, or pass force=true)"
                .to_string(),
        );
    };
    if ev.runner_regressed {
        return Some(format!(
            "promotion blocked: the benchmark run that scored this version reported status \
             'regressed' (mean {mean:.3} vs baseline {baseline:.3}) (pass force=true to override)"
        ));
    }
    match ev.ci_upper {
        Some(upper) if upper + EPS < baseline => Some(format!(
            "promotion blocked: benchmark mean {mean:.3} (95% CI upper {upper:.3}) is significantly \
             below baseline {baseline:.3} (pass force=true to override)"
        )),
        Some(_) => None,
        // No interval recorded: fall back to the bare mean compare, as before.
        None if mean + EPS < baseline => Some(format!(
            "promotion blocked: benchmark mean {mean:.3} regressed below baseline {baseline:.3} \
             (no confidence interval recorded — scalar compare) (pass force=true to override)"
        )),
        None => None,
    }
}

/// The gate evidence from a run: its mean, the runner's own confidence bound, and whether the runner
/// called it a regression. Reading the runner's numbers instead of re-deriving them is what keeps
/// one definition of "regressed" in the product.
fn evidence_of(run: &BenchmarkRun) -> GateEvidence {
    GateEvidence {
        mean: run.mean_score,
        ci_upper: run
            .report
            .get("ci95")
            .and_then(Value::as_array)
            .and_then(|a| a.get(1))
            .and_then(Value::as_f64),
        runner_regressed: run.status == "regressed",
    }
}

/// The gate evidence from the most recent run that **provably scored `version` of `prompt_id`** —
/// its report carries the `{prompt_id, prompt_version}` the version-triggered enqueue stamped
/// through the runner. Runs are matched newest-`finished_at`-first. For benches whose runs predate
/// the tagging (no tagged run at all), falls back to the newest scored run of any version, so legacy
/// projects keep a working gate rather than an always-blocking one; once tagged runs exist for the
/// version, only they count — a tagged-but-unscored set correctly reads as "no scored run yet".
fn version_scored_run(runs: &[BenchmarkRun], prompt_id: &str, version: u32) -> Option<GateEvidence> {
    let mut tagged: Vec<&BenchmarkRun> = runs
        .iter()
        .filter(|r| {
            r.report.get("prompt_id").and_then(Value::as_str) == Some(prompt_id)
                && r.report.get("prompt_version").and_then(Value::as_u64) == Some(version as u64)
        })
        .collect();
    if tagged.is_empty() {
        return runs.iter().find(|r| r.mean_score.is_some()).map(evidence_of);
    }
    tagged.sort_by_key(|r| r.finished_at);
    tagged.iter().rev().find(|r| r.mean_score.is_some()).map(|r| evidence_of(r))
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

    fn run_with(report: Value, mean: Option<f64>, finished_offset_secs: i64) -> BenchmarkRun {
        BenchmarkRun {
            id: new_id(),
            benchmark_id: "b".into(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now() + chrono::Duration::seconds(finished_offset_secs)),
            n_cases: 1,
            mean_score: mean,
            pass_rate: mean,
            cost_usd: 0.0,
            status: "passed".into(),
            p50_latency_ms: None,
            p95_latency_ms: None,
            total_tokens: None,
            report,
        }
    }

    /// Legacy-shaped evidence: a bare mean with no recorded interval (the scalar-compare path).
    fn scalar(mean: f64) -> Option<GateEvidence> {
        Some(GateEvidence { mean: Some(mean), ci_upper: None, runner_regressed: false })
    }

    #[test]
    fn gate_reads_the_run_for_the_promoted_version_not_the_newest() {
        let tag = |v: u32| serde_json::json!({ "prompt_id": "p1", "prompt_version": v });
        let mean_of = |e: Option<GateEvidence>| e.and_then(|e| e.mean);
        let runs = vec![
            // Newest run overall scored v3 GREEN — must NOT clear a v9 promotion.
            run_with(tag(3), Some(0.95), 100),
            // The run that actually scored v9 is older and RED.
            run_with(tag(9), Some(0.40), 50),
        ];
        assert_eq!(mean_of(version_scored_run(&runs, "p1", 9)), Some(0.40), "v9's own run counts");
        assert_eq!(mean_of(version_scored_run(&runs, "p1", 3)), Some(0.95));
        // Two runs for the same version: the newest finished_at wins.
        let runs2 = vec![run_with(tag(9), Some(0.40), 10), run_with(tag(9), Some(0.90), 20)];
        assert_eq!(mean_of(version_scored_run(&runs2, "p1", 9)), Some(0.90));
        // Tagged runs exist but none scored → None (the gate blocks as "no scored run yet").
        let runs3 = vec![run_with(tag(9), None, 10)];
        assert!(version_scored_run(&runs3, "p1", 9).is_none());
        // Legacy: no tagged runs at all → newest scored run of any version (old behavior preserved).
        let legacy = vec![run_with(Value::Null, Some(0.7), 10)];
        assert_eq!(mean_of(version_scored_run(&legacy, "p1", 9)), Some(0.7));
        // A different prompt's tag never matches.
        let other = vec![run_with(serde_json::json!({"prompt_id":"px","prompt_version":9}), Some(0.9), 10)];
        assert_eq!(
            mean_of(version_scored_run(&other, "p1", 9)), Some(0.9), "falls back to legacy path"
        );
    }

    #[test]
    fn gate_allows_when_no_baseline_or_forced() {
        assert!(gate_promotion(scalar(0.1), None, false).is_none(), "no baseline → allow");
        assert!(gate_promotion(None, Some(0.9), true).is_none(), "force overrides a block");
        assert!(gate_promotion(scalar(0.1), Some(0.9), true).is_none(), "force overrides a regression");
    }

    #[test]
    fn gate_blocks_regression_and_unscored() {
        assert!(gate_promotion(None, Some(0.8), false).is_some(), "baseline but no run → block");
        assert!(gate_promotion(scalar(0.79), Some(0.8), false).is_some(), "below baseline → block");
        assert!(gate_promotion(scalar(0.8), Some(0.8), false).is_none(), "meeting baseline → allow");
        assert!(gate_promotion(scalar(0.95), Some(0.8), false).is_none(), "above baseline → allow");
        // A run whose mean is missing entirely reads as "no scored run yet", not as a pass.
        let no_mean = Some(GateEvidence { mean: None, ci_upper: None, runner_regressed: false });
        assert!(gate_promotion(no_mean, Some(0.8), false).is_some());
    }

    #[test]
    fn gate_is_significance_aware_when_the_run_recorded_an_interval() {
        // The false positive the old scalar gate produced: mean 0.79 vs baseline 0.80 on a noisy
        // run whose 95% interval reaches 0.88. That 0.01 dip is inside the run's own uncertainty,
        // so it is not evidence of a regression and must not block a deploy.
        let noisy = Some(GateEvidence {
            mean: Some(0.79), ci_upper: Some(0.88), runner_regressed: false,
        });
        assert!(gate_promotion(noisy, Some(0.80), false).is_none(), "a dip inside the noise");
        // A REAL regression — the whole interval below baseline — still blocks. The gate is not
        // disarmed, only made to require evidence.
        let real = Some(GateEvidence {
            mean: Some(0.50), ci_upper: Some(0.56), runner_regressed: false,
        });
        let reason = gate_promotion(real, Some(0.80), false).expect("must block");
        assert!(reason.contains("significantly below"), "got: {reason}");
        assert!(reason.contains("0.560"), "the interval is quoted so the operator can check it");
    }

    #[test]
    fn gate_honours_the_runners_own_regressed_verdict() {
        // The runner's verdict is the significance-aware one (paired per-case, family-wise
        // corrected). If it says regressed, the gate blocks even where the raw mean looks fine —
        // one definition of "regressed", not two.
        let ev = Some(GateEvidence {
            mean: Some(0.85), ci_upper: Some(0.92), runner_regressed: true,
        });
        let reason = gate_promotion(ev, Some(0.80), false).expect("must block");
        assert!(reason.contains("'regressed'"), "got: {reason}");
        // …and force still overrides it.
        assert!(gate_promotion(ev, Some(0.80), true).is_none());
    }
}
