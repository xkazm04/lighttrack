//! Usage governance: the cap rules, the standing margin policies that mint them, and the
//! two reads that say where a project stands against them.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "create_limit",
        method: Method::Post,
        path: "/v1/projects/:id/limits",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "project id"),
            ber("metric", &["cost_usd", "calls", "tokens"], "what is capped"),
            ber("window", &["hour", "day", "month"], "the rolling window"),
            br(
                "threshold",
                JsonTy::Number,
                "a fixed cap, or `{\"pct\": N}` for one derived from recognized revenue",
            ),
            be("action", &["alert", "throttle", "block"], "what a breach does (default alert)"),
        ],
        response: TypeRef::Named("LimitRule"),
        mcp: Some(McpTool {
            name: "create_limit",
            description: "Add a usage-limit rule to a project (applies to monitored ingest traffic only — the judge is exempt).",
            read_only: false,
            args: &["id", "metric", "window", "threshold", "action"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["limits", "set"]),
        doc: "Add a usage-limit rule to a project; monitored ingest only, never the judge.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_limits",
        method: Method::Get,
        path: "/v1/projects/:id/limits",
        access: Key(Read),
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("LimitRule"),
        mcp: Some(McpTool {
            name: "list_limits",
            description: "List a project's configured limit rules.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["limits", "list"]),
        render_kind: Some("list_limits"),
        doc: "A project's limit rules, enabled and disabled alike.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "update_limit",
        method: Method::Put,
        path: "/v1/limits/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "limit rule id"),
            ber("metric", &["cost_usd", "calls", "tokens"], "what is capped"),
            ber("window", &["hour", "day", "month"], "the rolling window"),
            br(
                "threshold",
                JsonTy::Number,
                "a fixed cap, or `{\"pct\": N}` for one derived from recognized revenue",
            ),
            be("action", &["alert", "throttle", "block"], "what a breach does (default alert)"),
            b("enabled", JsonTy::Boolean, "toggle the rule on/off (default true)"),
            b("warn_at", JsonTy::Number, "soft-warning fraction of threshold in (0,1)"),
            b("scope", JsonTy::Object, "dimension scope, e.g. {\"model\":\"gpt-4o\"}"),
        ],
        response: TypeRef::Named("LimitRule"),
        mcp: Some(McpTool {
            name: "update_limit",
            description: "Replace a usage-limit rule wholesale (by rule id). Toggle it with `enabled`, retune `threshold`/`warn_at`, or change `scope`. `metric`/`window`/`threshold` are required (the rule is replaced, not patched); `project_id` is immutable.",
            read_only: false,
            idempotent: true,
            args: &["id", "metric", "window", "threshold", "action", "enabled", "warn_at", "scope"],
        }),
        cli: Some(&["limits", "update"]),
        doc: "Replace a limit rule wholesale; the sweep-owned fields are carried over, not cleared.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "delete_limit",
        method: Method::Delete,
        path: "/v1/limits/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "limit rule id")],
        response: TypeRef::Untyped("{ deleted: id } — the rule that was removed."),
        mcp: Some(McpTool {
            name: "delete_limit",
            description: "Remove a usage-limit rule by id. Idempotent from the caller's view; 404s if the id is unknown.",
            read_only: false,
            idempotent: true,
            args: &["id"],
        }),
        cli: Some(&["limits", "delete"]),
        doc: "Remove a limit rule by id.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "create_margin_policy",
        method: Method::Post,
        path: "/v1/projects/:id/margin-policies",
        access: Admin,
        mutating: true,
        params: &[
            p("id", "project id"),
            br("trigger", JsonTy::Object, "the margin condition that arms the policy"),
            br("action", JsonTy::Object, "the limit rule it creates when armed"),
            b("min_cost_usd", JsonTy::Number, "windowed cost a subject must exceed to be acted on"),
            b("cooldown_secs", JsonTy::Integer, "gap between actions (default 3600)"),
            b("expiry_secs", JsonTy::Integer, "how long a created rule lives (default 86400)"),
            b("enabled", JsonTy::Boolean, "default true"),
        ],
        response: TypeRef::Named("MarginPolicy"),
        cli: Some(&["margin-policies", "create"]),
        doc: "Store a standing margin guardrail; the forecast sweep is what turns it into rules.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_margin_policies",
        method: Method::Get,
        path: "/v1/projects/:id/margin-policies",
        access: Admin,
        params: &[pm("id", "project", "project id")],
        response: TypeRef::ArrayOf("MarginPolicy"),
        mcp: Some(McpTool {
            name: "list_margin_policies",
            description: "List a project's standing margin guardrails: the policies that turn a loss-making or eroding customer into a limit rule automatically. Read-only — the rules they create show up in `list_limits` carrying an `origin`.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["margin-policies", "list"]),
        doc: "A project's standing margin guardrails.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "delete_margin_policy",
        method: Method::Delete,
        path: "/v1/projects/:id/margin-policies/:pid",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "project id"), p("pid", "margin policy id")],
        response: TypeRef::Untyped("{ deleted: id } — the policy that was removed."),
        cli: Some(&["margin-policies", "delete"]),
        doc: "Remove a margin policy; the rules it created are reaped by the sweep, not here.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "limits_status",
        method: Method::Get,
        path: "/v1/limits/status",
        access: Key(Read),
        params: &[q("project", "project id; required unless the key already names one")],
        response: TypeRef::Untyped(
            "{ project_id, throttled, statuses: [LimitStatus], rejected: [{metric, window, scope, \
             …}], cost_basis: {unpriced_calls, imputed_cost_usd, client_reported_cost_usd, \
             unpriceable, derived_thresholds, inert_thresholds, notes} } — the caps evaluated now, \
             plus how much of them rests on weak cost evidence.",
        ),
        mcp: Some(McpTool {
            name: "get_limit_status",
            description: "Evaluate a project's limit rules now; per-rule status + overall throttle flag.",
            args: &["project"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["limits", "status"]),
        render_kind: Some("get_limit_status"),
        doc: "Evaluate a project's limits now: per-rule status, the throttle flag, cost provenance.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "limits_usage",
        method: Method::Get,
        path: "/v1/limits/usage",
        access: Key(Read),
        params: &[
            q("project", "project id; required unless the key already names one"),
            qe(
                "by",
                &["api_key", "customer", "model", "provider", "name"],
                "the dimension to group by (default api_key)",
            ),
            qe("window", &["hour", "day", "month"], "rolling window (default day)"),
            qt("limit", JsonTy::Integer, "max rows (default 20, max 200)"),
        ],
        response: TypeRef::Untyped(
            "{ project_id, by, window, since, total, entries: [{value, label?, …usage, \
             cost_share_pct, rules: [LimitStatus]}], truncated? } — who is spending, and every \
             scoped rule that currently binds them.",
        ),
        cli: Some(&["limits", "usage"]),
        doc: "Rolling usage broken down by one scope dimension — the 'who is spending' view.",
        ..Endpoint::DEFAULT
    },
];
