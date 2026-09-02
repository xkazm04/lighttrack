//! Structured rubrics — the weighted, anchored contract every judge verdict is scored
//! against.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_rubric",
        method: Method::Post,
        path: "/v1/projects/:id/rubrics",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            br("name", JsonTy::String, ""),
            br("dimensions", JsonTy::Array, "the weighted, anchored dimensions a judge scores against"),
            b("threshold", JsonTy::Number, "overall pass threshold 0-1 (default 0.7)"),
        ],
        response: TypeRef::Named("Rubric"),
        mcp: Some(McpTool {
            name: "create_rubric",
            description: "Create a structured, weighted rubric for per-dimension judging.",
            read_only: false,
            args: &["id", "name", "dimensions", "threshold"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["rubrics", "create"]),
        doc: "Create a structured rubric at generation 1.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_rubrics",
        method: Method::Get,
        path: "/v1/projects/:id/rubrics",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("Rubric"),
        mcp: Some(McpTool {
            name: "list_rubrics",
            description: "List a project's structured rubrics.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["rubrics", "list"]),
        render_kind: Some("list_rubrics"),
        doc: "A project's structured rubrics.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_rubric",
        method: Method::Get,
        path: "/v1/rubrics/:id",
        access: Key(Read),
        params: &[pm("id", "rubric", "rubric id")],
        response: TypeRef::Untyped(
            "A `Rubric` flattened, plus `active` (has anything ever been calibrated against this \
             id) and `calibrated_judges` — a new version inherits neither.",
        ),
        mcp: Some(McpTool {
            name: "get_rubric",
            description: "Fetch one rubric by id.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["rubrics", "show"]),
        render_kind: Some("get_rubric"),
        doc: "One rubric, plus whether any judge has been calibrated against it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "create_rubric_version",
        method: Method::Post,
        path: "/v1/rubrics/:id/versions",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "the rubric to supersede"),
            b("dimensions", JsonTy::Array, "the new dimensions; omitted ⇒ carried forward"),
            b("threshold", JsonTy::Number, "the new pass threshold; omitted ⇒ carried forward"),
        ],
        response: TypeRef::Named("Rubric"),
        cli: Some(&["rubrics", "version"]),
        doc: "Mint the next generation of a rubric as a new row, so stored verdicts keep their meaning.",
        ..Endpoint::DEFAULT
    },
];
