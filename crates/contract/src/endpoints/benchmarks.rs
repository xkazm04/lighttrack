//! Benchmark definitions, their run history, the CI gate verdict, and the two doors that
//! produce a run.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_benchmark",
        method: Method::Post,
        path: "/v1/projects/:id/benchmarks",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            br("name", JsonTy::String, "benchmark name"),
            b("rubric", JsonTy::String, "freeform rubric text (single-score mode)"),
            b("rubric_id", JsonTy::String, "structured rubric id (per-dimension mode)"),
            b("judge_model", JsonTy::String, "[provider/]model (default opus@xhigh)"),
            // The first row to carry a removal marker. `target` and `targets` have been two ways
            // to say the same thing since the comparison matrix landed; a self-hosted caller that
            // still sends `target` now learns the version that stops accepting it from
            // /openapi.json and /v1/capabilities, not from a release note.
            Param {
                deprecated: Some(Deprecation {
                    stage: DeprecationStage::Advertised,
                    removed_in: "0.2.0",
                    replacement: "send a one-element `targets` array instead",
                }),
                ..b("target", JsonTy::Object, "single generation target; superseded by `targets`")
            },
            Param {
                name: "targets",
                kind: ParamKind::Body,
                ty: JsonTy::Array,
                doc: "comparison matrix: one candidate per target",
                schema: Some(crate::nested::BENCHMARK_TARGETS),
                ..Param::DEFAULT
            },
            Param {
                name: "dataset",
                kind: ParamKind::Body,
                ty: JsonTy::Array,
                doc: "inline cases",
                schema: Some(crate::nested::BENCHMARK_DATASET),
                ..Param::DEFAULT
            },
            b("dataset_ref", JsonTy::String, "stored dataset id, instead of (or beside) `dataset`"),
            b(
                "baseline_score",
                JsonTy::Number,
                "the mean a run must not fall below (0.0..=1.0 — run means are normalized, so \
                 this is a fraction and not a percentage; 85 is rejected, 0.85 is meant)",
            ),
            b(
                "schedule_interval_secs",
                JsonTy::Integer,
                "opt-in recurrence; rejected for a comparison-matrix target",
            ),
        ],
        response: TypeRef::Named("Benchmark"),
        mcp: Some(McpTool {
            name: "create_benchmark",
            description: "Create a benchmark definition. Use `rubric` (freeform text) or `rubric_id` (structured). Supply an inline `dataset` or a `dataset_ref`; `targets` defines a multi-model comparison matrix.",
            read_only: false,
            args: &[
                "id", "name", "rubric", "rubric_id", "judge_model", "dataset_ref", "dataset",
                "targets", "baseline_score",
            ],
            ..McpTool::DEFAULT
        }),
        doc: "Define a benchmark: a dataset, a rubric, a judge, and optionally a target matrix.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_benchmarks",
        method: Method::Get,
        path: "/v1/projects/:id/benchmarks",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("Benchmark"),
        mcp: Some(McpTool {
            name: "list_benchmarks",
            description: "List a project's benchmark definitions (with inline datasets).",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("list_benchmarks"),
        doc: "A project's benchmark definitions.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_benchmark",
        method: Method::Get,
        path: "/v1/benchmarks/:id",
        access: Key(Read),
        params: &[pm("id", "benchmark", "benchmark id")],
        response: TypeRef::Named("Benchmark"),
        mcp: Some(McpTool {
            name: "get_benchmark",
            description: "Fetch one benchmark definition by id.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("get_benchmark"),
        doc: "One benchmark by id, scoped to the caller's project.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_benchmark_runs",
        method: Method::Get,
        path: "/v1/benchmarks/:id/runs",
        access: Key(Read),
        params: &[pm("id", "benchmark", "benchmark id")],
        response: TypeRef::ArrayOf("BenchmarkRun"),
        mcp: Some(McpTool {
            name: "get_benchmark_runs",
            description: "Run history (scorecards: mean score, pass rate, cost, status) for a benchmark.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("get_benchmark_runs"),
        doc: "A benchmark's run history, newest first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "benchmark_gate",
        method: Method::Get,
        path: "/v1/benchmarks/:id/gate",
        access: Key(Read),
        params: &[pm("id", "benchmark", "benchmark id")],
        response: TypeRef::Untyped(
            "{ status: pass|regressed|no_baseline|no_runs|partial, run_id?, mean?, baseline?, n?, \
             judge_trust? } — the verdict from the latest FINISHED run, plus whether the judge \
             behind it has ever been checked against a human.",
        ),
        mcp: Some(McpTool {
            name: "check_benchmark_gate",
            description: "CI-gate verdict for a benchmark from its latest finished run: pass | regressed | no_baseline | no_runs, with the supporting run id, mean, baseline, and case count. Use in a pipeline step to block a regression.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("check_benchmark_gate"),
        doc: "CI-gate verdict from the latest finished run; 409 when policy requires a trusted judge.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_benchmark_run",
        method: Method::Post,
        // The runner's own report-back door: `lt-runner` posts a finished scorecard here and
        // nothing else does. An agent asks the gate; it never writes the evidence the gate reads.
        machine: true,
        path: "/v1/benchmark-runs",
        access: Key(Manage),
        mutating: true,
        body: Some(TypeRef::Named("BenchmarkRun")),
        response: TypeRef::Named("BenchmarkRun"),
        doc: "Record a finished benchmark run's scorecard; fires the run-completion alert.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "enqueue_benchmark",
        method: Method::Post,
        path: "/v1/benchmarks/:id/enqueue",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "benchmark", "benchmark id"),
            b("samples", JsonTy::Integer, "runs per case (default 1)"),
            b("heal", JsonTy::Boolean, "attempt prompt healing on low scores (default false)"),
        ],
        response: TypeRef::Named("Job"),
        mcp: Some(McpTool {
            name: "enqueue_benchmark",
            description: "Queue a benchmark run (non-blocking; `lt-runner serve` executes it). Returns the job — poll it with get_job.",
            read_only: false,
            args: &["id", "samples", "heal"],
            ..McpTool::DEFAULT
        }),
        doc: "Queue a `bench_run` job for this benchmark; a runner executes it out of band.",
        ..Endpoint::DEFAULT
    },
];
