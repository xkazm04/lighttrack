//! The relay, the device fleet, the alert ledger and the collective network.

use super::super::model::{Column as C, Index as I, Kind::*, Table};

pub static RELAY_TASKS: Table = Table::new(
    "relay_tasks",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("source", Text).doc("originator tag (which app enqueued it)"),
        C::new("action_type", Text)
            .nn()
            .doc("resolved against the device's library"),
        C::new("payload", Json).doc("JSON params"),
        C::new("status", Text)
            .nn()
            .def("'queued'")
            .doc("queued | leased | succeeded | dead"),
        C::new("attempts", Int)
            .nn()
            .def("0")
            .doc("consumed on lease; Deferred hands one back"),
        C::new("max_attempts", Int).nn().def("4"),
        C::new("retry_interval_secs", Int)
            .nn()
            .def("18000")
            .doc("5h — one Claude subscription window"),
        C::new("idempotency_key", Text),
        C::new("device", Text).doc("which device holds/held the lease (a `devices.id`)"),
        C::new("lease_deadline", Ts).doc("expired lease => reclaimable (or dead)"),
        C::new("next_attempt_at", Ts)
            .nn()
            .doc("not leasable before this (retry backoff)"),
        C::new("result", Json),
        C::new("error", Text),
        C::new("created_at", Ts).nn(),
        C::new("updated_at", Ts).nn(),
        C::new("failures", Int).nn().def("0").added("M7").doc(
            "The fenced, renewable lease (M7): `failures` is the retry budget and `stale_reclaims` \
             counts device deaths, so a laptop that sleeps mid-run no longer burns one of the \
             task's chances. `lease_fence` is the holding device's identity, compared exactly on \
             settle/renew/progress; `progress` is its liveness.",
        ),
        C::new("stale_reclaims", Int).nn().def("0").added("M7"),
        C::new("lease_fence", Text).added("M7"),
        C::new("progress", Text).added("M7"),
    ],
)
.doc(
    "Cloud→device relay queue (docs/RELAY.md): apps enqueue action_type + JSON params; the \
     enrolled local device leases due tasks over outbound HTTPS, runs them against its local action \
     library, and settles each with succeeded | failed | deferred. Prompts/tools/credentials live \
     only on the device — the payload carries parameters, never instructions.",
)
.indexes(&[
    I::new("idx_relay_due", "status, next_attempt_at"),
    I::new("idx_relay_idem", "project_id, idempotency_key")
        .unique()
        .predicate("idempotency_key IS NOT NULL"),
    I::new("idx_relay_lease", "status, lease_deadline"),
]);

pub static DEVICES: Table = Table::new(
    "devices",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).doc("NULL = operator-wide: serves every project"),
        C::new("name", Text).nn(),
        C::new("key_prefix", Text).nn(),
        C::new("key_hash", Text)
            .nn()
            .doc("the api_keys scheme verbatim (\"<salt>:<sha256hex>\")"),
        C::new("capabilities", Json)
            .nn()
            .def("'[]'")
            .doc("JSON array of action types / \"<ns>/*\" prefixes; [] = everything"),
        C::new("last_seen_at", Ts).doc("liveness: there is no inbound path to a device"),
        C::new("agent_version", Text),
        C::new("created_at", Ts).nn(),
        C::new("revoked", Bool)
            .nn()
            .def("0")
            .doc("a flag, not a delete: past tasks still resolve"),
    ],
)
.doc(
    "Who may lease relay tasks, and what each one can actually run (M18). The relay used to have \
     exactly one anonymous device: a shared key authorized every lease and the `device` written \
     onto a task was whatever the client asserted, so identity was un-revocable and routing was \
     blind. The raw `ltd_<prefix>_<secret>` is shown once and never stored.",
)
.indexes(&[
    I::new("idx_devices_prefix", "key_prefix").unique(),
    I::new("idx_devices_project", "project_id"),
]);

pub static ALERTS: Table = Table::new(
    "alerts",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).doc("NULL = a deployment-wide condition"),
        C::new("kind", Text)
            .nn()
            .doc("AlertKind wire literal (limit_breach | score_drop | ...)"),
        C::new("dedup_key", Text)
            .nn()
            .doc("the logical identity the cooldown dedups on"),
        C::new("severity", Text).nn().doc("info | warning | critical"),
        C::new("payload", Json).doc("the same body the channel delivered"),
        C::new("fired_at", Ts).nn(),
        C::new("delivered", Json).doc("JSON array of {channel_id, ok, status, at}"),
        C::new("acked_at", Ts),
        C::new("acked_by", Text),
        C::new("resolution", Json)
            .doc("what came of it (responder diagnosis / note)"),
    ],
)
.doc(
    "Every alert this deployment has fired (M3). Before this table an alert was a `tracing::warn!` \
     and a HashMap entry: dedup reset on restart, each replica alerted independently, and nothing \
     recorded whether delivery landed. `dedup_key` + `fired_at` is the cooldown gate, decided in \
     one transaction so two replicas produce one alert.",
)
.indexes(&[
    I::new("idx_alerts_dedup", "dedup_key, fired_at")
        .doc("The dedup gate: one range seek, not a scan."),
    I::new("idx_alerts_fired", "fired_at"),
    I::new("idx_alerts_project", "project_id, fired_at"),
]);

