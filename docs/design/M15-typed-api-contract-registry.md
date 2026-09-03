# M15 — One typed API contract that generates OpenAPI, MCP tools, CLI verbs and render dispatch

Size XL · gate contract (MCP tool names/schemas are a contract for agent configs) · wave F (last:
it must see every route waves A–E added) · contexts: api-server, mcp-server, cli-tool,
rendering-core, rendering-analytics

## Problem
The route surface is described by hand four times: the axum router (`crates/api/src/main.rs`), the
MCP tool list with inline JSON schemas (`crates/mcp/src/read.rs`, `write.rs`, `prompts_tools.rs`) plus
hand-written `outputSchema`s (`schemas.rs`), the clap verb tree (`crates/cli/src/cli.rs`), and the
render dispatch keyed on the MCP tool-name string (`crates/render/src/lib.rs`). M16's
`ROUTE_SCOPES` table is a fifth partial description (auth only). Drift is already measured: MCP
`limit_rule` outputSchema lagged the input schema; several render kinds had no producer; ~16 routes
were reachable from neither MCP nor CLI; the CLI cannot page. Every wave A–E route added rows to
`ROUTE_SCOPES` by hand and MCP/CLI coverage by hand where the builder remembered.

## Design
1. `crates/contract` (new lib crate): `Endpoint { id: &'static str, method, path, params: &[Param { name, kind, required, doc }], body: Option<TypeRef>, response: TypeRef, access: { read: Access, write: Access } /* absorbs ROUTE_SCOPES */, mutating, idempotent, paged, mcp: Option<McpTool { name, read_only }>, cli: Option<&'static [&'static str]>, render_kind: Option<&'static str>, doc }`
   and `const ENDPOINTS: &[Endpoint]`. `TypeRef` names a DTO type that derives `schemars::JsonSchema`
   (add the derive to the DTOs in `core`/`api`; replace ad-hoc `json!` response bodies with typed
   structs where they exist).
2. API: a `#[test]` asserts every route string in `build_router` has exactly one `Endpoint` and vice
   versa (parse `main.rs` like `auth_scopes.rs` does), and that `Endpoint.access` equals the
   `ROUTE_SCOPES` row — then `ROUTE_SCOPES` is **deleted** and `auth_scopes.rs` reads the contract.
   Serve `GET /openapi.json` rendered from the table (paths, params, request/response schemas via
   schemars, security scheme).
3. MCP: `contract::mcp_tools(allow_writes)` replaces `read::tools()`, `schemas::output_schema()`,
   `write::tools()`'s lists; `is_write_tool` derives from `mutating`; tool **names stay identical**
   (a contract test pins the current name set — none may vanish). Keep dispatch bodies and the
   panic guard.
4. CLI: derive the `Command` tree at runtime from the table (clap `Command` builder) or generate
   `cli.rs` with a regenerate-and-diff test; the missing verbs appear for free (`limits usage`,
   `margin trend|customer|simulate`, `storage status`, `ingest status`, `rollup`, `capabilities`,
   `alerts`, `schedules`, `--cursor` paging everywhere a `paged` endpoint exists).
5. Render: `render()` keyed by `Endpoint.render_kind`; tests: every `render_kind` has a renderer,
   every renderer has a producing endpoint, every list renderer handles `[]`.
6. README/ROADMAP tool counts become a generated table (`scripts/gen-api-matrix.mjs` or a Rust test
   writing `docs/API.md`; stale-check; register in the catch-up marker).

## Out of scope
Changing any endpoint's behaviour or shape. New endpoints.

## Gates
`cargo build/test/clippy` for lighttrack-contract, -api, -mcp (`cargo check` for the bin), -cli,
-render, -core; the bijection tests; `/openapi.json` validated by a schema check in a test.

## Evaluation
Before: N routes (count), K with 0 MCP/CLI coverage, R dead render kinds, 5 hand-kept descriptions.
After: 0 uncovered routes; bijection tests enforce route ⇔ endpoint ⇔ renderer; `/openapi.json`
present; MCP tool-name set unchanged (test).
