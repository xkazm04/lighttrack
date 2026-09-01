//! Benchmarks (Phase 3.5) — definitions, runs, and the comparison target matrix.

use axum::{
    extract::{Path, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use lighttrack_core::{new_id, BenchTarget, Benchmark, BenchmarkCase, BenchmarkRun};

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
    /// Opt-in recurrence: when set (> 0), `lt-runner serve` re-runs this benchmark roughly every
    /// this many seconds, turning it into continuous quality monitoring. Unset = one-shot (runs only
    /// on manual enqueue or a prompt-version cut). Carried inside `target` — see [`embed_recurrence`].
    #[serde(default)]
    schedule_interval_secs: Option<u64>,
}

/// Judging is the one call in this product whose quality *is* the product, and it is deliberately
/// unbudgeted (D4). Measured on a 12-item golden set with a 3-dimension rubric, a small judge was the
/// worst on every axis that matters: it compressed good and bad answers toward the middle (0.80 vs
/// 0.35, against opus@xhigh's 0.95 vs 0.32), correlated worst with the human labels (0.745 vs 0.844),
/// and scored a genuinely good answer below the pass threshold. Default to the strong judge and let
/// an operator trade down explicitly.
///
/// Note the standing caveat: prefer a judge family *different* from the generator you are grading, or
/// self-preference bias creeps into the verdict.
fn default_judge_model() -> String {
    "opus@xhigh".to_string()
}

use lighttrack_core::RECURRENCE_KEY;

/// Fold an opt-in recurrence interval into the stored `target`. Recurrence needs an object (or empty)
/// target; a comparison-matrix target is a JSON array with no room for a sibling key, so that
/// combination is a hard 400 rather than a silent drop (a matrix benchmark simply can't recur in v1).
fn embed_recurrence(target: serde_json::Value, secs: u64) -> Result<serde_json::Value, String> {
    match target {
        serde_json::Value::Null => Ok(serde_json::json!({ RECURRENCE_KEY: secs })),
        serde_json::Value::Object(mut m) => {
            m.insert(RECURRENCE_KEY.to_string(), serde_json::json!(secs));
            Ok(serde_json::Value::Object(m))
        }
        serde_json::Value::Array(_) => Err(
            "schedule_interval_secs is not supported for a comparison-matrix benchmark (an array \
             `target`/`targets`); use a single-target, rubric, or simple benchmark for recurrence"
                .into(),
        ),
        _ => Err("schedule_interval_secs requires an object or empty `target`".into()),
    }
}

/// Validate the stored `target` field before it reaches the store. An **array** is unambiguously a
/// comparison matrix and must deserialize as `Vec<BenchTarget>`; a malformed one is rejected here
/// (400) rather than silently degrading to a different benchmark mode at run time. Non-array targets
/// (null / object / string) are legacy free-form and pass through untouched.
fn validate_target_matrix(target: &serde_json::Value) -> Result<(), String> {
    if target.is_array() {
        serde_json::from_value::<Vec<BenchTarget>>(target.clone())
            .map(|_| ())
            .map_err(|e| {
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
    // Opt-in recurrence rides inside `target` (no schema/column change); reject the one combination
    // it can't carry (a comparison matrix) up front.
    let target = match req.schedule_interval_secs.filter(|s| *s > 0) {
        Some(secs) => embed_recurrence(target, secs).map_err(ApiError::bad_request)?,
        None => target,
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
    if let Principal::Project {
        project_id: pid, ..
    } = p
    {
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
/// on `status`; `run_id`/`mean`/`baseline`/`n` give the supporting numbers, and `caveat` names the
/// condition that made a floor verdict inapplicable when one did.
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
    /// Why the stored baseline could not be compared against. Present only when the predicate
    /// below refused the floor, so an operator reads *which* condition expired rather than
    /// hunting for a baseline that is sitting right there.
    #[serde(skip_serializing_if = "Option::is_none")]
    caveat: Option<String>,
}

/// Why a stored baseline cannot be compared against the run that was scored, or `None` when the
/// comparison holds.
///
/// A baseline is not a constant — it is a measurement, and it carries the conditions it was taken
/// under. `stamp_pins` records those conditions on every *run* (`judge_model`, `dataset_ref`, and
/// the dataset's frozen state and version); the baseline is a bare scalar in the benchmark row and
/// records none of them. So the strongest statement this gate can make is the one condition it can
/// actually read off the run: **the case set was allowed to move.**
///
/// An unfrozen dataset means the cases could have changed between the moment the baseline was
/// established and the moment this run was scored. The two numbers are then means over different
/// populations, and subtracting them is arithmetic without a claim behind it. That is not a
/// regression and it is not a pass; it is the *unverified* lane the exit-code contract already
/// carries (`EXIT_NO_BASELINE`, deliberately distinct from `EXIT_REGRESSED` so CI can warn rather
/// than hard-fail).
fn baseline_not_comparable(run: &BenchmarkRun) -> Option<String> {
    match run.report.get("dataset_frozen") {
        Some(serde_json::Value::Bool(false)) => {
            let v = run
                .report
                .get("dataset_version")
                .and_then(serde_json::Value::as_i64);
            Some(match v {
                Some(v) => format!(
                    "baseline not comparable: this run scored an UNFROZEN dataset (version {v}),                      so its case set may differ from the one the baseline was established on;                      freeze the dataset and re-establish the baseline"
                ),
                None => "baseline not comparable: this run scored an UNFROZEN dataset, so its case                          set may differ from the one the baseline was established on; freeze the                          dataset and re-establish the baseline"
                    .into(),
            })
        }
        _ => None,
    }
}

/// Decide the gate verdict from a benchmark's runs (newest-first, as the store returns them) and the
/// benchmark itself. Uses the latest *finished* run's honest status (Direction 1/2); legacy runs that
/// predate the honest-status work fall back to a scalar mean-vs-baseline compare. `n` prefers the
/// report's significance `n`, else `n_cases`.
///
/// Takes the whole `Benchmark` rather than its `baseline_score` because the conditions a floor
/// verdict depends on live beside the number, and narrowing to `Option<f64>` at this boundary threw
/// them away while the caller was still holding them.
pub(crate) fn decide_gate(runs: &[BenchmarkRun], bench: &Benchmark) -> GateResponse {
    let baseline = bench.baseline_score;
    let Some(run) = runs.iter().find(|r| r.finished_at.is_some()) else {
        return GateResponse {
            status: "no_runs".into(),
            run_id: None,
            mean: None,
            baseline,
            n: None,
            caveat: None,
        };
    };
    let status = match run.status.as_str() {
        "passed" => "pass",
        "regressed" => "regressed",
        "no_baseline" => "no_baseline",
        // A run the operator's cost ceiling cut short (or refused to start) judged only part of its
        // dataset. Its mean is a mean over whatever the money reached, so it is UNVERIFIED — it must
        // never fall through to the scalar compare below and come back out as `pass`.
        "partial" | "aborted" | "cancelled" => "partial",
        // Legacy status (e.g. "completed"/"compared") → scalar compare of mean vs baseline.
        _ => match (run.mean_score, baseline) {
            (Some(m), Some(b)) if m + 1e-9 < b => "regressed",
            (Some(_), Some(_)) => "pass",
            _ => "no_baseline",
        },
    };
    // The predicate runs on the verdicts that actually rest on the baseline. `no_baseline`,
    // `no_runs` and `partial` never consulted it, so there is nothing to refuse and no caveat to
    // add — re-labelling them would replace one honest unverified state with another and lose why.
    let (status, caveat) = match status {
        "pass" | "regressed" => match baseline.and_then(|_| baseline_not_comparable(run)) {
            Some(why) => ("no_baseline", Some(why)),
            None => (status, None),
        },
        _ => (status, None),
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
        caveat,
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
    Ok(Json(decide_gate(&runs, &bench)))
}

#[cfg(test)]
mod tests {
    use super::{decide_gate, embed_recurrence, validate_target_matrix};
    use lighttrack_core::{Benchmark, BenchmarkRun};
    use serde_json::json;

    /// A benchmark carrying just the baseline, built via serde like `run` below.
    fn bench(baseline: Option<f64>) -> Benchmark {
        serde_json::from_value(json!({
            "name": "b", "rubric": "r", "judge_model": "haiku", "baseline_score": baseline,
        }))
        .unwrap()
    }

    /// Build a run via serde so the test doesn't hand-construct every field.
    fn run(
        status: &str,
        finished: bool,
        mean: Option<f64>,
        report: serde_json::Value,
    ) -> BenchmarkRun {
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
        let g = decide_gate(&[], &bench(Some(0.8)));
        assert_eq!(g.status, "no_runs");
        // A run that never finished is ignored.
        let g = decide_gate(&[run("passed", false, Some(0.9), json!(null))], &bench(Some(0.8)));
        assert_eq!(g.status, "no_runs");
    }

    #[test]
    fn gate_maps_honest_statuses() {
        let g = decide_gate(
            &[run("passed", true, Some(0.9), json!({ "n": 30 }))],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "pass");
        assert_eq!(g.n, Some(30)); // report n wins over n_cases
        assert_eq!(g.run_id.as_deref(), Some("run-passed"));

        assert_eq!(
            decide_gate(&[run("regressed", true, Some(0.5), json!(null))], &bench(Some(0.8))).status,
            "regressed"
        );
        assert_eq!(
            decide_gate(&[run("no_baseline", true, Some(0.5), json!(null))], &bench(None)).status,
            "no_baseline"
        );
    }

    #[test]
    fn gate_never_passes_a_cost_halted_run() {
        // The trap: a halted run whose partial mean happens to clear the baseline. If `partial` fell
        // through to the legacy scalar compare it would come back `pass` on 30% of the dataset.
        let g = decide_gate(&[run("partial", true, Some(0.95), json!(null))], &bench(Some(0.8)));
        assert_eq!(g.status, "partial");
        let g = decide_gate(&[run("aborted", true, Some(0.95), json!(null))], &bench(Some(0.8)));
        assert_eq!(g.status, "partial");
    }

    #[test]
    fn gate_legacy_status_falls_back_to_scalar() {
        // "completed" predates honest statuses → scalar mean-vs-baseline compare.
        assert_eq!(
            decide_gate(&[run("completed", true, Some(0.5), json!(null))], &bench(Some(0.8))).status,
            "regressed"
        );
        assert_eq!(
            decide_gate(&[run("completed", true, Some(0.9), json!(null))], &bench(Some(0.8))).status,
            "pass"
        );
        // No baseline → no_baseline; n falls back to n_cases when the report has none.
        let g = decide_gate(&[run("completed", true, Some(0.9), json!(null))], &bench(None));
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
        assert_eq!(decide_gate(&runs, &bench(Some(0.8))).status, "regressed");
    }

    #[test]
    fn gate_refuses_the_floor_when_the_dataset_was_not_frozen() {
        // `stamp_pins` records the dataset's frozen state on every run. An unfrozen dataset means
        // the case set could have moved between the baseline's establishment and this run, so the
        // two means are over different populations and neither `pass` nor `regressed` is a claim
        // anyone can defend. It degrades to the UNVERIFIED lane, not to a verdict.
        let unfrozen = json!({ "dataset_frozen": false, "dataset_version": 7 });

        // The dangerous direction: a run that would have passed. A silent `pass` here is a gate
        // that cannot fire, which is indistinguishable from a gate that is working.
        let g = decide_gate(
            &[run("passed", true, Some(0.9), unfrozen.clone())],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "no_baseline");
        let caveat = g.caveat.expect("the refused floor names its condition");
        assert!(caveat.contains("UNFROZEN"), "{caveat}");
        assert!(caveat.contains("version 7"), "the version is named: {caveat}");

        // And the other direction: a `regressed` verdict resting on the same unusable comparison is
        // not a regression either. Reporting one would be a false alarm with a number behind it.
        let g = decide_gate(
            &[run("regressed", true, Some(0.5), unfrozen.clone())],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "no_baseline");
        assert!(g.caveat.is_some());

        // A legacy status reaching the scalar compare gets the predicate too — that path is the one
        // with no significance testing at all, so it needs it most.
        let g = decide_gate(
            &[run("completed", true, Some(0.9), unfrozen)],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "no_baseline");
        assert!(g.caveat.is_some());
    }

    #[test]
    fn gate_keeps_its_verdict_when_the_dataset_was_frozen_or_absent() {
        // A frozen dataset is the comparable case: the verdict stands, and no caveat is invented.
        let frozen = json!({ "dataset_frozen": true, "dataset_version": 7 });
        let g = decide_gate(
            &[run("passed", true, Some(0.9), frozen.clone())],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "pass");
        assert_eq!(g.caveat, None);

        let g = decide_gate(&[run("regressed", true, Some(0.5), frozen)], &bench(Some(0.8)));
        assert_eq!(g.status, "regressed");
        assert_eq!(g.caveat, None);

        // An inline dataset stamps no frozen flag at all. The predicate reads absence as "nothing
        // says the cases moved" rather than as a refusal — a benchmark carrying its own cases has
        // no separate dataset to drift underneath it.
        let g = decide_gate(
            &[run("passed", true, Some(0.9), json!({ "n": 30 }))],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "pass");
        assert_eq!(g.caveat, None);
    }

    #[test]
    fn gate_does_not_relabel_states_that_never_consulted_the_baseline() {
        let unfrozen = json!({ "dataset_frozen": false, "dataset_version": 7 });

        // `partial` is already unverified for a different and more specific reason — the run judged
        // part of its dataset. Overwriting it with `no_baseline` would trade one honest state for
        // another and lose why, so the predicate leaves it alone.
        let g = decide_gate(
            &[run("partial", true, Some(0.95), unfrozen.clone())],
            &bench(Some(0.8)),
        );
        assert_eq!(g.status, "partial");
        assert_eq!(g.caveat, None);

        // With no baseline at all there is no floor to refuse: `no_baseline` from absence is a
        // different state from `no_baseline` from expiry, and only the second carries a caveat.
        let g = decide_gate(&[run("completed", true, Some(0.9), unfrozen)], &bench(None));
        assert_eq!(g.status, "no_baseline");
        assert_eq!(g.caveat, None);
    }


    #[test]
    fn non_array_targets_pass_through() {
        assert!(validate_target_matrix(&json!(null)).is_ok());
        assert!(validate_target_matrix(&json!({ "endpoint": "https://x" })).is_ok());
        assert!(validate_target_matrix(&json!("legacy")).is_ok());
    }

    #[test]
    fn valid_matrix_ok_malformed_rejected() {
        assert!(
            validate_target_matrix(&json!([{ "provider": "openai", "model": "gpt-4o" }])).is_ok()
        );
        // Missing required `provider` → rejected (would otherwise silently degrade to simple mode).
        assert!(validate_target_matrix(&json!([{ "model": "x" }])).is_err());
        assert!(validate_target_matrix(&json!(["nope"])).is_err());
    }

    #[test]
    fn embed_recurrence_into_object_and_null() {
        // Null target becomes a fresh object carrying the interval.
        assert_eq!(
            embed_recurrence(json!(null), 3600).unwrap(),
            json!({ "schedule_interval_secs": 3600 })
        );
        // An existing free-form object keeps its keys and gains the interval.
        assert_eq!(
            embed_recurrence(json!({ "endpoint": "https://x" }), 60).unwrap(),
            json!({ "endpoint": "https://x", "schedule_interval_secs": 60 })
        );
    }

    #[test]
    fn embed_recurrence_rejects_matrix_and_scalars() {
        // A comparison matrix (array) has no room for a sibling key → hard error, not a silent drop.
        assert!(
            embed_recurrence(json!([{ "provider": "openai", "model": "gpt-4o" }]), 60).is_err()
        );
        assert!(embed_recurrence(json!("legacy-string"), 60).is_err());
    }
}