pub static ALERT_CHANNELS: Table = Table::new(
    "alert_channels",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).doc("NULL = global (receives every project's alerts)"),
        C::new("kind", Text).nn().doc("webhook | ntfy | email"),
        C::new("target", Text)
            .nn()
            .doc("URL (webhook/ntfy) or address (email)"),
        C::new("secret_hash", Text).doc("sha256(secret): the derived HMAC signing key"),
        C::new("prev_secret_hash", Text).doc("kept live through a rotation"),
        C::new("min_severity", Text)
            .nn()
            .doc("info | warning | critical"),
        C::new("kinds", Json).doc("JSON array of AlertKind; NULL = every kind"),
        C::new("enabled", Bool).nn().def("1"),
        C::new("created_at", Ts).nn(),
    ],
)
.doc(
    "Where an alert goes. `project_id IS NULL` is a global channel — the shape the env-configured \
     destinations have always had, which is why those are synthesised at startup and never stored.",
)
.indexes(&[I::new("idx_alert_channels_project", "project_id")]);

pub static COLLECTIVE_ENTRIES: Table = Table::new(
    "collective_entries",
    &[
        C::new("contributor_id", Text)
            .nn()
            .doc("opaque, non-reversible source id (a hash)"),
        C::new("provider", Text).nn(),
        C::new("model", Text).nn(),
        C::new("task_type", Text)
            .nn()
            .doc("coarse bucket from a fixed vocabulary"),
        C::new("quality", Real).nn().doc("mean score 0..1"),
        C::new("pass_rate", Real).nn(),
        C::new("avg_cost_usd", Real).nn().doc("per case"),
        C::new("p50_latency_ms", Int),
        C::new("p95_latency_ms", Int),
        C::new("n_runs", Int).nn().def("0"),
        C::new("n_cases", Int).nn().def("0"),
        C::new("received_at", Ts).nn(),
        C::new("quality_variance", Real)
            .added("M20")
            .doc("v2: case-weighted variance of quality across the contributor's runs"),
        C::new("judge_provider", Text)
            .added("M20")
            .doc("v2: coarse judge family (anthropic|openai|google|unknown|mixed)"),
        C::new("rubric_fingerprint", Text)
            .added("M20")
            .doc("v2: short one-way hash of the rubric shape (no content leak)"),
        C::new("determinism", Text)
            .added("M20")
            .doc("v3 rigor: weakest stamp (exact|best-effort|sampled), NULL = unrecorded"),
        C::new("frozen_dataset", Text)
            .added("M20")
            .doc("v3 rigor: coverage tag (all|mixed|none), NULL = unknown"),
        C::new("significance_tested", Text)
            .added("M20")
            .doc("v3 rigor: coverage tag (all|mixed|none), NULL = unknown"),
    ],
)
.doc(
    "Collective Model Intelligence: privacy-safe, aggregate-only digest entries contributed by \
     other LightTrack instances. No raw text, no project/customer ids — only public model \
     identities plus aggregate quality/cost/latency. The primary key makes a re-contribution an \
     upsert in place rather than a second vote from the same source.",
)
.pk(&["contributor_id", "provider", "model", "task_type"])
.indexes(&[
    I::new("idx_collective_model", "provider, model, task_type"),
    I::new("idx_collective_received", "received_at").doc(
        "Retention-narrowed leaderboard reads. Timestamps are fixed-width RFC3339(Nanos,Z), so the \
         string range is a correct chronological one.",
    ),
]);

pub static COLLECTIVE_CONTRIBUTIONS: Table = Table::new(
    "collective_contributions",
    &[
        C::new("id", Text).pk(),
        C::new("hub_url_hash", Text)
            .nn()
            .doc("`h-` + 12 hex of sha256(normalized hub URL)"),
        C::new("contributor_id_as_acked", Text)
            .doc("what the HUB filed it under; may differ from ours"),
        C::new("schema_version", Int).nn(),
        C::new("generated_at", Ts)
            .nn()
            .doc("when the digest was BUILT"),
        C::new("entries_count", Int).nn(),
        C::new("projects_included", Int)
            .nn()
            .doc("the consent envelope, at rest"),
        C::new("projects_excluded", Int).nn(),
        C::new("digest_sha256", Text)
            .nn()
            .doc("the hash gate; excludes generated_at by construction"),
        C::new("ack", Json).doc("the hub's answer verbatim, or the failure"),
        C::new("status", Text).nn().doc("sent | rejected | failed"),
        C::new("created_at", Ts).nn().doc("when the push happened"),
    ],
)
.doc(
    "What THIS instance pushed to a collective hub, and what the hub said back (M22). The digest \
     BODY is deliberately absent: only the hash the gate skips an unchanged re-push by, and the \
     counts. `hub_url_hash` is hashed for the same reason the contributor id is — a ledger an \
     operator shows someone should not be where a private hub's address leaks from.",
)
.indexes(&[
    I::new("idx_contributions_created", "created_at"),
    I::new("idx_contributions_hub", "hub_url_hash, created_at"),
]);
