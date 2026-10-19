//! Judge verdicts: recording one, and reading the ledger back.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "post_score",
        method: Method::Post,
        path: "/v1/scores",
        access: Key(Ingest),
        mutating: true,
        // `project_id`/`event_id` are the wire names; MCP pins the shorter agent-facing ones.
        params: &[
            Param {
                name: "project_id",
                kind: ParamKind::Body,
                doc: "project id (required with an admin key; a project key derives it)",
                mcp_name: Some("project"),
                ..Param::DEFAULT
            },
            br("rubric", JsonTy::String, "rubric name/label this verdict is against"),
            br("value", JsonTy::Number, "score achieved"),
            b("max", JsonTy::Number, "maximum possible score (default 1.0)"),
            b("pass", JsonTy::Boolean, "pass/fail verdict"),
            b("reasoning", JsonTy::String, "the judge's rationale"),
            b("scored_by", JsonTy::String, "who/what produced this score (default `mcp` over MCP)"),
            b("cost_usd", JsonTy::Number, "cost of the judge call, for visibility"),
            Param {
                name: "event_id",
                kind: ParamKind::Body,
                doc: "event id this score judges (optional)",
                mcp_name: Some("event"),
                ..Param::DEFAULT
            },
        ],
        body: Some(TypeRef::Named("Score")),
        response: TypeRef::Named("Score"),
        mcp: Some(McpTool {
            name: "record_score",
            description: "Record an LLM-as-judge score against a rubric. Optionally tie it to the `event` it judges. `project` is required with an admin key (a project key derives it).",
            read_only: false,
            args: &["project_id", "rubric", "value", "max", "pass", "reasoning", "scored_by", "cost_usd", "event_id"],
            ..McpTool::DEFAULT
        }),
        doc: "Record one judge verdict; the rolling quality-regression window sees it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_scores",
        method: Method::Get,
        path: "/v1/scores",
        access: Key(Read),
        params: &[
            q("project", ""),
            q("run", "only this benchmark run's case results, in case order"),
            q("rubric_id", "only verdicts judged against this stored rubric; survives a rubric rename, unlike the free-text `rubric` label"),
            qe("kind", &["freeform", "rubric", "bench_case", "compare_cell", "pairwise_game", "calibration", "trace", "other"], "only verdicts of this kind"),
            qt("limit", JsonTy::Integer, "max scores (default 50, max 1000; run-scoped: default 5000)"),
            q("needs_review", "1/true keeps only verdicts a human should look at (M11)"),
            qt("threshold", JsonTy::Number, "the pass threshold `needs_review` measures near-misses against (default 0.7)"),
        ],
        response: TypeRef::ArrayOf("Score"),
        mcp: Some(McpTool {
            name: "list_scores",
            description: "Recent LLM-as-judge scores (newest first). Optionally narrowed to one project, one rubric (`rubric_id`), or one kind of verdict (`kind`) - a benchmark case is not the same measurement as an ad-hoc score, and averaging them together is the mistake this filter exists to prevent.",
            args: &["project", "rubric_id", "kind", "limit"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("list_scores"),
        doc: "Judge verdicts, newest first; narrowable to one run, rubric, kind, or the review queue.",
        degrades: &["score_filters"],
        ..Endpoint::DEFAULT
    },
    // An aggregate over the *verdicts* above rather than over the benchmark row, which is why it is
    // declared here beside them: its only store read is `list_run_scores`, and what it publishes is
    // the per-dimension signature those verdicts already carry.
    Endpoint {
        id: "benchmark_regression",
        method: Method::Get,
        path: "/v1/benchmarks/{id}/regression",
        // The same credential the gate is served to. A regression table is the gate's verdict with
        // its reasons attached; a key that may read one may read the other.
        access: Key(Read),
        params: &[
            pm("id", "benchmark", "benchmark id"),
            qr("run", "the finished run under scrutiny"),
            q(
                "against",
                "the run to compare it against (default: the previous finished run of this benchmark)",
            ),
        ],
        response: TypeRef::Untyped(
            "{ status: ok|no_dimensions, reason?, dimensions: [{ key, status: \
             compared|coverage_changed, mean_before?, mean_after?, delta?, n_before, n_after, \
             floor_hits_before, floor_hits_after, voided_before, voided_after }], flipped: [{ \
             case_index, dimension?, reasonings }], families_before, families_after: [{ family, \
             status: measured|not_measured, cases, denominator }], unpaired_before, unpaired_after \
             } — the per-dimension delta table, worst drop first; 409 when the two runs were not \
             scored by the same instrument.",
        ),
        mcp: Some(McpTool {
            name: "get_benchmark_regression",
            description: "Why a benchmark regressed, not just that it did: the per-dimension delta table between two finished runs (worst-dropping dimension first), the cases whose verdict flipped with the judge's own reasoning for the dimension that moved, and failure-family counts (parse_failures, low_agreement, injection_suspected, floor_hit) each with the denominator of cases that could have tripped it — a family whose denominator is 0 reads `not_measured`, never 0. `against` defaults to the previous finished run. 409 when the two runs were scored by different judges or rubrics, because that is a methodology change rather than a regression.",
            args: &["id", "run", "against"],
            ..McpTool::DEFAULT
        }),
        doc: "Per-dimension regression between two finished runs: deltas, flipped cases with the judge's words, and failure families with their denominators.",
        ..Endpoint::DEFAULT
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    /// The list door 400s with every `ScoreKind`, including `other` (a verdict this build does
    /// not classify). Leaving it out teaches an agent that the value is illegal.
    #[test]
    fn list_scores_names_every_score_kind() {
        let row = ENDPOINTS.iter().find(|e| e.id == "list_scores").unwrap();
        assert_eq!(
            row.param("kind").expect("kind").enum_values,
            &[
                "freeform",
                "rubric",
                "bench_case",
                "compare_cell",
                "pairwise_game",
                "calibration",
                "trace",
                "other",
            ]
        );
    }
}
