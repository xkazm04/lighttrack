//! The use-case registry - the declared inventory of the places a project calls an LLM, and the
//! coverage report that compares it against what the events actually did.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "upsert_use_case",
        method: Method::Post,
        path: "/v1/projects/:id/use-cases",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            br(
                "key",
                JsonTy::String,
                "stable identifier events attribute to via events.name; unique per project",
            ),
            br("name", JsonTy::String, "human title for a dashboard row"),
            b("description", JsonTy::String, "what this call site is for"),
            b(
                "kind",
                JsonTy::String,
                "generation|classification|extraction|summarization|judge|agent|embedding|rerank|other",
            ),
            b(
                "status",
                JsonTy::String,
                "active|planned|deprecated - decides whether silence or traffic is the finding",
            ),
            b(
                "component",
                JsonTy::String,
                "where in the application this call site lives",
            ),
            b(
                "expected_models",
                JsonTy::Array,
                "[provider/]model ids this call site is meant to run; ABSENT means no declaration, \
                 which is not the same as 'any model is fine'",
            ),
            b("owner", JsonTy::String, "who to ask about it"),
        ],
        response: TypeRef::Named("UseCase"),
        mcp: Some(McpTool {
            name: "upsert_use_case",
            description: "Register a place this project calls an LLM, or correct one already \
                          registered. Idempotent on (project, key).",
            read_only: false,
            idempotent: true,
            args: &[
                "id",
                "key",
                "name",
                "description",
                "kind",
                "status",
                "component",
                "expected_models",
                "owner",
            ],
        }),
        cli: Some(&["use-cases", "register"]),
        doc: "Register a place this project calls an LLM, or correct one already registered. Idempotent on (project, key), so fixing a description never means deleting the row the events attribute to.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_use_cases",
        method: Method::Get,
        path: "/v1/projects/:id/use-cases",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("UseCase"),
        mcp: Some(McpTool {
            name: "list_use_cases",
            description: "List the LLM call sites this project has declared.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["use-cases", "list"]),
        doc: "List the LLM call sites this project has declared.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "use_case_coverage",
        method: Method::Get,
        path: "/v1/projects/:id/use-cases/coverage",
        access: Key(Read),
        params: &[
            pm("id", "project", "project id"),
            q("since", "RFC3339 lower bound on ts; absent means all history"),
        ],
        response: TypeRef::Untyped(
            "{ project_id, since?, declared: [UseCase + calls + undeclared_models],              shadow: [{ name, calls, models }], quiet: [key], unattributed_calls }",
        ),
        mcp: Some(McpTool {
            name: "use_case_coverage",
            description: "What this project DECLARED it uses an LLM for, against what its events \
                          actually did: declared rows with their traffic and any models they never \
                          declared, shadow rows (observed, never declared), quiet rows (declared, \
                          silent), and the count of calls carrying no use case at all.",
            args: &["id", "since"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["use-cases", "coverage"]),
        doc: "Declared against observed: which call sites have traffic, which are running a model they never declared, which traffic nobody declared at all, and how many calls carry no use case.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_use_case",
        method: Method::Get,
        path: "/v1/projects/:id/use-cases/:key",
        access: Key(Read),
        params: &[
            pm("id", "project", "project id"),
            pm("key", "use_case", "use-case key"),
        ],
        response: TypeRef::Named("UseCase"),
        mcp: Some(McpTool {
            name: "get_use_case",
            description: "Read one declared LLM call site.",
            args: &["id", "key"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["use-cases", "show"]),
        doc: "Read one declared LLM call site.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "delete_use_case",
        method: Method::Delete,
        path: "/v1/projects/:id/use-cases/:key",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            pm("key", "use_case", "use-case key"),
        ],
        response: TypeRef::Untyped("{ deleted: key } - the use case that was removed."),
        mcp: None,
        cli: Some(&["use-cases", "delete"]),
        doc: "Remove a registration. The events that referenced it are untouched - they become undeclared traffic again, which is the honest state.",
        ..Endpoint::DEFAULT
    },
];
