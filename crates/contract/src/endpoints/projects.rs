//! Tenancy: projects, their lifecycle, their redaction posture, and their API keys.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_project",
        method: Method::Post,
        path: "/v1/projects",
        access: Admin,
        mutating: true,
        params: &[
            br("name", JsonTy::String, "human-readable project name"),
            be(
                "redaction",
                &["none", "hash", "drop"],
                "payload persistence policy (default none)",
            ),
        ],
        response: TypeRef::Named("Project"),
        mcp: Some(McpTool {
            name: "create_project",
            description: "Create a project.",
            read_only: false,
            args: &["name", "redaction"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["projects", "create"]),
        doc: "Create a project; a caller-supplied id is validated and honoured, else one is minted.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_projects",
        method: Method::Get,
        path: "/v1/projects",
        access: Admin,
        response: TypeRef::ArrayOf("Project"),
        mcp: Some(McpTool {
            name: "list_projects",
            description: "List all projects (admin key required in enforced mode).",
            ..McpTool::DEFAULT
        }),
        cli: Some(&["projects", "list"]),
        render_kind: Some("list_projects"),
        doc: "Every project on this deployment.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "update_project",
        method: Method::Put,
        path: "/v1/projects/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "project id"),
            b("name", JsonTy::String, "new name"),
            b("enabled", JsonTy::Boolean, "false stops this project's keys opening anything"),
            be(
                "redaction",
                &["none", "hash", "drop"],
                "payload persistence policy; enforced on the NEXT ingested event",
            ),
            b("collective_opt_in", JsonTy::Boolean, "consent to collective digests"),
            b("require_trusted_judge", JsonTy::Boolean, "refuse promotion on an untrusted judge"),
        ],
        response: TypeRef::Named("Project"),
        cli: Some(&["projects", "update"]),
        doc: "Update a project; an omitted field is left as-is and the policy cache is invalidated.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "archive_project",
        method: Method::Delete,
        path: "/v1/projects/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "project id")],
        response: TypeRef::Named("Project"),
        cli: Some(&["projects", "archive"]),
        doc: "Archive a project: disabled and stamped `archived_at`, with every row kept.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_redaction_posture",
        method: Method::Get,
        path: "/v1/projects/:id/redaction",
        access: Key(Read),
        params: &[
            p("id", "project id"),
            q("since", "RFC3339 lower bound on arrival time (default 30 days back)"),
        ],
        response: TypeRef::Untyped(
            "{ project_id, since, current_rules, unaccounted_events, total_events, groups: \
             [{stamp, events, …}] } — what the stored rows actually had done to them, counted from \
             the rows rather than from the configuration.",
        ),
        cli: Some(&["projects", "redaction"]),
        doc: "What the ingest boundary actually did to this project's stored rows.",
        ..Endpoint::DEFAULT
    },
    // Secret-minting: the response carries the full key exactly once, so this must never be
    // reachable over MCP — a key in a tool result is a key in a transcript.
    Endpoint {
        id: "create_key",
        method: Method::Post,
        path: "/v1/projects/:id/keys",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "project id"),
            b("name", JsonTy::String, "key label (default `default`)"),
            b("scopes", JsonTy::Array, "ingest | read | manage; omitted ⇒ the back-compat default"),
            b("expires_at", JsonTy::String, "RFC3339 hard expiry"),
        ],
        response: TypeRef::Untyped(
            "{ id, project_id, name, prefix, key, scopes, expires_at?, created_at } — `key` is the \
             full secret, shown exactly once and never retrievable again.",
        ),
        cli: Some(&["keys", "create"]),
        doc: "Mint an API key on a project; the secret is returned once and only once.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_keys",
        method: Method::Get,
        path: "/v1/projects/:id/keys",
        access: Admin,
        params: &[p("id", "project id")],
        response: TypeRef::Untyped(
            "[{ id, name, prefix, created_at, last_used_at?, revoked, scopes, expires_at? }] — a \
             key's non-secret metadata; never the hash and never the secret.",
        ),
        cli: Some(&["keys", "list"]),
        doc: "A project's keys with their scopes, expiry, last use and revocation state.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "revoke_key",
        method: Method::Delete,
        path: "/v1/projects/:id/keys/:kid",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "project id"), p("kid", "key id")],
        response: TypeRef::Untyped("The revoked key's non-secret metadata, with `revoked: true`."),
        cli: Some(&["keys", "revoke"]),
        doc: "Revoke a key immediately; the row is kept for audit.",
        ..Endpoint::DEFAULT
    },
    // Secret-minting too: the successor's full key is in the response, so no MCP tool reaches it.
    Endpoint {
        id: "rotate_key",
        method: Method::Post,
        path: "/v1/projects/:id/keys/:kid/rotate",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "project id"),
            p("kid", "key id to rotate"),
            b(
                "grace_secs",
                JsonTy::Integer,
                "how long the predecessor keeps working (0 retires it at once)",
            ),
        ],
        response: TypeRef::Untyped(
            "{ successor: {…, key}, predecessor: {…, expires_at} } — the new secret shown once, and \
             the old key now carrying the deadline that closes the grace window.",
        ),
        cli: Some(&["keys", "rotate"]),
        doc: "Rotate a key: mint a successor and give the predecessor a deadline, not a cliff.",
        ..Endpoint::DEFAULT
    },
];
