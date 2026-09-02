//! Tenancy, ingest and the job/schedule queues.

use super::super::model::{Column as C, Dialect, Index as I, Kind::*, Table};

pub static PROJECTS: Table = Table::new(
    "projects",
    &[
        C::new("id", Text).pk(),
        C::new("name", Text).nn(),
        C::new("enabled", Int).nn().def("1"),
        C::new("redaction", Text).nn().def("'none'").doc("none | hash | drop"),
        C::new("created_at", Ts).nn(),
        C::new("collective_opt_in", Int).nn().def("0").added("M20").doc(
            "Consent to include this project's benchmark runs in a collective-network digest. \
             Default off: contribution is an act, not an inheritance.",
        ),
        C::new("archived_at", Ts).added("M16").doc(
            "Set by DELETE /v1/projects/:id. Archive, never delete: the events and runs stay.",
        ),
        C::new("require_trusted_judge", Int).nn().def("0").added("M11").doc(
            "The per-project judge-trust policy. OFF by default: turning it on retroactively would \
             block every existing deployment's gates on the day it upgraded, nothing having been \
             calibrated yet.",
        ),
    ],
)
.doc("A tenant. Everything else in the schema hangs off a project id.");

pub static API_KEYS: Table = Table::new(
    "api_keys",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn().refs("projects(id)"),
        C::new("name", Text).nn(),
        C::new("prefix", Text).nn(),
        C::new("key_hash", Text).nn(),
        C::new("created_at", Ts).nn(),
        C::new("last_used_at", Ts),
        C::new("revoked", Int).nn().def("0"),
        C::new("scopes", Json).added("M16").doc(
            "JSON array of ingest|read|manage. NULL on rows written before scopes existed, which \
             read as the permissive back-compat default (core::decode_scopes).",
        ),
        C::new("expires_at", Ts)
            .added("M16")
            .doc("Fixed-width RFC3339. Past it, the key authenticates as nothing."),
    ],
)
.indexes(&[I::new("idx_api_keys_prefix", "prefix")]);

pub static EVENTS: Table = Table::new(
    "events",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("trace_id", Text),
        C::new("span_id", Text),
        C::new("parent_span_id", Text),
        C::new("ts", Ts).nn(),
        C::new("provider", Text).nn(),
        C::new("model", Text).nn(),
        C::new("operation", Text).nn().def("'chat'"),
        C::new("input_tokens", Int).nn().def("0"),
        C::new("output_tokens", Int).nn().def("0"),
        C::new("cached_input_tokens", Int),
        C::new("reasoning_tokens", Int),
        C::new("cost_usd", Real),
        C::new("latency_ms", Int),
        C::new("status", Text).nn().def("'success'"),
        C::new("error", Text),
        C::new("input", Json),
        C::new("output", Json),
        C::new("tags", Json).doc("JSON array"),
        C::new("source", Text),
        C::new("metadata", Json),
        C::new("name", Text)
            .added("M2")
            .doc("optional use-case / call-site label (rollup key)"),
        C::new("received_at", Ts)
            .added("M5")
            .select_as("COALESCE(received_at, ts) AS received_at")
            .doc(
                "Server-stamped arrival time (fixed-width RFC3339 UTC, like `ts`). `ts` is CLIENT \
                 event time and may be skewed or deliberately backdated; every rolling-window \
                 accounting read (limit admission, the forecast series) keys on `received_at` so \
                 one wrong clock cannot move a budget window. Backfilled to `ts` for rows written \
                 before the column existed.",
            ),
    ],
)
.doc("One observed LLM call. The ingest table; everything cost-shaped is a rollup of it.")
.indexes(&[
    I::new("idx_events_project_ts", "project_id, ts"),
    I::new("idx_events_project_received", "project_id, received_at")
        .pg_columns("project_id, COALESCE(received_at, ts)")
        .doc(
            "Windowed accounting (usage_since / usage_since_scoped / the daily forecast series) \
             filters on server arrival time, not client `ts`. Postgres indexes the *expression the \
             queries use*: a plain (project_id, received_at) index cannot serve \
             `COALESCE(received_at, ts) >= $2`, and admission runs that query on every ingested \
             event inside the per-project admission lock.",
        ),
    I::new("idx_events_trace", "trace_id"),
    I::new("idx_events_project_trace", "project_id, trace_id").doc(
        "The project-scoped trace rollup (list_trace_summaries): filter project_id + group by \
         trace_id without a full scan. Single-column idx_events_trace still serves the \
         project-agnostic per-trace fetch.",
    ),
    // Recorded drift, not a silent divergence: these four composites exist on SQLite and not on
    // Postgres today. Declaring the gap keeps the model honest without this refactor creating four
    // indexes on a live production `events` table as a side effect. Closing it is a deliberate
    // change with its own migration window.
    I::new("idx_events_project_name_ts", "project_id, name, ts").only(&[Dialect::Sqlite]),
    I::new("idx_events_project_provider_ts", "project_id, provider, ts")
        .only(&[Dialect::Sqlite])
        .doc(
            "Composites for the high-cardinality event-list predicates. Each puts the filtered \
             column ahead of `ts` so one index serves BOTH the equality seek and the `ORDER BY ts \
             DESC` keyset paging. NOT declared on Postgres today — see the model.",
        ),
    I::new("idx_events_project_model_ts", "project_id, model, ts").only(&[Dialect::Sqlite]),
    I::new("idx_events_project_status_ts", "project_id, status, ts").only(&[Dialect::Sqlite]),
])
.bq("DATE(ts)", "project_id, provider, model");

