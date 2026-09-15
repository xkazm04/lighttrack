//! Benchmarks and benchmark runs.

use rusqlite::{params, Connection, OptionalExtension, Row};
use serde_json::Value;

use lighttrack_core::{Benchmark, BenchmarkRun};

use crate::codec::{fmt_ts, parse_ts};
use crate::Result;

const BENCH_COLS: &str = "id, project_id, name, rubric, judge_model, target, dataset_ref, \
    dataset, rubric_id, baseline_score, created_at";

const RUN_COLS: &str = "id, benchmark_id, started_at, finished_at, n_cases, mean_score, \
    pass_rate, cost_usd, status, p50_latency_ms, p95_latency_ms, total_tokens, report";

pub(super) fn create(conn: &Connection, b: &Benchmark) -> Result<()> {
    let target = if b.target.is_null() {
        None
    } else {
        Some(serde_json::to_string(&b.target)?)
    };
    let dataset = serde_json::to_string(&b.dataset)?;
    conn.execute(
        "INSERT INTO benchmarks \
         (id, project_id, name, rubric, judge_model, target, dataset_ref, dataset, rubric_id, baseline_score, created_at) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)",
        params![
            b.id, b.project_id, b.name, b.rubric, b.judge_model, target, b.dataset_ref, dataset,
            b.rubric_id, b.baseline_score, fmt_ts(b.created_at),
        ],
    )?;
    Ok(())
}

pub(super) fn get(conn: &Connection, project: Option<&str>, id: &str) -> Result<Option<Benchmark>> {
    let sql = format!(
        "SELECT {BENCH_COLS} FROM benchmarks WHERE id = ?1{}",
        super::scope_and(2)
    );
    let mut stmt = conn.prepare(&sql)?;
    let raw = stmt.query_row(params![id, project], map_bench).optional()?;
    raw.map(bench_from_raw).transpose()
}

pub(super) fn list(conn: &Connection, project: &str) -> Result<Vec<Benchmark>> {
    let sql = format!(
        "SELECT {BENCH_COLS} FROM benchmarks WHERE project_id = ?1 ORDER BY created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![project], map_bench)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(bench_from_raw).collect()
}

pub(super) fn create_run(conn: &Connection, r: &BenchmarkRun) -> Result<()> {
    let report = if r.report.is_null() {
        None
    } else {
        Some(serde_json::to_string(&r.report)?)
    };
    conn.execute(
        "INSERT INTO benchmark_runs \
         (id, benchmark_id, started_at, finished_at, n_cases, mean_score, pass_rate, cost_usd, status, \
          p50_latency_ms, p95_latency_ms, total_tokens, report) \
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
        params![
            r.id,
            r.benchmark_id,
            fmt_ts(r.started_at),
            r.finished_at.map(fmt_ts),
            r.n_cases as i64,
            r.mean_score,
            r.pass_rate,
            r.cost_usd,
            r.status,
            r.p50_latency_ms.map(|v| v as i64),
            r.p95_latency_ms.map(|v| v as i64),
            r.total_tokens.map(|v| v as i64),
            report,
        ],
    )?;
    Ok(())
}

/// `benchmark_runs` carries no `project_id` of its own, so the tenant filter rides the parent
/// benchmark: a foreign benchmark id yields an empty list, never someone else's runs.
///
/// `id` breaks the `started_at` tie. Two runs of one benchmark can share a start instant — a
/// scheduler firing a recurrence twice, a replay, a test fixture — and this table has no insertion
/// column to fall back on, so `ORDER BY started_at DESC` alone leaves their order to the query
/// plan. That is not academic: adding `idx_benchmark_runs_bench` flipped a scan into an index walk
/// and with it the run that `version_scored_run` reads as "latest", which changed a promotion
/// gate's verdict from refuse to allow. The tie-break does NOT claim one of two simultaneous runs
/// is later — nothing here knows that. It claims the answer will be the same on every read, which
/// is the property a gate needs.
pub(super) fn list_runs(
    conn: &Connection,
    project: Option<&str>,
    benchmark_id: &str,
) -> Result<Vec<BenchmarkRun>> {
    let sql = format!(
        "SELECT {RUN_COLS} FROM benchmark_runs WHERE benchmark_id = ?1 \
           AND (?2 IS NULL OR EXISTS \
                (SELECT 1 FROM benchmarks b WHERE b.id = benchmark_id AND b.project_id = ?2)) \
         ORDER BY started_at DESC, id DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let raws = stmt
        .query_map(params![benchmark_id, project], map_run)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    raws.into_iter().map(run_from_raw).collect()
}

type BenchRaw = (
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<f64>,
    String,
);

fn map_bench(row: &Row) -> rusqlite::Result<BenchRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
    ))
}

