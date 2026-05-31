//! Benchmarks and benchmark runs.

use sqlx::postgres::{PgPool, PgRow};
use sqlx::Row;

use lighttrack_core::{Benchmark, BenchmarkRun};
use lighttrack_store::Result;

use crate::util::{fmt_ts, json_or_null, parse_ts, pgerr, val_or_null};

const BENCH_COLS: &str = "id, project_id, name, rubric, judge_model, target, dataset_ref, \
    dataset, rubric_id, baseline_score, created_at";

const RUN_COLS: &str = "id, benchmark_id, started_at, finished_at, n_cases, mean_score, \
    pass_rate, cost_usd, status, p50_latency_ms, p95_latency_ms, total_tokens, report";

pub(crate) async fn create(pool: &PgPool, b: &Benchmark) -> Result<()> {
    let target = json_or_null(&b.target)?;
    let dataset = serde_json::to_string(&b.dataset)?;
    sqlx::query(
        "INSERT INTO benchmarks (id, project_id, name, rubric, judge_model, target, \
         dataset_ref, dataset, rubric_id, baseline_score, created_at) \
         VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11)",
    )
    .bind(b.id.clone())
    .bind(b.project_id.clone())
    .bind(b.name.clone())
    .bind(b.rubric.clone())
    .bind(b.judge_model.clone())
    .bind(target)
    .bind(b.dataset_ref.clone())
    .bind(dataset)
    .bind(b.rubric_id.clone())
    .bind(b.baseline_score)
    .bind(fmt_ts(b.created_at))
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn get(pool: &PgPool, id: &str) -> Result<Option<Benchmark>> {
    let row = sqlx::query(&format!("SELECT {BENCH_COLS} FROM benchmarks WHERE id = $1"))
        .bind(id.to_string())
        .fetch_optional(pool)
        .await
        .map_err(pgerr)?;
    row.as_ref().map(bench_from_row).transpose()
}

pub(crate) async fn list(pool: &PgPool, project: &str) -> Result<Vec<Benchmark>> {
    let rows = sqlx::query(&format!(
        "SELECT {BENCH_COLS} FROM benchmarks WHERE project_id = $1 ORDER BY created_at DESC"
    ))
    .bind(project.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(bench_from_row).collect()
}

pub(crate) async fn create_run(pool: &PgPool, r: &BenchmarkRun) -> Result<()> {
    let report = json_or_null(&r.report)?;
    sqlx::query(
        "INSERT INTO benchmark_runs (id, benchmark_id, started_at, finished_at, n_cases, \
         mean_score, pass_rate, cost_usd, status, p50_latency_ms, p95_latency_ms, \
         total_tokens, report) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11,$12,$13)",
    )
    .bind(r.id.clone())
    .bind(r.benchmark_id.clone())
    .bind(fmt_ts(r.started_at))
    .bind(r.finished_at.map(fmt_ts))
    .bind(r.n_cases as i64)
    .bind(r.mean_score)
    .bind(r.pass_rate)
    .bind(r.cost_usd)
    .bind(r.status.clone())
    .bind(r.p50_latency_ms.map(|v| v as i64))
    .bind(r.p95_latency_ms.map(|v| v as i64))
    .bind(r.total_tokens.map(|v| v as i64))
    .bind(report)
    .execute(pool)
    .await
    .map_err(pgerr)?;
    Ok(())
}

pub(crate) async fn list_runs(pool: &PgPool, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
    let rows = sqlx::query(&format!(
        "SELECT {RUN_COLS} FROM benchmark_runs WHERE benchmark_id = $1 ORDER BY started_at DESC"
    ))
    .bind(benchmark_id.to_string())
    .fetch_all(pool)
    .await
    .map_err(pgerr)?;
    rows.iter().map(run_from_row).collect()
}

fn bench_from_row(row: &PgRow) -> Result<Benchmark> {
    let target: Option<String> = row.try_get(5).map_err(pgerr)?;
    let dataset: Option<String> = row.try_get(7).map_err(pgerr)?;
    let created_at: String = row.try_get(10).map_err(pgerr)?;
    Ok(Benchmark {
        id: row.try_get(0).map_err(pgerr)?,
        project_id: row.try_get(1).map_err(pgerr)?,
        name: row.try_get(2).map_err(pgerr)?,
        rubric: row.try_get(3).map_err(pgerr)?,
        judge_model: row.try_get(4).map_err(pgerr)?,
        target: val_or_null(target)?,
        dataset_ref: row.try_get(6).map_err(pgerr)?,
        dataset: match dataset {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        rubric_id: row.try_get(8).map_err(pgerr)?,
        baseline_score: row.try_get(9).map_err(pgerr)?,
        created_at: parse_ts(&created_at)?,
    })
}

fn run_from_row(row: &PgRow) -> Result<BenchmarkRun> {
    let started_at: String = row.try_get(2).map_err(pgerr)?;
    let finished_at: Option<String> = row.try_get(3).map_err(pgerr)?;
    let report: Option<String> = row.try_get(12).map_err(pgerr)?;
    Ok(BenchmarkRun {
        id: row.try_get(0).map_err(pgerr)?,
        benchmark_id: row.try_get(1).map_err(pgerr)?,
        started_at: parse_ts(&started_at)?,
        finished_at: match finished_at {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        n_cases: row.try_get::<i64, _>(4).map_err(pgerr)? as u32,
        mean_score: row.try_get(5).map_err(pgerr)?,
        pass_rate: row.try_get(6).map_err(pgerr)?,
        cost_usd: row.try_get(7).map_err(pgerr)?,
        status: row.try_get(8).map_err(pgerr)?,
        p50_latency_ms: row.try_get::<Option<i64>, _>(9).map_err(pgerr)?.map(|v| v as u64),
        p95_latency_ms: row.try_get::<Option<i64>, _>(10).map_err(pgerr)?.map(|v| v as u64),
        total_tokens: row.try_get::<Option<i64>, _>(11).map_err(pgerr)?.map(|v| v as u64),
        report: val_or_null(report)?,
    })
}