pub static LIMIT_RULES: Table = Table::new(
    "limit_rules",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("metric", Text).nn().doc("cost_usd | calls | tokens"),
        C::new("window", Text)
            .nn()
            .quoted_pg()
            .doc("hour | day | month"),
        C::new("threshold", Real).nn(),
        C::new("action", Text).nn().doc("alert | throttle | block"),
        C::new("enabled", Int).nn().def("1"),
        C::new("warn_at", Real)
            .added("pre-M1")
            .doc("optional soft-warning fraction in (0,1); NULL = no pre-warning"),
        C::new("scope_kind", Text)
            .added("pre-M1")
            .doc("provider | model | name; NULL = project-wide (unscoped)"),
        C::new("scope_value", Text)
            .added("pre-M1")
            .doc("the scoped dimension value; NULL when unscoped"),
        C::new("threshold_json", Json).added("M4").doc(
            "Measure-to-act guardrails: a threshold that is not a bare number. All five columns \
             here are nullable — an existing row reads back as exactly the fixed, human-made, \
             never-expiring rule it always was.",
        ),
        C::new("escalation_json", Json).added("M4"),
        C::new("escalated_until", Ts).added("M4"),
        C::new("origin", Text).added("M4"),
        C::new("expires_at", Ts).added("M4"),
    ],
)
.indexes(&[I::new("idx_limit_rules_origin", "origin")
    .predicate("origin IS NOT NULL")
    .only(&[Dialect::Postgres])]);

pub static JOBS: Table = Table::new(
    "jobs",
    &[
        C::new("id", Text).pk(),
        C::new("type", Text).nn(),
        C::new("payload", Json),
        C::new("status", Text).nn().def("'queued'"),
        C::new("attempts", Int)
            .nn()
            .def("0")
            .doc("claims, including ones a crash ended"),
        C::new("max_attempts", Int).nn().def("3"),
        C::new("progress", Text),
        C::new("error", Text),
        C::new("result", Json),
        C::new("claimed_at", Ts),
        C::new("created_at", Ts).nn(),
        C::new("updated_at", Ts).nn(),
        C::new("failures", Int)
            .nn()
            .def("0")
            .added("M7")
            .doc("runs that actually failed — the retry budget, kept apart from `attempts`"),
        C::new("stale_reclaims", Int)
            .nn()
            .def("0")
            .added("M7")
            .doc("worker deaths (claim held past the stale window)"),
        C::new("project_id", Text).added("M17").doc(
            "The job queue's missing tenant: without it a project key reading GET /v1/jobs saw \
             every project's payloads. Nullable — NULL is an operator/legacy job, which \
             `Scope::Operator` sees and no project scope does.",
        ),
    ],
)
.doc("Background job queue: enqueue returns immediately; lt-runner serve executes.")
.indexes(&[
    I::new("idx_jobs_status", "status, created_at"),
    I::new("idx_jobs_project_created", "project_id, created_at DESC"),
]);

pub static SCHEDULES: Table = Table::new(
    "schedules",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("kind", Text)
            .nn()
            .doc("JobKind wire literal (bench_run | score_events | ...)"),
        C::new("payload", Json).doc("enqueued verbatim as the job's payload"),
        C::new("interval_secs", Int).nn(),
        C::new("next_due", Ts).nn(),
        C::new("last_job_id", Text),
        C::new("enabled", Bool).nn().def("1"),
        C::new("created_at", Ts).nn(),
    ],
)
.doc(
    "Recurring workloads as rows (M7). Before this, recurrence lived either in a benchmark's \
     target or in a daemon's --interval flag, so nothing could enumerate it.",
)
.indexes(&[
    I::new("idx_schedules_due", "enabled, next_due"),
    I::new("idx_schedules_project", "project_id"),
]);