fn bench_from_raw(r: BenchRaw) -> Result<Benchmark> {
    let target = match r.5 {
        Some(s) => serde_json::from_str(&s)?,
        None => Value::Null,
    };
    let dataset = match r.7 {
        Some(s) => serde_json::from_str(&s)?,
        None => Vec::new(),
    };
    Ok(Benchmark {
        id: r.0,
        project_id: r.1,
        name: r.2,
        rubric: r.3,
        judge_model: r.4,
        target,
        dataset_ref: r.6,
        dataset,
        rubric_id: r.8,
        baseline_score: r.9,
        created_at: parse_ts(&r.10)?,
    })
}

type RunRaw = (
    String,
    String,
    String,
    Option<String>,
    i64,
    Option<f64>,
    Option<f64>,
    f64,
    String,
    Option<i64>,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

fn map_run(row: &Row) -> rusqlite::Result<RunRaw> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
        row.get(8)?,
        row.get(9)?,
        row.get(10)?,
        row.get(11)?,
        row.get(12)?,
    ))
}

fn run_from_raw(r: RunRaw) -> Result<BenchmarkRun> {
    Ok(BenchmarkRun {
        id: r.0,
        benchmark_id: r.1,
        started_at: parse_ts(&r.2)?,
        finished_at: match r.3 {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        n_cases: r.4 as u32,
        mean_score: r.5,
        pass_rate: r.6,
        cost_usd: r.7,
        status: r.8,
        p50_latency_ms: r.9.map(|v| v as u64),
        p95_latency_ms: r.10.map(|v| v as u64),
        total_tokens: r.11.map(|v| v as u64),
        report: match r.12 {
            Some(s) => serde_json::from_str(&s)?,
            None => Value::Null,
        },
    })
}

#[cfg(test)]
mod order_tests {
    use super::*;
    use chrono::{TimeZone, Utc};

    /// The `started_at` tie-break is a contract, not an accident of the query plan.
    ///
    /// This test earns its place by being *falsifiable*: delete `, id DESC` from `list_runs` and it
    /// fails. A test that merely ran the same listing twice and compared would not — two identical
    /// queries reuse one plan and agree either way, which is exactly why the original defect
    /// survived until an index changed the plan underneath it.
    #[test]
    fn runs_that_start_together_come_back_in_a_pinned_order() {
        let c = Connection::open_in_memory().expect("in-memory db");
        crate::sqlite::schema::apply(&c).expect("schema");
        c.execute(
            "INSERT INTO benchmarks (id, project_id, name, rubric, judge_model, created_at) \
             VALUES ('b1','p1','n','r','haiku','2026-01-01T00:00:00.000000000Z')",
            [],
        )
        .expect("benchmark");

        // Inserted in an order that disagrees with the tie-break, so passing cannot be luck.
        let tied = "2026-01-01T00:00:00.000000000Z";
        for id in ["run-b", "run-c", "run-a"] {
            c.execute(
                "INSERT INTO benchmark_runs (id, benchmark_id, started_at, n_cases, cost_usd, status) \
                 VALUES (?1,'b1',?2,0,0.0,'passed')",
                params![id, tied],
            )
            .expect("run");
        }

        let ids: Vec<String> = list_runs(&c, None, "b1")
            .expect("list")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(
            ids,
            vec!["run-c", "run-b", "run-a"],
            "tied runs order by id DESC — stable across query plans, which is the property a \
             promotion gate reading 'the latest run' depends on"
        );
    }

    /// And the ordering that actually matters is still `started_at` first — the tie-break must not
    /// have quietly become the primary sort.
    #[test]
    fn a_later_start_still_outranks_a_higher_id() {
        let c = Connection::open_in_memory().expect("in-memory db");
        crate::sqlite::schema::apply(&c).expect("schema");
        c.execute(
            "INSERT INTO benchmarks (id, project_id, name, rubric, judge_model, created_at) \
             VALUES ('b1','p1','n','r','haiku','2026-01-01T00:00:00.000000000Z')",
            [],
        )
        .expect("benchmark");
        for (id, at) in [
            ("run-z", Utc.with_ymd_and_hms(2026, 1, 1, 0, 0, 0).unwrap()),
            ("run-a", Utc.with_ymd_and_hms(2026, 1, 1, 1, 0, 0).unwrap()),
        ] {
            c.execute(
                "INSERT INTO benchmark_runs (id, benchmark_id, started_at, n_cases, cost_usd, status) \
                 VALUES (?1,'b1',?2,0,0.0,'passed')",
                params![id, fmt_ts(at)],
            )
            .expect("run");
        }
        let ids: Vec<String> = list_runs(&c, None, "b1")
            .expect("list")
            .into_iter()
            .map(|r| r.id)
            .collect();
        assert_eq!(ids, vec!["run-a", "run-z"], "started_at DESC leads");
    }
}

#[cfg(test)]
mod cols_tests {
    use super::*;

    #[test]
    fn bench_cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(BENCH_COLS, tables::BENCHMARKS.select_list(Dialect::Sqlite));
    }

    #[test]
    fn run_cols_match_the_schema_model() {
        use crate::schema::{tables, Dialect};
        assert_eq!(
            RUN_COLS,
            tables::BENCHMARK_RUNS.select_list(Dialect::Sqlite)
        );
    }
}
