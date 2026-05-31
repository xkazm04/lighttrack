//! `benchmarks` + `benchmark_runs` collections.

use serde_json::{json, Value};

use lighttrack_core::{Benchmark, BenchmarkRun};
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_benchmark(rest: &Rest, b: &Benchmark) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(b.id));
    m.insert("project_id".into(), json!(b.project_id));
    m.insert("name".into(), json!(b.name));
    m.insert("rubric".into(), json!(b.rubric));
    m.insert("judge_model".into(), json!(b.judge_model));
    m.insert("target".into(), json!(json_or_null_str(&b.target)?));
    m.insert("dataset_ref".into(), json!(b.dataset_ref));
    m.insert("dataset".into(), json!(serde_json::to_string(&b.dataset)?));
    m.insert("rubric_id".into(), json!(b.rubric_id));
    m.insert("baseline_score".into(), json!(b.baseline_score));
    m.insert("created_at".into(), json!(fmt_ts(b.created_at)));
    rest.put_doc("benchmarks", &b.id, &m)
}

pub(crate) fn get_benchmark(rest: &Rest, id: &str) -> Result<Option<Benchmark>> {
    rest.get_doc("benchmarks", id)?.as_ref().map(bench_from).transpose()
}

pub(crate) fn list_benchmarks(rest: &Rest, project: &str) -> Result<Vec<Benchmark>> {
    let filters: Vec<(&str, &str, Value)> = vec![("project_id", "EQUAL", json!(project))];
    let docs = rest.query("benchmarks", &filters, Some(("created_at", true)), None)?;
    docs.iter().map(bench_from).collect()
}

pub(crate) fn create_benchmark_run(rest: &Rest, r: &BenchmarkRun) -> Result<()> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(r.id));
    m.insert("benchmark_id".into(), json!(r.benchmark_id));
    m.insert("started_at".into(), json!(fmt_ts(r.started_at)));
    m.insert("finished_at".into(), json!(r.finished_at.map(fmt_ts)));
    m.insert("n_cases".into(), json!(r.n_cases as i64));
    m.insert("mean_score".into(), json!(r.mean_score));
    m.insert("pass_rate".into(), json!(r.pass_rate));
    m.insert("cost_usd".into(), json!(r.cost_usd));
    m.insert("status".into(), json!(r.status));
    m.insert("p50_latency_ms".into(), json!(r.p50_latency_ms.map(|v| v as i64)));
    m.insert("p95_latency_ms".into(), json!(r.p95_latency_ms.map(|v| v as i64)));
    m.insert("total_tokens".into(), json!(r.total_tokens.map(|v| v as i64)));
    m.insert("report".into(), json!(json_or_null_str(&r.report)?));
    rest.put_doc("benchmark_runs", &r.id, &m)
}

pub(crate) fn list_benchmark_runs(rest: &Rest, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
    let filters: Vec<(&str, &str, Value)> = vec![("benchmark_id", "EQUAL", json!(benchmark_id))];
    let docs = rest.query("benchmark_runs", &filters, Some(("started_at", true)), None)?;
    docs.iter().map(run_from).collect()
}

fn bench_from(m: &Fields) -> Result<Benchmark> {
    Ok(Benchmark {
        id: freq(m, "id")?,
        project_id: freq(m, "project_id")?,
        name: freq(m, "name")?,
        rubric: freq(m, "rubric")?,
        judge_model: freq(m, "judge_model")?,
        target: fjson(m, "target")?,
        dataset_ref: fstr(m, "dataset_ref"),
        dataset: match fstr(m, "dataset") {
            Some(s) => serde_json::from_str(&s)?,
            None => Vec::new(),
        },
        rubric_id: fstr(m, "rubric_id"),
        baseline_score: ff64(m, "baseline_score"),
        created_at: parse_ts(&freq(m, "created_at")?)?,
    })
}

fn run_from(m: &Fields) -> Result<BenchmarkRun> {
    Ok(BenchmarkRun {
        id: freq(m, "id")?,
        benchmark_id: freq(m, "benchmark_id")?,
        started_at: parse_ts(&freq(m, "started_at")?)?,
        finished_at: match fstr(m, "finished_at") {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        n_cases: fi64(m, "n_cases").unwrap_or(0) as u32,
        mean_score: ff64(m, "mean_score"),
        pass_rate: ff64(m, "pass_rate"),
        cost_usd: ff64(m, "cost_usd").unwrap_or(0.0),
        status: freq(m, "status")?,
        p50_latency_ms: fi64(m, "p50_latency_ms").map(|v| v as u64),
        p95_latency_ms: fi64(m, "p95_latency_ms").map(|v| v as u64),
        total_tokens: fi64(m, "total_tokens").map(|v| v as u64),
        report: fjson(m, "report")?,
    })
}
