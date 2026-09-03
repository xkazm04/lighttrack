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
            qe("kind", &["freeform", "rubric", "bench_case", "compare_cell", "pairwise_game", "calibration", "trace"], "only verdicts of this kind"),
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
        ..Endpoint::DEFAULT
    },
];
