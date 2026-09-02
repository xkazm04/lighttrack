//! The cloud→device relay: the task queue apps enqueue into, the lease protocol an enrolled device
//! speaks, the device fleet, and the action fingerprint ledger derived from the settle events.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "enqueue_relay_task",
        method: Method::Post,
        path: "/v1/relay/tasks",
        access: Key(Ingest),
        mutating: true,
        params: &[
            br("action_type", JsonTy::String, "the action the device is to run, e.g. `xprice/reprice-summary`"),
            b("payload", JsonTy::Object, "parameters for the action — never prompts or credentials"),
            b("project_id", JsonTy::String, "admin/dev only; a project key forces its own project"),
            b("source", JsonTy::String, "who enqueued it"),
            b("idempotency_key", JsonTy::String, "the same (project, key) returns the existing task"),
            b("max_attempts", JsonTy::Integer, "attempts before the task dead-letters"),
            b("retry_interval_secs", JsonTy::Integer, "wait between attempts"),
        ],
        response: TypeRef::Untyped(
            "The created RelayTask, flattened, plus `admission: {queued: {eligible_devices}}` and \
             an optional `warning` when a soft usage limit is close. A 422 `relay_unroutable` \
             means nothing in the fleet advertises that action type.",
        ),
        cli: Some(&["relay", "tasks", "enqueue"]),
        doc: "Enqueue a device task; routability and the project's usage limits are checked first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_relay_tasks",
        method: Method::Get,
        path: "/v1/relay/tasks",
        access: Key(Read),
        params: &[
            q("project", ""),
            qe(
                "status",
                &["queued", "leased", "succeeded", "dead", "cancelling", "cancelled"],
                "only tasks in this state; an unknown value is a 400, not an empty page",
            ),
            qt("limit", JsonTy::Integer, "max tasks (default 50, max 1000)"),
        ],
        response: TypeRef::ArrayOf("RelayTask"),
        mcp: Some(McpTool {
            name: "list_relay_tasks",
            description: "Cloud→device relay tasks (newest first): work handed to an enrolled local device to run through Claude Code. Filter by project and status (queued | leased | succeeded | dead | cancelling | cancelled). Use this to answer \"did my relay task run\" — a task sitting `queued` with a low `attempts` is waiting for a device, not failing.",
            args: &["project", "status", "limit"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["relay", "tasks", "list"]),
        doc: "Relay tasks, newest first, filtered by project and status.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_relay_task",
        method: Method::Get,
        path: "/v1/relay/tasks/:id",
        access: Key(Read),
        params: &[pm("id", "task", "relay task id")],
        response: TypeRef::Named("RelayTask"),
        mcp: Some(McpTool {
            name: "get_relay_task",
            description: "One relay task by id: its status, result or error, attempt/failure counters, the device holding it, and its liveness `progress`. `failures` is the retry budget (runs that actually failed); `stale_reclaims` counts devices that died mid-run — they are different problems and the two counters exist to tell them apart.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        doc: "One relay task by id — the shape the originating app polls.",
        ..Endpoint::DEFAULT
    },
    // The device agent's settle report, authenticated by its device key and fenced by the lease it
    // was handed — never an operator's call, and never an agent's.
    Endpoint {
        id: "post_relay_result",
        method: Method::Post,
        path: "/v1/relay/tasks/:id/result",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "relay task id"),
            ber("status", &["succeeded", "failed", "deferred"], "the run's outcome"),
            b("result", JsonTy::Object, "the action's result, for `succeeded`"),
            b("error", JsonTy::String, "what went wrong, for `failed` / `deferred`"),
            b("retry_after_secs", JsonTy::Integer, "for `deferred`: when to try again"),
            b("fence", JsonTy::String, "the `lease_fence` handed out at lease time; a stale one is a 409"),
            b("model", JsonTy::String, "model the run used"),
            b("input_tokens", JsonTy::Integer, "usage from the CLI envelope"),
            b("output_tokens", JsonTy::Integer, "usage from the CLI envelope"),
            b("latency_ms", JsonTy::Integer, "how long the run took"),
            b("cost_usd", JsonTy::Number, "what the envelope says it cost — the book and flat rate are fallbacks"),
            b("mode", JsonTy::String, "the posture it ran under (generate | readonly-scan | edit)"),
            b("prompt_sha256", JsonTy::String, "fingerprint of the rendered prompt actually executed"),
            b("action_version", JsonTy::String, "the version the action declares"),
            b("input", JsonTy::Object, "the rendered prompt, only when the action set `report_io`"),
            b("output", JsonTy::Object, "the result text, only when the action set `report_io`"),
        ],
        response: TypeRef::Named("RelayTask"),
        doc: "A device reports a run's outcome; a landed report also writes the run's usage event.",
        ..Endpoint::DEFAULT
    },
    // The lease heartbeat: a device proving it still holds the task, fenced by `lease_fence`.
    Endpoint {
        id: "renew_relay_lease",
        method: Method::Post,
        path: "/v1/relay/tasks/:id/renew",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "relay task id"),
            br("fence", JsonTy::String, "the `lease_fence` handed out at lease time"),
        ],
        response: TypeRef::Named("LeaseHeld"),
        doc: "Extend a held lease; a 409 means the lease is gone and the device must stop.",
        ..Endpoint::DEFAULT
    },
    // Liveness detail from the running device, kept off the heartbeat so a stall computing progress
    // can never stall the renewal.
    Endpoint {
        id: "post_relay_progress",
        method: Method::Post,
        path: "/v1/relay/tasks/:id/progress",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "relay task id"),
            br("fence", JsonTy::String, "the `lease_fence` handed out at lease time"),
            br("progress", JsonTy::String, "what the run is doing now"),
        ],
        response: TypeRef::Named("LeaseHeld"),
        doc: "Record what a leased run is doing; fenced exactly like renew.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "cancel_relay_task",
        method: Method::Post,
        path: "/v1/relay/tasks/:id/cancel",
        access: Key(Manage),
        mutating: true,
        params: &[p("id", "relay task id")],
        response: TypeRef::Named("RelayCancel"),
        cli: Some(&["relay", "tasks", "cancel"]),
        doc: "Stop a queued or leased task; a finished one is a 409, never a silent success.",
        ..Endpoint::DEFAULT
    },
    // The lease door itself: an enrolled device asking for due work over outbound HTTPS.
    Endpoint {
        id: "lease_relay_tasks",
        method: Method::Post,
        path: "/v1/relay/lease",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            b("capabilities", JsonTy::Array, "action types this device can run, exact or `ns/*`; empty means no filter"),
            b("agent_version", JsonTy::String, "the `lt-agent` version, recorded on the device"),
            b("max", JsonTy::Integer, "tasks to lease (default 1, cap 20)"),
            b("lease_secs", JsonTy::Integer, "requested TTL, clamped to 60..1800"),
            b("wait_secs", JsonTy::Integer, "long-poll: hold up to this many seconds for work (cap 25)"),
        ],
        response: TypeRef::Untyped(
            "{ tasks: [RelayTask], renew_secs, lease_secs } — the leased work plus the renewal \
             cadence and the TTL actually granted after clamping.",
        ),
        doc: "A device leases due tasks matching its capabilities, and is told how often to renew.",
        ..Endpoint::DEFAULT
    },
    // Never over MCP: this mints a device key shown exactly once, and a key in a tool result is a
    // key in a transcript (`crates/mcp/src/relay_tools.rs` holds the test that keeps it that way).
    Endpoint {
        id: "create_relay_device",
        method: Method::Post,
        path: "/v1/relay/devices",
        access: Admin,
        mutating: true,
        params: &[
            br("name", JsonTy::String, "human name for the machine, e.g. `studio-laptop`"),
            b("project_id", JsonTy::String, "scope it to one project; omit for an operator-wide device"),
            b("capabilities", JsonTy::Array, "what it may run, exact or `ns/*`; omitted means everything"),
        ],
        response: TypeRef::Untyped(
            "The Device row, flattened, plus `key` — the raw `ltd_…` device key, shown here and \
             nowhere else ever again; only its salted digest is stored.",
        ),
        mcp: None,
        cli: Some(&["relay", "devices", "add"]),
        doc: "Enrol a device and mint its key, which is shown exactly once.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_relay_devices",
        method: Method::Get,
        path: "/v1/relay/devices",
        access: Admin,
        params: &[q("project", "one project's devices; operator-wide devices are always included")],
        response: TypeRef::Untyped(
            "[Device (no key, no digest) + { seen_secs_ago, online }] — the fleet with the only \
             liveness a device with no inbound path can have: when it last called in.",
        ),
        mcp: Some(McpTool {
            name: "list_relay_devices",
            description: "The enrolled relay device fleet: each device's advertised capabilities (the action types it can run, exactly or as `ns/*`), when it was last seen, its agent version, and whether it is revoked. Keys are never included. Read this when relay tasks are not being picked up: a queued task whose action type nothing here advertises will never run, whatever its status says. Admin key required.",
            args: &["project"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["relay", "devices", "list"]),
        doc: "The enrolled fleet: capabilities, liveness, agent version, revocation.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "revoke_relay_device",
        method: Method::Delete,
        path: "/v1/relay/devices/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "device id")],
        response: TypeRef::Named("Device"),
        cli: Some(&["relay", "devices", "revoke"]),
        doc: "Revoke a device: a flag, not a delete, so tasks it ran keep naming a device that resolves.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_relay_actions",
        method: Method::Get,
        path: "/v1/relay/actions",
        access: Key(Read),
        params: &[
            q("project", ""),
            qt("limit", JsonTy::Integer, "settle events to walk (default 1000, cap 20000)"),
        ],
        response: TypeRef::Untyped(
            "{ actions: [{action_type, prompt_sha256, versions, runs, errors, judgeable, \
             first_seen, last_seen}], scanned, truncated } — one row per action × prompt \
             fingerprint, derived from the settle events; `truncated` says the walk hit its ceiling.",
        ),
        cli: Some(&["relay", "actions"]),
        doc: "The action fingerprint ledger: which prompt text each action has been running, and when.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "snapshot_relay_action_dataset",
        method: Method::Post,
        path: "/v1/relay/actions/:action_type/dataset",
        access: Admin,
        mutating: true,
        params: &[
            p("action_type", "the namespaced action type; its `/` is percent-encoded in the path"),
            br("project_id", JsonTy::String, "the project whose succeeded runs are snapshotted"),
            b("name", JsonTy::String, "dataset name (default `relay:<action_type>`)"),
            b("limit", JsonTy::Integer, "succeeded tasks to snapshot (default 200, cap 1000)"),
        ],
        response: TypeRef::Untyped(
            "The created Dataset, flattened, plus `items` and `skipped` — runs that carried no \
             usable (payload, result) pair are counted, not silently dropped.",
        ),
        cli: Some(&["relay", "actions", "snapshot"]),
        doc: "Snapshot an action's succeeded runs into a dataset so a benchmark can gate its prompt.",
        ..Endpoint::DEFAULT
    },
];
