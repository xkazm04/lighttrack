//! Ingest and the observability reads: capabilities, events, the OTLP door, traces, and the two
//! operational status surfaces.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "get_capabilities",
        method: Method::Get,
        path: "/v1/capabilities",
        access: Key(Read),
        response: TypeRef::Untyped(
            "{ backend, surfaces: {name: bool}, refuses: [name], atomic_limits: bool } — what this \
             deployment's store backend implements and what it answers 501 `unsupported` for.",
        ),
        mcp: Some(McpTool {
            name: "get_capabilities",
            description: "What this LightTrack deployment's store backend actually serves: the backend name, the surfaces it implements, the surfaces it REFUSES (whose routes answer HTTP 501 `unsupported` rather than an empty result), and whether usage caps are enforced atomically or are merely advisory. Read this before concluding a surface returned no data — a 501 here means 'not ported on this backend', never 'you have none'.",
            ..McpTool::DEFAULT
        }),
        cli: Some(&["capabilities"]),
        doc: "What this deployment's store backend serves, and what it answers 501 for.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_event",
        method: Method::Post,
        machine: true,
        path: "/v1/events",
        access: Key(Ingest),
        mutating: true,
        body: Some(TypeRef::Named("LlmEvent")),
        response: TypeRef::Untyped(
            "{ id, cost_usd, limit: {throttle, warning, binding_scope, binding_rule} } — the stored \
             event's id plus the admission verdict the caps produced.",
        ),
        doc: "Ingest one LLM call event; cost is computed and the project's limits are evaluated.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_events",
        method: Method::Get,
        path: "/v1/events",
        access: Key(Read),
        paged: true,
        params: &[
            q("project", ""),
            qt("limit", JsonTy::Integer, "max events (default 20, max 1000)"),
            q("since", "RFC3339 lower bound on event time (inclusive)"),
            q("until", "RFC3339 upper bound on event time (exclusive)"),
            q("provider", "exact provider match (anthropic, openai, …)"),
            q("model", "exact model match"),
            q("trace_id", "only events in this trace"),
            q("name", "use-case name filter (a call's `name`)"),
            qe("status", &["success", "error"], "keep only events of this status"),
            q("tag", "only events carrying this tag"),
            q("meta", "a metadata predicate: `key` or `key=value`"),
            qt("min_cost", JsonTy::Number, "minimum event cost (USD)"),
            qt("count", JsonTy::Integer, "1 also returns X-Total-Count over the whole match set"),
            q("cursor", "keyset cursor from a prior call's next_cursor"),
        ],
        response: TypeRef::ArrayOf("LlmEvent"),
        mcp: Some(McpTool {
            name: "query_events",
            description: "Recent LLM call events (newest first). Filter by project/time window/provider/model/trace/use-case name; page with `cursor` (from a prior call's next_cursor).",
            args: &[
                "project", "limit", "since", "until", "provider", "model", "trace_id", "name",
                "cursor",
            ],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["events"]),
        render_kind: Some("query_events"),
        doc: "Recent events, newest first; keyset pagination through `X-Next-Cursor`.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_events_batch",
        method: Method::Post,
        machine: true,
        path: "/v1/events/batch",
        access: Key(Ingest),
        mutating: true,
        body: Some(TypeRef::ArrayOf("LlmEvent")),
        response: TypeRef::Untyped(
            "{ accepted, rejected, invalid, results: [{index, status, id?, error?}] } — per-item \
             outcomes under HTTP 200; one bad row never fails the batch.",
        ),
        doc: "Ingest an array of events; each item is accepted, rejected or invalid on its own.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_ingest_status",
        method: Method::Get,
        path: "/v1/ingest/status",
        access: Admin,
        response: TypeRef::Untyped(
            "{ in_flight, capacity, shed_total, timeout_total, … } — the load-shedding view of the \
             ingest doors.",
        ),
        cli: Some(&["ingest", "status"]),
        doc: "Load shedding: in-flight depth plus the shed and timeout counters.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_storage_status",
        method: Method::Get,
        path: "/v1/storage/status",
        access: Admin,
        response: TypeRef::Untyped(
            "{ tables: [{name, bytes, rows}], indexes: […], latency: {family: …}, maintenance: \
             [{at, action, deferred_because?}] } — disk accounting plus the maintenance flight \
             recorder, including the passes that were DEFERRED.",
        ),
        cli: Some(&["storage", "status"]),
        doc: "Disk accounting per table and index, per-family latency, and the maintenance ledger.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_event",
        method: Method::Get,
        path: "/v1/events/:id",
        access: Key(Read),
        params: &[pm("id", "event", "event id")],
        response: TypeRef::Named("LlmEvent"),
        mcp: Some(McpTool {
            name: "get_event",
            description: "Fetch a single LLM call event by id.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("get_event"),
        doc: "One event by id.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_traces_otlp",
        method: Method::Post,
        machine: true,
        path: "/v1/traces",
        access: Key(Ingest),
        mutating: true,
        body: Some(TypeRef::Untyped(
            "An OTLP/HTTP JSON `ExportTraceServiceRequest`; OTel GenAI spans become events.",
        )),
        response: TypeRef::Untyped("{ accepted, rejected, invalid, results: […] } — as the batch door."),
        doc: "OTLP/HTTP JSON export door: GenAI spans fan into the native batch write path.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_traces",
        method: Method::Get,
        path: "/v1/traces",
        access: Key(Read),
        paged: true,
        params: &[
            q("project", ""),
            qt("limit", JsonTy::Integer, "max traces (default 20, max 1000)"),
            q("since", "RFC3339 lower bound on the trace's end time (inclusive)"),
            q("until", "RFC3339 upper bound on the trace's end time (exclusive)"),
            qe("status", &["success", "error"], "keep only traces of this status"),
            qt("min_cost", JsonTy::Number, "minimum whole-trace cost (USD)"),
            q("cursor", "keyset cursor from a prior call's next_cursor"),
        ],
        response: TypeRef::Untyped(
            "[{ trace_id, project_id, started_at, ended_at, spans, cost_usd, tokens, status }] — \
             one rollup row per trace.",
        ),
        mcp: Some(McpTool {
            name: "list_traces",
            description: "Recent agent traces (events grouped by trace_id), newest first — end-to-end cost, latency, tokens, and span count per request. Filter by project/time window/status/min cost; page with `cursor`.",
            args: &["project", "limit", "since", "until", "status", "min_cost", "cursor"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["traces"]),
        render_kind: Some("list_traces"),
        doc: "Traces (events grouped by trace_id), newest first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_trace",
        method: Method::Get,
        path: "/v1/traces/:id",
        access: Key(Read),
        params: &[pm("id", "trace", "trace id")],
        response: TypeRef::Untyped(
            "{ trace: {…totals}, spans: [ …tree… ], scores: [Score] } — one trace's rollup, its \
             span tree, and the verdicts recorded within it.",
        ),
        mcp: Some(McpTool {
            name: "get_trace",
            description: "Fetch one trace by id: rolled-up totals, the span tree, and any scores recorded within it.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["trace"]),
        render_kind: Some("get_trace"),
        doc: "One trace: totals, span tree, and scores within it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "score_trace",
        method: Method::Post,
        path: "/v1/traces/:id/score",
        access: Key(Ingest),
        mutating: true,
        params: &[
            pm("id", "trace", "trace id — the score is anchored to its root span"),
            br("rubric", JsonTy::String, "the rubric label this verdict was judged against"),
            br("value", JsonTy::Number, "the score"),
            b("max", JsonTy::Number, "the scale's maximum (default 1)"),
            b("pass", JsonTy::Boolean, "did it pass"),
            b("reasoning", JsonTy::String, "why"),
            b("cost_usd", JsonTy::Number, "what judging it cost"),
            b("scored_by", JsonTy::String, "who or what scored it (default `mcp`)"),
        ],
        response: TypeRef::Named("Score"),
        mcp: Some(McpTool {
            name: "score_trace",
            description: "Record a verdict on a WHOLE trace (an agent run end to end), not one call. Anchored to the trace's root span, so trace-level quality is comparable across runs.",
            read_only: false,
            args: &["id", "rubric", "value", "max", "pass", "reasoning", "cost_usd", "scored_by"],
            ..McpTool::DEFAULT
        }),
        doc: "Score a whole trace, anchored to its root span.",
        ..Endpoint::DEFAULT
    },
];
