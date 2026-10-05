//! `GET /v1/benchmarks/:id/regression` — the failure signature beside the mean.
//!
//! `GET .../gate` hands an operator one bit (`{"status":"regressed","mean":0.71,"baseline":0.78}`)
//! and every byte needed to answer *which dimension, which cases, how did they fail* is already on
//! the rows the same run wrote: per-dimension score, weight, floor, `floor_hit`, `voided`, the
//! judge's own reasoning strings, `parse_failures`, `injection_suspected`. Nothing aggregated any
//! of it per run. This route is that aggregate.
//!
//! Two things it deliberately does not do. It does **not** copy the comparability rule — it calls
//! [`crate::benchmarks::instrument_moved`], because a diff across a judge or rubric swap is a
//! methodology change rather than a regression, and the gate already states that in words worth
//! reusing. And it adds **no `Store` method**: the arithmetic is a pure fold over two
//! `Vec<Score>` in [`diff`], so SQLite and Postgres cannot diverge on it.

pub(crate) mod diff;

use axum::{
    extract::{Path, Query, State},
    http::HeaderMap,
    Json,
};
use serde::Deserialize;

use lighttrack_core::BenchmarkRun;

use crate::benchmarks::{instrument_moved, load_benchmark_authorized};
use crate::error::ApiError;
use crate::guards::authenticate;
use crate::state::{spawn_db, AppState};

/// A run's cases are one dataset pass, so the read allows the same page the run-scoped score
/// listing does rather than truncating a 500-case comparison into a wrong mean.
const CASE_LIMIT: usize = 10_000;

#[derive(Debug, Deserialize)]
pub(crate) struct RegressionParams {
    /// The run under scrutiny.
    run: String,
    /// What to compare it against. Defaults to the previous finished run of the same benchmark.
    #[serde(default)]
    against: Option<String>,
}

/// The run `id` names, or a 404 that does not confirm whether some other benchmark owns it.
fn pick<'a>(runs: &'a [BenchmarkRun], id: &str) -> Result<&'a BenchmarkRun, ApiError> {
    runs.iter()
        .find(|r| r.id == id)
        .ok_or_else(|| ApiError::not_found(format!("run '{id}' is not a run of this benchmark")))
}

pub(crate) async fn benchmark_regression(
    State(st): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    Query(q): Query<RegressionParams>,
) -> Result<Json<diff::RegressionDiff>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let bench = load_benchmark_authorized(&st, &p, &id).await?;

    let store = st.store.clone();
    let sc = p.scope_owned();
    let bid = id.clone();
    let runs = spawn_db(move || store.list_benchmark_runs(sc.as_deref().into(), &bid)).await?;

    let after = pick(&runs, &q.run)?;
    let before = match &q.against {
        Some(a) => pick(&runs, a)?,
        // Newest-first, as the store returns them: the previous finished run is the first finished
        // one *after* this one in that order.
        None => runs
            .iter()
            .skip_while(|r| r.id != after.id)
            .skip(1)
            .find(|r| r.finished_at.is_some())
            .ok_or_else(|| {
                ApiError::not_found(format!(
                    "run '{}' has no earlier finished run to compare against; pass `against=<run \
                     id>` once a second run exists",
                    q.run
                ))
            })?,
    };

    // The rule the gate already wrote, called rather than re-derived. Either run having been scored
    // by a different instrument than the benchmark now uses means at least one of the two numbers
    // came out of a different instrument, and subtracting them would publish a methodology change
    // as a quality finding.
    for r in [before, after] {
        if let Some(why) = instrument_moved(r, &bench) {
            return Err(ApiError::conflict(why));
        }
    }

    let (b_id, a_id) = (before.id.clone(), after.id.clone());
    let store = st.store.clone();
    let sc = p.scope_owned();
    let before_scores =
        spawn_db(move || store.list_run_scores(&b_id, sc.as_deref().into(), CASE_LIMIT)).await?;
    let store = st.store.clone();
    let sc = p.scope_owned();
    let after_scores =
        spawn_db(move || store.list_run_scores(&a_id, sc.as_deref().into(), CASE_LIMIT)).await?;

    Ok(Json(diff::diff(&before_scores, &after_scores)))
}

#[cfg(test)]
mod tests;
