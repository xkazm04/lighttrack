//! The vocabulary one [`Endpoint`] row is written in.
//!
//! Every field here exists because some surface downstream needs it: `access` is what the API's
//! guards are held to, `params` become OpenAPI parameters *and* MCP input schemas, `mcp` and `cli`
//! are the coverage the bijection tests measure, `render_kind` keys the Markdown renderer.

/// The HTTP method family. Split rather than folded into a bitset: `PUT /v1/limits/:id` and
/// `DELETE /v1/limits/:id` have different access and different bodies, so they are two rows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Get,
    Post,
    Put,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "get",
            Method::Post => "post",
            Method::Put => "put",
            Method::Delete => "delete",
        }
    }

    /// `GET` is the read family; everything else is the write family. This is the split
    /// `auth_scopes` used to encode as two columns on one row.
    pub fn is_read(self) -> bool {
        matches!(self, Method::Get)
    }
}

/// The capability a project key must carry. A mirror of `lighttrack_core::Scope` as strings so this
/// crate stays dependency-free; `crates/api/src/auth_scopes.rs` holds the test that the two agree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyScope {
    Ingest,
    Read,
    Manage,
}

impl KeyScope {
    pub fn as_str(self) -> &'static str {
        match self {
            KeyScope::Ingest => "ingest",
            KeyScope::Read => "read",
            KeyScope::Manage => "manage",
        }
    }
}

/// Who may call one endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    /// Admin (or dev-mode) principals only — no project key reaches it, whatever its scopes.
    Admin,
    /// A project key carrying this scope, or an admin.
    Key(KeyScope),
    /// Authenticated by something that is not a LightTrack principal at all (the HMAC-signed
    /// billing webhook), or by nothing (`/health`, `/openapi.json`).
    Unauthenticated,
}

/// Where one published surface element sits on the removal path. `docs/DEPRECATION.md` is the
/// policy; this is the machine-readable half of it, so a caller learns the removal version from
/// the surface rather than from a changelog it never reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeprecationStage {
    /// Stage 1 — still served, still honoured, and advertised as going away.
    Advertised,
    /// Stage 2 — off by default and erroring when used, re-enablable only by
    /// `LIGHTTRACK_ALLOW_REMOVED`, which takes the version being escaped from and therefore
    /// stops matching once the deployment moves past it.
    Erroring,
}

impl DeprecationStage {
    pub fn as_str(self) -> &'static str {
        match self {
            DeprecationStage::Advertised => "advertised",
            DeprecationStage::Erroring => "erroring",
        }
    }
}

/// The marker a field or route carries while it is on its way off the published surface.
///
/// It exists on the row, beside the thing being removed, for the same reason `machine` does: the
/// reason is visible where the decision is, not in a list somewhere else that quietly grows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deprecation {
    pub stage: DeprecationStage,
    /// The release that removes it. Never a patch — see `docs/DEPRECATION.md`.
    pub removed_in: &'static str,
    /// What a caller should use instead, in one clause.
    pub replacement: &'static str,
}

impl Deprecation {
    /// The sentence appended to the surface's own prose, in both renderings.
    pub fn note(&self) -> String {
        format!(
            "DEPRECATED ({}), removed in {}: {}. Policy: docs/DEPRECATION.md.",
            self.stage.as_str(),
            self.removed_in,
            self.replacement
        )
    }
}

/// Where a parameter travels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamKind {
    /// A `:segment` of the route path.
    Path,
    /// A query-string parameter.
    Query,
    /// A field of the JSON request body. Named individually rather than hidden behind the body type
    /// because the MCP tools take body fields as flat arguments, and that is the surface an agent
    /// config depends on.
    Body,
}

/// The JSON type of a parameter — what an MCP `inputSchema` and an OpenAPI schema both need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonTy {
    String,
    Integer,
    Number,
    Boolean,
    Object,
    Array,
}

impl JsonTy {
    pub fn as_str(self) -> &'static str {
        match self {
            JsonTy::String => "string",
            JsonTy::Integer => "integer",
            JsonTy::Number => "number",
            JsonTy::Boolean => "boolean",
            JsonTy::Object => "object",
            JsonTy::Array => "array",
        }
    }
}

/// One parameter of one endpoint.
pub struct Param {
    pub name: &'static str,
    pub kind: ParamKind,
    pub ty: JsonTy,
    pub required: bool,
    pub doc: &'static str,
    /// A closed value set, or empty for an open one.
    pub enum_values: &'static [&'static str],
    /// The name this parameter takes as an MCP tool argument, when it differs from the wire name —
    /// a path `:id` is `event` / `trace` / `benchmark` to an agent, and those names are pinned.
    pub mcp_name: Option<&'static str>,
    /// A complete JSON Schema for this parameter, as raw JSON text, replacing the type/enum/doc
    /// triple. Only for the handful of write-tool arguments whose value is a nested object or an
    /// array of them: an agent that cannot see `targets[].prompt_ref` writes a benchmark whose runs
    /// can never satisfy the promotion gate, and "type: array" alone does not tell it.
    pub schema: Option<&'static str>,
    /// Required over MCP even though the wire leaves it optional. Real, and not a wart: a handler
    /// derives the project from the project key that called it, and an MCP tool has no such key to
    /// derive from — an agent that omits it gets a 400 it cannot diagnose from the schema. Pinned in
    /// `crates/mcp/tool-contract.json`, so it is also a compatibility fact, not only a nicety.
    pub mcp_required: Option<bool>,
    /// On its way off the published surface, if it is. `None` is the overwhelming majority.
    pub deprecated: Option<Deprecation>,
}

