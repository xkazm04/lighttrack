//! The alert ledger's read/act surface, and the routing config behind it: what fired, who saw it,
//! what came of it, and where the next one goes.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "list_alerts",
        method: Method::Get,
        path: "/v1/alerts",
        // `read` sees its OWN project's alerts — `resolve_read_project` narrows a project key.
        access: Key(Read),
        paged: true,
        // The cursor comes back in the body as `next_cursor` rather than in `X-Next-Cursor`. It is
        // still a cursor and `cursor=` is still how you ask for the next page, so the flag is set:
        // what a caller needs to know is that paging exists, not which envelope carries it.
        params: &[
            q("project", ""),
            qe(
                "kind",
                &[
                    "limit_breach",
                    "limit_warning",
                    "forecast_alert",
                    "relay_task_dead",
                    "error_spike",
                    "score_drop",
                    "bench_run",
                    "ingest_rejected",
                ],
                "only alerts of this kind",
            ),
            q("since", "window start: an RFC3339 instant, or a relative 30m / 24h / 7d"),
            qt(
                "acked",
                JsonTy::Boolean,
                "true = only acknowledged, false = only open (the on-call view); omit for both",
            ),
            qt("limit", JsonTy::Integer, "max alerts (default 20)"),
            q("cursor", "keyset cursor from a prior call's next_cursor"),
        ],
        response: TypeRef::Untyped(
            "{ alerts: [Alert], next_cursor } — the fired-alert ledger newest first, each row \
             carrying its deliveries, acknowledgement and resolution.",
        ),
        mcp: Some(McpTool {
            name: "list_alerts",
            description: "The fired-alert ledger: what LightTrack has actually alerted on (limit breaches, spend forecasts, error spikes, quality regressions, dead relay tasks, finished benchmark runs, rejected ingest), whether each delivery LANDED, who acknowledged it, and what came of it. This is the durable record — an alert that fired while nobody was watching is here, and `delivered: []` means the alert reached no channel at all.",
            args: &["project", "kind", "since", "acked", "limit", "cursor"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["alerts", "list"]),
        render_kind: Some("list_alerts"),
        doc: "The fired-alert ledger, newest first: deliveries, acknowledgement and resolution.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "ack_alert",
        method: Method::Post,
        path: "/v1/alerts/:id/ack",
        // Acknowledging is a state change on shared operational record, so it needs `manage`.
        access: Key(Manage),
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "the alert id from list_alerts"),
            b(
                "by",
                JsonTy::String,
                "who saw it — an on-call handle, an email, a runbook link. Defaults server-side to \
                 the calling key's label.",
            ),
        ],
        response: TypeRef::Untyped("{ acked, acked_at } — the alert id and the instant it was seen."),
        mcp: Some(McpTool {
            name: "ack_alert",
            description: "Acknowledge one fired alert: record that a human (or you, on their behalf) has SEEN it. Idempotent — acking twice is the same fact. This does not resolve or silence anything; the alert stays in the ledger and its cooldown is unaffected. Find open alerts with `list_alerts` and `acked: false`.",
            read_only: false,
            idempotent: true,
            args: &["id", "by"],
        }),
        cli: Some(&["alerts", "ack"]),
        doc: "Acknowledge one alert: record that someone saw it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "attach_alert_resolution",
        method: Method::Post,
        // A resolution is written by the responder (an admin-keyed service), not by an app.
        machine: true,
        path: "/v1/alerts/:id/resolution",
        access: Admin,
        mutating: true,
        params: &[p("id", "the alert id being closed out")],
        body: Some(TypeRef::Untyped(
            "Any JSON object — the responder's diagnosis, stored verbatim on the alert.",
        )),
        response: TypeRef::Untyped("{ resolved } — the alert id the resolution was attached to."),
        doc: "Attach what came of an alert, turning a notification into a closed loop.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_alert_channels",
        method: Method::Get,
        path: "/v1/projects/:id/alert-channels",
        // Where a project's alerts go is instance configuration, not a tenant read.
        access: Admin,
        params: &[p("id", "project id")],
        response: TypeRef::ArrayOf("AlertChannel"),
        cli: Some(&["alerts", "channels", "list"]),
        doc: "The channels this project's alerts effectively reach: its own plus the inherited globals.",
        ..Endpoint::DEFAULT
    },
    // No MCP tool, deliberately: this mints a webhook signing secret and returns it exactly once,
    // and a secret in a tool result is a secret in a transcript.
    Endpoint {
        id: "put_alert_channel",
        method: Method::Put,
        path: "/v1/projects/:id/alert-channels",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "project id"),
            ber("kind", &["webhook", "ntfy", "email"], "the destination's transport"),
            br("target", JsonTy::String, "the URL (webhook/ntfy) or address (email)"),
            be(
                "min_severity",
                &["info", "warning", "critical"],
                "severity floor for this channel (default info)",
            ),
            b("kinds", JsonTy::Array, "alert kinds this channel wants; empty = every kind"),
            b("enabled", JsonTy::Boolean, "deliver to it (default true)"),
            b(
                "signed",
                JsonTy::Boolean,
                "sign this channel's deliveries; the secret is returned once and never stored",
            ),
        ],
        response: TypeRef::Untyped(
            "The redacted AlertChannel, plus `secret` and `secret_note` when `signed` — the \
             plaintext signing secret, shown exactly once.",
        ),
        cli: Some(&["alerts", "channels", "set"]),
        doc: "Add a routing destination for a project's alerts; a signed one is minted a secret once.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "delete_alert_channel",
        method: Method::Delete,
        path: "/v1/projects/:id/alert-channels/:cid",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "project id"), p("cid", "alert channel id")],
        response: TypeRef::Untyped("{ deleted } — the channel id that was removed."),
        cli: Some(&["alerts", "channels", "delete"]),
        doc: "Remove one stored routing destination.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "test_alert_channel",
        method: Method::Post,
        path: "/v1/alert-channels/:id/test",
        // Sending a real, signed test alert is a use of the deployment's own credentials.
        access: Admin,
        mutating: true,
        params: &[p("id", "alert channel id")],
        response: TypeRef::Untyped(
            "{ channel_id, target, signed, ok, status } — what a real test delivery down this \
             channel actually did.",
        ),
        cli: Some(&["alerts", "channels", "test"]),
        doc: "Send a real, signed test alert down one channel and report whether it landed.",
        ..Endpoint::DEFAULT
    },
];
