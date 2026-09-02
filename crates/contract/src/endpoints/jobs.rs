//! The background job queue and the stored schedules that feed it: the operator's enqueue/read/cancel
//! doors, the worker's lease protocol (claim → progress → renew → finish), and schedule CRUD.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "list_jobs",
        method: Method::Get,
        path: "/v1/jobs",
        access: Admin,
        params: &[
            qe(
                "status",
                &["queued", "running", "cancelling", "cancelled", "done", "failed"],
                "keep only jobs in this state",
            ),
            qt("limit", JsonTy::Integer, "max jobs (default 50, max 1000)"),
        ],
        response: TypeRef::ArrayOf("Job"),
        mcp: Some(McpTool {
            name: "list_jobs",
            description: "List background jobs (benchmark runs). Optionally filter by status.",
            args: &["status", "limit"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["jobs", "list"]),
        render_kind: Some("list_jobs"),
        doc: "Recent queue rows, newest first; scoped so a key never reads another tenant's payloads.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "enqueue_job",
        method: Method::Post,
        path: "/v1/jobs",
        access: Admin,
        mutating: true,
        params: &[
            ber(
                "type",
                &["bench_run", "score_events", "score_traces", "dataset_sample", "calibrate"],
                "the job kind",
            ),
            b("payload", JsonTy::Object, "kind-specific fields, validated against `type` at the door"),
        ],
        response: TypeRef::Named("Job"),
        mcp: Some(McpTool {
            name: "enqueue_job",
            description: "Queue one unit of background work of any kind (bench_run | score_events | score_traces | dataset_sample | calibrate). Non-blocking; a worker executes it — poll with get_job. The payload is validated against the kind, so a malformed one is refused here rather than dead-lettering three attempts later.",
            read_only: false,
            args: &["type", "payload"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["jobs", "enqueue"]),
        render_kind: Some("get_job"),
        doc: "Enqueue one unit of background work of any known kind; an unknown type is a 400.",
        ..Endpoint::DEFAULT
    },
    // The worker's lease protocol. `claim`/`progress`/`renew`/`finish` are what a running worker
    // calls on a timer against a job it holds — never an operator or an agent, so they are machine
    // doors and carry no MCP tool or CLI verb.
    Endpoint {
        id: "claim_job",
        method: Method::Post,
        path: "/v1/jobs/claim",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            b("stale_secs", JsonTy::Integer, "how long a lease may go unrenewed before it is reclaimable (default 120)"),
            b("kinds", JsonTy::Array, "the job kinds this worker can execute; empty means any"),
            b("providers", JsonTy::Array, "the providers this worker holds credentials for (advisory; logged, not filtered)"),
        ],
        response: TypeRef::Untyped(
            "A `Job` — the row this worker now holds, with its `claimed_at` lease fence — or `null` \
             when nothing claimable is queued.",
        ),
        doc: "Atomically claim one queued (or stale-leased) job of a kind this worker can run.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_job",
        method: Method::Get,
        path: "/v1/jobs/:id",
        access: Admin,
        params: &[pm("id", "job", "job id")],
        response: TypeRef::Named("Job"),
        mcp: Some(McpTool {
            name: "get_job",
            description: "Fetch one job by id — poll a benchmark run's status / progress / result.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["jobs", "show"]),
        render_kind: Some("get_job"),
        doc: "One job: status, progress, attempts, error and result.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "cancel_job",
        method: Method::Post,
        path: "/v1/jobs/:id/cancel",
        access: Admin,
        mutating: true,
        params: &[p("id", "job id")],
        response: TypeRef::Untyped(
            "{ outcome: \"cancelled\" | \"cancelling\" } — a queued job is stopped outright, a \
             running one is marked `cancelling` and stops at its next case boundary. A job that \
             already reached a terminal state is a 409, never a silent success.",
        ),
        cli: Some(&["jobs", "cancel"]),
        doc: "Ask a queued or running job to stop; cancelling a finished job is a 409.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "job_progress",
        method: Method::Post,
        path: "/v1/jobs/:id/progress",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "job id"),
            br("progress", JsonTy::String, "a short human-readable progress line"),
        ],
        response: TypeRef::Untyped("{ ok: true }"),
        doc: "Record a running job's progress line; carried apart from the heartbeat on purpose.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "job_renew",
        method: Method::Post,
        path: "/v1/jobs/:id/renew",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "job id"),
            br("claimed_at", JsonTy::String, "the lease fence handed out at claim — proof the job is still this worker's"),
        ],
        response: TypeRef::Untyped(
            "{ claimed_at } — the extended lease. A **409** means the lease is no longer this \
             worker's (reaped, requeued or reclaimed) and the run must stop.",
        ),
        doc: "Heartbeat: extend this worker's lease, or learn by 409 that it lost the job.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "job_finish",
        method: Method::Post,
        path: "/v1/jobs/:id/finish",
        access: Admin,
        mutating: true,
        machine: true,
        params: &[
            p("id", "job id"),
            br("status", JsonTy::String, "the terminal status to record (done | failed | cancelled)"),
            b("result", JsonTy::Object, "the verdict payload"),
            b("error", JsonTy::String, "why it failed, when it did"),
            b("claimed_at", JsonTy::String, "the lease fence; omitted for an operator-shaped finish"),
        ],
        response: TypeRef::Untyped(
            "{ ok: true } — or a **409** naming the lease that beat this one, meaning the verdict \
             sent was NOT recorded.",
        ),
        doc: "Record a job's verdict, conditioned on it being non-terminal and still held.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "create_schedule",
        method: Method::Post,
        path: "/v1/projects/:id/schedules",
        access: Admin,
        mutating: true,
        params: &[
            pm("id", "project", "the project this workload recurs for"),
            ber(
                "type",
                &["bench_run", "score_events", "score_traces", "dataset_sample", "calibrate"],
                "the job kind enqueued each time it fires",
            ),
            b("payload", JsonTy::Object, "the job payload enqueued each time it fires"),
            br("interval_secs", JsonTy::Integer, "how often it fires (floor 60)"),
            b("start_in_secs", JsonTy::Integer, "seconds until the first firing (default 0 = at once)"),
            b("enabled", JsonTy::Boolean, "default true; false stores it paused"),
        ],
        response: TypeRef::Named("Schedule"),
        mcp: Some(McpTool {
            name: "create_schedule",
            description: "Make a workload RECUR: store a schedule (a job kind + payload on an interval) the server sweeps. This is how a compare benchmark recurs — its matrix target cannot carry a recurrence field — and how scoring/sampling/calibration recur without a daemon process being kept alive.",
            read_only: false,
            args: &["id", "type", "payload", "interval_secs", "start_in_secs", "enabled"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["schedules", "create"]),
        doc: "Store a recurring workload; its payload is validated against its kind at creation.",
        ..Endpoint::DEFAULT
    },
    // `list_schedules` is attached here rather than to `/v1/schedules`: the tool's one argument is a
    // project, and this is the only row that actually has that parameter — the deployment-wide
    // listing is what it falls back to when the argument is omitted.
    Endpoint {
        id: "list_project_schedules",
        method: Method::Get,
        path: "/v1/projects/:id/schedules",
        access: Key(Read),
        params: &[pm("id", "project", "one project's schedules; omit over MCP for every project's")],
        response: TypeRef::ArrayOf("Schedule"),
        mcp: Some(McpTool {
            name: "list_schedules",
            description: "List recurring workloads: every stored schedule (a job kind + payload on an interval), for one project or the whole deployment. This is the answer to \"what runs on a schedule here\" — including recurring compare benchmarks, which cannot express recurrence any other way.",
            args: &["id"],
            ..McpTool::DEFAULT
        }),
        render_kind: Some("list_schedules"),
        doc: "One project's schedules; a project key reads its own, a mismatch is a 403.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_all_schedules",
        method: Method::Get,
        path: "/v1/schedules",
        access: Admin,
        response: TypeRef::ArrayOf("Schedule"),
        cli: Some(&["schedules", "list"]),
        render_kind: Some("list_schedules"),
        doc: "Every recurring workload in this deployment, across projects.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "update_schedule",
        method: Method::Put,
        path: "/v1/schedules/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("id", "schedule id"),
            be(
                "type",
                &["bench_run", "score_events", "score_traces", "dataset_sample", "calibrate"],
                "change the job kind it enqueues",
            ),
            b("payload", JsonTy::Object, "replace the payload"),
            b("interval_secs", JsonTy::Integer, "change how often it fires (floor 60)"),
            b("enabled", JsonTy::Boolean, "pause (false) or resume (true) it"),
        ],
        response: TypeRef::Named("Schedule"),
        cli: Some(&["schedules", "set"]),
        doc: "Patch a schedule; an omitted field is left alone, so pausing cannot rewrite a payload.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "delete_schedule",
        method: Method::Delete,
        path: "/v1/schedules/:id",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[p("id", "schedule id")],
        response: TypeRef::Untyped("{ deleted: id } — the jobs it already produced are kept."),
        cli: Some(&["schedules", "delete"]),
        doc: "Remove a schedule; the history of what it enqueued survives it.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "schedule_runs",
        method: Method::Get,
        path: "/v1/schedules/:id/runs",
        access: Admin,
        params: &[p("id", "schedule id")],
        response: TypeRef::ArrayOf("Job"),
        cli: Some(&["schedules", "runs"]),
        render_kind: Some("list_jobs"),
        doc: "The jobs one schedule has produced, matched by the `schedule_id` its sweep stamps.",
        ..Endpoint::DEFAULT
    },
];
