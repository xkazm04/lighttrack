//! Benchmarks (Phase 3.5) — definitions, runs, and the comparison target matrix.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{
    new_id, BenchTarget, Benchmark, BenchmarkCase, BenchmarkRun,
};

use crate::alerts::BenchRunAlert;
use crate::auth::Principal;
use crate::error::ApiError;
use crate::guards::{authenticate, ensure_can_admin, resolve_read_project};
use crate::state::{spawn_db, AppState};

#[derive(Deserialize)]
pub(crate) struct CreateBenchmarkReq {
    name: String,
    /// Freeform rubric text (single-score mode); optional when `rubric_id` is set.
    #[serde(default)]
    rubric: String,
    #[serde(default = "default_judge_model")]
    judge_model: String,
    #[serde(default)]
    target: serde_json::Value,
    /// Comparison matrix: generate candidate outputs from each of these targets (Phase 3.6e).
    #[serde(default)]
    targets: Vec<BenchTarget>,
    #[serde(default)]
    dataset: Vec<BenchmarkCase>,
    /// Reference a stored dataset by id instead of (or in addition to) an inline dataset.
    #[serde(default)]
    dataset_ref: Option<String>,
    /// Optional structured rubric (id) for per-dimension judging.
    #[serde(default)]
    rubric_id: Option<String>,
    #[serde(default)]
    baseline_score: Option<f64>,
}

fn default_judge_model() -> String {
    "haiku".to_string()
}

/// Validate the stored `target` field before it reaches the store. An **array** is unambiguously a
/// comparison matrix and must deserialize as `Vec<BenchTarget>`; a malformed one is rejected here
/// (400) rather than silently degrading to a different benchmark mode at run time. Non-array targets
/// (null / object / string) are legacy free-form and pass through untouched.
fn validate_target_matrix(target: &serde_json::Value) -> Result<(), String> {
    if target.is_array() {
        serde_json::from_value::<Vec<BenchTarget>>(target.clone()).map(|_| ()).map_err(|e| {
            format!(
                "`target` is an array but not a valid comparison matrix \
                 (expected [{{provider, model, system_prompt?, label?}}, ...]): {e}"
            )
        })
    } else {
        Ok(())
    }
}

pub(crate) async fn create_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
    Json(req): Json<CreateBenchmarkReq>,
) -> Result<Json<Benchmark>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    // The target matrix (if any) is stored in the `target` field as a JSON array. A typed `targets`
    // is already valid; a raw `target` array must be validated before we persist it.
    let target = if req.targets.is_empty() {
        validate_target_matrix(&req.target).map_err(ApiError::bad_request)?;
        req.target
    } else {
        serde_json::to_value(&req.targets).unwrap_or(serde_json::Value::Null)
    };
    let b = Benchmark {
        id: new_id(),
        project_id: pid,
        name: req.name,
        rubric: req.rubric,
        judge_model: req.judge_model,
        target,
        dataset_ref: req.dataset_ref,
        dataset: req.dataset,
        rubric_id: req.rubric_id,
        baseline_score: req.baseline_score,
        created_at: Utc::now(),
    };
    let store = st.store.clone();
    let b2 = b.clone();
    spawn_db(move || store.create_benchmark(&b2)).await?;
    Ok(Json(b))
}

pub(crate) async fn list_benchmarks(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(pid): Path<String>,
) -> Result<Json<Vec<Benchmark>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    resolve_read_project(&p, Some(&pid))?;
    let store = st.store.clone();
    let v = spawn_db(move || store.list_benchmarks(&pid)).await?;
    Ok(Json(v))
}

/// Fetch a benchmark and authorize project-key access to it.
pub(crate) async fn load_benchmark_authorized(
    st: &AppState,
    p: &Principal,
    id: &str,
) -> Result<Benchmark, ApiError> {
    let store = st.store.clone();
    let id2 = id.to_string();
    let bench = spawn_db(move || store.get_benchmark(&id2))
        .await?
        .ok_or_else(|| ApiError::not_found(format!("benchmark '{id}' not found")))?;
    if let Principal::Project(pid) = p {
        if &bench.project_id != pid {
            return Err(ApiError::forbidden("key not authorized for that benchmark"));
        }
    }
    Ok(bench)
}

pub(crate) async fn get_benchmark(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Benchmark>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    Ok(Json(load_benchmark_authorized(&st, &p, &id).await?))
}

pub(crate) async fn list_benchmark_runs(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Vec<BenchmarkRun>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    load_benchmark_authorized(&st, &p, &id).await?; // authorize
    let store = st.store.clone();
    let runs = spawn_db(move || store.list_benchmark_runs(&id)).await?;
    Ok(Json(runs))
}

pub(crate) async fn post_benchmark_run(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(run): Json<BenchmarkRun>,
) -> Result<Json<BenchmarkRun>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let bench = load_benchmark_authorized(&st, &p, &run.benchmark_id).await?; // authorize via the benchmark
    let store = st.store.clone();
    let run2 = run.clone();
    spawn_db(move || store.create_benchmark_run(&run2)).await?;
    // Best-effort completion webhook (off the request path, cooldown-deduped) so a CI gate / dashboard
    // learns a run finished with its honest status.
    st.alerts.notify_bench_run(BenchRunAlert {
        benchmark: run.benchmark_id.clone(),
        run_id: run.id.clone(),
        status: run.status.clone(),
        mean: run.mean_score,
        baseline: bench.baseline_score,
    });
    Ok(Json(run))
}

