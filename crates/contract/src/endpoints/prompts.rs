//! The prompt registry: versions, the label pointers a runtime fetch resolves, the gating
//! benchmark link, the canary policy, and promotion.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_prompt",
        method: Method::Post,
        path: "/v1/projects/:id/prompts",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "project id"),
            br("name", JsonTy::String, "registry prompt name (unique per project)"),
            br("content", JsonTy::String, "content of version 1"),
            b("config", JsonTy::Object, "structured config (model, params, variable schema)"),
            b("note", JsonTy::String, "change note for version 1"),
            b("benchmark_id", JsonTy::String, "the benchmark whose regression check gates promotions"),
        ],
        response: TypeRef::Untyped(
            "{ prompt: Prompt, version: PromptVersion, enqueued_job? } — the registry entry, its \
             version 1, and the benchmark job the cut auto-enqueued.",
        ),
        cli: Some(&["prompts", "create"]),
        doc: "Register a new prompt with its first version; 409 when the name already exists.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_prompts",
        method: Method::Get,
        path: "/v1/projects/:id/prompts",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("Prompt"),
        mcp: Some(McpTool {
            name: "list_prompts",
            description: "List a project's registry prompts with their label→version pointers and linked benchmark.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["prompts", "list"]),
        render_kind: Some("list_prompts"),
        doc: "A project's registry prompts with their label pointers and gating benchmark.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_prompt",
        method: Method::Get,
        path: "/v1/projects/:id/prompts/:name",
        access: Key(Read),
        params: &[
            pm("id", "project", "project id"),
            p("name", "registry prompt name"),
            q("label", "resolve the version this label points at (e.g. production)"),
            qt("version", JsonTy::Integer, "resolve this exact version number"),
        ],
        response: TypeRef::Untyped(
            "{ id, name, version, label?, tag, content, config?, note? } — one resolved version; \
             `tag` is the `\"<name>@v<n>\"` attribution string to stamp on the traffic it produces.",
        ),
        mcp: Some(McpTool {
            name: "get_prompt",
            description: "Resolve one registry prompt to a concrete version's text: by explicit `version`, by `label` (e.g. production), or — absent both — the latest version.",
            args: &["id", "name", "label", "version"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("get_prompt"),
        doc: "Runtime fetch: resolve a prompt by version, by label, or to its latest version.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "link_prompt_benchmark",
        method: Method::Put,
        path: "/v1/projects/:id/prompts/:name",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "project id"),
            p("name", "registry prompt name"),
            b("benchmark_id", JsonTy::String, "the gating benchmark; null unlinks"),
        ],
        response: TypeRef::Named("Prompt"),
        cli: Some(&["prompts", "link"]),
        doc: "Point an existing prompt at the benchmark whose regression check gates its promotions.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "add_prompt_version",
        method: Method::Post,
        path: "/v1/projects/:id/prompts/:name/versions",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            p("name", "registry prompt name"),
            br("content", JsonTy::String, "the prompt text / template for this version"),
            b("config", JsonTy::Object, "structured config (model, params, variable schema)"),
            b("note", JsonTy::String, "change note describing why this version was cut"),
            // Only the MCP tool's create-on-404 fallback carries this; the `/versions` body ignores it.
            b("benchmark_id", JsonTy::String, "gating benchmark — honored only when the prompt is created"),
        ],
        response: TypeRef::Untyped(
            "{ version: PromptVersion, enqueued_job? } — the new version and the benchmark job the \
             cut auto-enqueued.",
        ),
        mcp: Some(McpTool {
            name: "create_prompt_version",
            description: "Add a new version to a registry prompt, creating the prompt if it does not exist yet. A new version auto-enqueues the linked benchmark (poll it with get_job). `benchmark_id` is only honored when the prompt is first created.",
            read_only: false,
            args: &["id", "name", "content", "config", "note", "benchmark_id"],
            ..McpTool::DEFAULT
        }),
        doc: "Cut the next monotonic version; the linked benchmark is auto-enqueued against it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_prompt_versions",
        method: Method::Get,
        path: "/v1/projects/:id/prompts/:name/versions",
        access: Key(Read),
        params: &[p("id", "project id"), p("name", "registry prompt name")],
        response: TypeRef::ArrayOf("PromptVersion"),
        cli: Some(&["prompts", "versions"]),
        doc: "Every stored version of one registry prompt.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "set_prompt_canary",
        method: Method::Put,
        path: "/v1/projects/:id/prompts/:name/canary",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "project id"),
            p("name", "registry prompt name"),
            b("canary", JsonTy::Object, "the CanaryPolicy to store; null clears it"),
        ],
        response: TypeRef::Named("Prompt"),
        cli: Some(&["prompts", "canary"]),
        doc: "Set or clear the online canary policy; a policy that could never fire is refused.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "promote_prompt",
        method: Method::Post,
        path: "/v1/projects/:id/prompts/:name/promote",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            p("name", "registry prompt name"),
            br("label", JsonTy::String, "the label to move, e.g. production"),
            br("version", JsonTy::Integer, "the version number the label should point at"),
            b("force", JsonTy::Boolean, "override the benchmark regression gate (default false)"),
        ],
        response: TypeRef::Untyped(
            "a Prompt, flattened, plus `warning?` when the gate could not verify the run generated \
             with this version, and `judge_trust?` for the judge behind that evidence.",
        ),
        mcp: Some(McpTool {
            name: "promote_prompt",
            description: "Point a label (e.g. production) at a version. Blocked (409) when the prompt's linked benchmark regressed below its baseline — pass force=true to override an intentional rollout.",
            read_only: false,
            idempotent: true,
            args: &["id", "name", "label", "version", "force"],
        }),
        doc: "Point a label at a version, blocked (409) when the gating benchmark has regressed.",
        ..Endpoint::DEFAULT
    },
];