impl Param {
    pub const DEFAULT: Param = Param {
        name: "",
        kind: ParamKind::Query,
        ty: JsonTy::String,
        required: false,
        doc: "",
        enum_values: &[],
        mcp_name: None,
        schema: None,
        mcp_required: None,
        deprecated: None,
    };

    /// Is this parameter required of an MCP caller?
    pub fn required_over_mcp(&self) -> bool {
        self.mcp_required.unwrap_or(self.required)
    }

    /// The name this parameter answers to over MCP.
    pub fn arg_name(&self) -> &'static str {
        match self.mcp_name {
            Some(n) => n,
            None => self.name,
        }
    }
}

/// What an endpoint accepts or returns.
///
/// `Named`/`ArrayOf` point at a type that derives `schemars::JsonSchema`; the API resolves the name
/// to a real schema when it renders `/openapi.json`. `Untyped` is the honest answer for the many
/// handlers that build their response with `serde_json::json!` and have no struct to point at: the
/// doc string describes the shape in prose rather than inventing a struct per handler across the
/// whole API. Turning an `Untyped` into a `Named` is a strict improvement and needs no coordination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeRef {
    Named(&'static str),
    ArrayOf(&'static str),
    /// No body at all (a 204, or a `DELETE` that answers `{}`).
    Empty,
    /// An ad-hoc `json!` shape, described in prose.
    Untyped(&'static str),
}

/// The MCP tool one endpoint is reachable as. Names and argument names are a contract for existing
/// agent configurations — see `crates/mcp/tool-contract.json`.
pub struct McpTool {
    pub name: &'static str,
    pub description: &'static str,
    pub read_only: bool,
    pub idempotent: bool,
    /// Which of the endpoint's `params` the tool exposes, by wire name, in listing order.
    pub args: &'static [&'static str],
}

impl McpTool {
    pub const DEFAULT: McpTool = McpTool {
        name: "",
        description: "",
        read_only: true,
        idempotent: false,
        args: &[],
    };
}

/// One HTTP endpoint, and every surface it is reachable from.
pub struct Endpoint {
    /// Stable identifier, unique across the table. Used as the OpenAPI `operationId`.
    pub id: &'static str,
    pub method: Method,
    /// The axum route string, `:segment` and all — matched verbatim against `build_router`.
    pub path: &'static str,
    pub params: &'static [Param],
    pub body: Option<TypeRef>,
    pub response: TypeRef,
    pub access: Access,
    /// Does calling it change stored state? Drives the MCP write gate.
    pub mutating: bool,
    /// Is calling it twice the same as calling it once?
    pub idempotent: bool,
    /// Does it hand back a cursor for the next page? Two shapes exist for historical reasons — the
    /// `X-Next-Cursor` response header (events, traces, the contribution ledger) and a `next_cursor`
    /// field in the body (labels, calibrations, alerts) — and this flag covers both, because what a
    /// caller needs to know is that paging exists and that `cursor` is the argument. The generated
    /// OpenAPI description says which shape.
    pub paged: bool,
    /// A door only a program walks through: an SDK's ingest call, a device agent's lease/renew, a
    /// provider's signed webhook. Exempt from the rule that every endpoint must be reachable from
    /// MCP or the CLI — and the exemption lives on the row, where the reason is visible, rather
    /// than in an allowlist inside a test where it would quietly grow.
    pub machine: bool,
    pub mcp: Option<McpTool>,
    /// The CLI verb path, e.g. `&["limits", "status"]`.
    pub cli: Option<&'static [&'static str]>,
    /// The `lighttrack_render::render` key, when a Markdown view of the response exists.
    pub render_kind: Option<&'static str>,
    /// On its way off the published surface, if it is — the route-level twin of `Param::deprecated`.
    pub deprecated: Option<Deprecation>,
    pub doc: &'static str,
}

impl Endpoint {
    /// The neutral row every declaration starts from: a read, unpaged, uncovered, undocumented.
    /// Rows fill in what differs, so a table of 130 endpoints stays readable.
    pub const DEFAULT: Endpoint = Endpoint {
        id: "",
        method: Method::Get,
        path: "",
        params: &[],
        body: None,
        response: TypeRef::Empty,
        access: Access::Admin,
        mutating: false,
        idempotent: false,
        paged: false,
        machine: false,
        mcp: None,
        cli: None,
        render_kind: None,
        deprecated: None,
        doc: "",
    };

    pub fn param(&self, name: &str) -> Option<&Param> {
        self.params.iter().find(|p| p.name == name)
    }
}