/// Machine-readable CI-gate verdict for a benchmark, from its latest finished run. `status` is
/// `pass | regressed | no_baseline | no_runs`. Consumers (a pipeline step, a dashboard badge) branch
/// on `status`; `run_id`/`mean`/`baseline`/`n` give the supporting numbers.
#[derive(Debug, Serialize, PartialEq)]
pub(crate) struct GateResponse {
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    run_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    mean: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    baseline: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    n: Option<u64>,
}

/// Decide the gate verdict from a benchmark's runs (newest-first, as the store returns them) and its
/// baseline. Uses the latest *finished* run's honest status (Direction 1/2); legacy runs that predate
/// the honest-status work fall back to a scalar mean-vs-baseline compare. `n` prefers the report's
/// significance `n`, else `n_cases`.
pub(crate) fn decide_gate(runs: &[BenchmarkRun], baseline: Option<f64>) -> GateResponse {
    let Some(run) = runs.iter().find(|r| r.finished_at.is_some()) else {
        return GateResponse { status: "no_runs".into(), run_id: None, mean: None, baseline, n: None };
    };
    let status = match run.status.as_str() {
        "passed" => "pass",
        "regressed" => "regressed",
        "no_baseline" => "no_baseline",
        // Legacy status (e.g. "completed"/"compared") → scalar compare of mean vs baseline.
        _ => match (run.mean_score, baseline) {
            (Some(m), Some(b)) if m + 1e-9 < b => "regressed",
            (Some(_), Some(_)) => "pass",
            _ => "no_baseline",
        },
    };
    let n = run
        .report
        .get("n")
        .and_then(serde_json::Value::as_u64)
        .or(Some(run.n_cases as u64));
    GateResponse {
        status: status.into(),
        run_id: Some(run.id.clone()),
        mean: run.mean_score,
        baseline,
        n,
    }
}

pub(crate) async fn benchmark_gate(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<GateResponse>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let bench = load_benchmark_authorized(&st, &p, &id).await?;
    let store = st.store.clone();
    let runs = spawn_db(move || store.list_benchmark_runs(&id)).await?;
    Ok(Json(decide_gate(&runs, bench.baseline_score)))
}

#[cfg(test)]
mod tests {
    use super::{decide_gate, validate_target_matrix};
    use lighttrack_core::BenchmarkRun;
    use serde_json::json;

    /// Build a run via serde so the test doesn't hand-construct every field.
    fn run(status: &str, finished: bool, mean: Option<f64>, report: serde_json::Value) -> BenchmarkRun {
        let mut v = json!({
            "id": format!("run-{status}"), "benchmark_id": "b", "started_at": "2026-01-01T00:00:00.000000000Z",
            "n_cases": 5, "mean_score": mean, "status": status, "report": report,
        });
        if finished {
            v["finished_at"] = json!("2026-01-01T00:01:00.000000000Z");
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn gate_no_runs_when_none_finished() {
        let g = decide_gate(&[], Some(0.8));
        assert_eq!(g.status, "no_runs");
        // A run that never finished is ignored.
        let g = decide_gate(&[run("passed", false, Some(0.9), json!(null))], Some(0.8));
        assert_eq!(g.status, "no_runs");
    }

    #[test]
    fn gate_maps_honest_statuses() {
        let g = decide_gate(&[run("passed", true, Some(0.9), json!({ "n": 30 }))], Some(0.8));
        assert_eq!(g.status, "pass");
        assert_eq!(g.n, Some(30)); // report n wins over n_cases
        assert_eq!(g.run_id.as_deref(), Some("run-passed"));

        assert_eq!(decide_gate(&[run("regressed", true, Some(0.5), json!(null))], Some(0.8)).status, "regressed");
        assert_eq!(decide_gate(&[run("no_baseline", true, Some(0.5), json!(null))], None).status, "no_baseline");
    }

    #[test]
    fn gate_legacy_status_falls_back_to_scalar() {
        // "completed" predates honest statuses → scalar mean-vs-baseline compare.
        assert_eq!(decide_gate(&[run("completed", true, Some(0.5), json!(null))], Some(0.8)).status, "regressed");
        assert_eq!(decide_gate(&[run("completed", true, Some(0.9), json!(null))], Some(0.8)).status, "pass");
        // No baseline → no_baseline; n falls back to n_cases when the report has none.
        let g = decide_gate(&[run("completed", true, Some(0.9), json!(null))], None);
        assert_eq!(g.status, "no_baseline");
        assert_eq!(g.n, Some(5));
    }

    #[test]
    fn gate_uses_latest_finished_run() {
        // Store returns newest-first; the first finished run wins.
        let runs = [
            run("regressed", true, Some(0.5), json!(null)),
            run("passed", true, Some(0.9), json!(null)),
        ];
        assert_eq!(decide_gate(&runs, Some(0.8)).status, "regressed");
    }

    #[test]
    fn non_array_targets_pass_through() {
        assert!(validate_target_matrix(&json!(null)).is_ok());
        assert!(validate_target_matrix(&json!({ "endpoint": "https://x" })).is_ok());
        assert!(validate_target_matrix(&json!("legacy")).is_ok());
    }

    #[test]
    fn valid_matrix_ok_malformed_rejected() {
        assert!(validate_target_matrix(&json!([{ "provider": "openai", "model": "gpt-4o" }])).is_ok());
        // Missing required `provider` → rejected (would otherwise silently degrade to simple mode).
        assert!(validate_target_matrix(&json!([{ "model": "x" }])).is_err());
        assert!(validate_target_matrix(&json!(["nope"])).is_err());
    }
}
