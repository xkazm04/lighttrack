//! What a store backend actually implements, declared rather than discovered.
//!
//! The `Store` trait carries ~45 methods that default to [`StoreError::Unsupported`], and two
//! backends in production inherit about half of them each. Before this module the only way to know
//! which half was to read three `impl Store` blocks — so a surface could silently be missing in
//! production (`PUT /v1/projects/:id` answered 501 on Postgres and nobody had decided that).
//!
//! A [`Capabilities`] manifest makes the answer data: each backend names the [`Surface`]s it
//! serves, the conformance driver runs the full semantics for a declared surface and asserts an
//! explicit refusal for an undeclared one, `docs/PARITY.md` is generated from the three manifests,
//! and the API publishes it at `GET /v1/capabilities`. The failure mode we refuse is a gap that
//! reads as "no data"; a declared refusal is a documented limitation.
//!
//! [`StoreError::Unsupported`]: crate::StoreError::Unsupported

mod render;

use std::collections::BTreeSet;

use serde::Serialize;

pub use render::{parity_doc, GENERATED_BY};

/// A coherent group of `Store` methods a backend either serves or refuses **as a whole**.
///
/// Granularity is deliberate: a surface is the smallest unit an operator cares about (a route, a
/// feature), because a half-ported surface is exactly the state that produces authoritative-looking
/// empty pages. Every trait method belongs to exactly one — enforced by the test below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    /// Ingest, admission, projects, keys, scores, prices, datasets, rubrics, benchmarks, jobs,
    /// revenue: the floor every backend must clear to be a backend at all.
    EventsCore,
    /// The extended event predicates + keyset paging + scoped/grouped usage rollups.
    EventFilters,
    /// The one grouped-rollup primitive (`Store::rollup`) every cost/usage/margin/forecast surface
    /// reads through. A backend that serves it serves the nine legacy grouped methods too, via
    /// their default impls — which is why it is its own surface rather than a member of another.
    Rollup,
    /// What the ingest boundary did to the stored rows, grouped by stamp (M9).
    RedactionPosture,
    /// Re-converting stored revenue at a corrected FX rate (M9).
    RevenueReprice,
    /// Narrowing verdicts by their typed identity: rubric id and score kind (M9).
    ScoreFilters,
    /// Events rolled up by `trace_id`: listing, detail, whole-trace scores.
    Traces,
    /// Daily (UTC) usage/cost series — the input `GET /v1/forecast` fits a trend to.
    Forecast,
    /// Token and per-customer cost breakdowns behind the margin what-if surfaces.
    MarginBreakdowns,
    /// The versioned prompt registry.
    Prompts,
    /// The cloud→device task queue (`docs/RELAY.md`).
    Relay,
    /// The opt-in shared model leaderboard's entry table.
    Collective,
    /// Mutating a project in place (name / enabled / redaction / collective opt-in).
    ProjectAdmin,
    /// Listing and revoking a project's API keys.
    KeyAdmin,
    /// Reading, updating and deleting a limit rule after creation.
    LimitLifecycle,
    /// Standing margin guardrails: the policies the forecast sweep turns into limit rules. Its own
    /// surface rather than part of `LimitLifecycle` because it is a separate table with a separate
    /// admin route set — a backend can serve every limit-rule method and still have no
    /// `margin_policies` table, and an operator needs to be told that rather than shown an empty
    /// list.
    MarginPolicies,
    /// Job cancellation and lease renewal — the liveness half of the job queue.
    JobLeases,
    /// Stored schedules: recurrence as a row, and the sweep's due-list read.
    ///
    /// Its own surface rather than part of `EventsCore`, because it is genuinely optional: a
    /// deployment can run every recurring workload from an external scheduler (cron, Cloud
    /// Scheduler) hitting `POST /v1/jobs`, and a backend that cannot host the table must be able to
    /// say so instead of answering an empty due-list that reads as "nothing is scheduled here".
    Schedules,
    /// The persisted alert ledger: what fired, where it went, who acknowledged it, what came of it.
    ///
    /// Its own surface because it is the product's own audit trail rather than a feature of ingest:
    /// a backend that cannot host it must say so, because an empty `GET /v1/alerts` reads as
    /// "nothing has fired here" — the most reassuring possible lie about a monitoring system.
    Alerts,
    /// Per-project alert routing: the channel table `channels_for` unions with the global ones.
    ///
    /// Separate from [`Surface::Alerts`] because the split is real: a deployment can keep the
    /// env-configured global channels (which are synthesised, never stored) and still record every
    /// alert, so a backend may serve the ledger and refuse the routing table.
    AlertRouting,
    /// Disk accounting and the quiet-window maintenance pass.
    Maintenance,
    /// The store's own per-family latency profile.
    Metrics,
    /// The unpriced-traffic ledger, the forward fill that closes it, and the dated price book's
    /// history (M26).
    ///
    /// Its own surface rather than part of `EventsCore`, whose `upsert_price`/`list_prices` are the
    /// price *book*: this is the loop around it — see what is unpriced, add the rate, price the
    /// history. A backend can serve the book perfectly and still be unable to rewrite stored event
    /// rows, and an operator told "0 filled" by a backend that never looked would draw exactly the
    /// wrong conclusion.
    Pricing,
    /// The enrolled relay device fleet (M18): hashed per-device keys, advertised capabilities,
    /// liveness, and the eligibility count the enqueue door admits against.
    ///
    /// Its own surface rather than part of [`Surface::Relay`], because a backend can host the task
    /// queue and have no `devices` table — and "nobody is enrolled" is a *load-bearing* answer
    /// there (it is what admits a legacy shared-key deployment's traffic), so it must never be
    /// something a missing table says by accident.
    Devices,
    /// The contributor-side contribution ledger (M22): what this instance pushed to which hub, and
    /// what the hub acked.
    ///
    /// Its own surface rather than part of [`Surface::Collective`] because the two sit at opposite
    /// ends of the same wire: `Collective` is what a **hub** stores about others, this is what an
    /// **instance** stores about itself, and a deployment is routinely one and not the other. The
    /// answer that must never be accidental is the empty one — an empty ledger reads as "we have
    /// never contributed", which sends a hash-gated push every interval and makes a
    /// `withdraw --all` silently cover nothing.
    Contributions,
    /// Judge verdicts summarized per value of one event [`Dimension`] — the served-version quality
    /// ledger behind `GET /v1/quality/prompts` and the prompt canary (M23).
    ///
    /// Its own surface rather than a member of [`Surface::ScoreFilters`]: that one narrows verdicts
    /// by their own typed identity, while this joins them to `events` and groups on a value inside
    /// the event's `metadata`. A backend can serve every score filter and be unable to express that
    /// join, and an operator shown an empty quality table would conclude the version is unjudged
    /// rather than unmeasurable here.
    ///
    /// [`Dimension`]: lighttrack_core::Dimension
    ScoreSummaries,
    /// The human verdict ledger (M11): what a person said about an event, a golden-set item, or a
    /// judge's own verdict.
    ///
    /// Its own surface rather than part of [`Surface::EventsCore`] because it is the *input* to the
    /// trust argument rather than a product record, and a backend can serve every score and dataset
    /// method without having a `labels` table. An empty listing here would read as "nobody has
    /// graded anything", which is what lets a calibration run on nothing at all.
    Labels,
    /// The stored calibration results, and the `(rubric, judge)` trust lookup every gate makes
    /// (M11).
    ///
    /// Separate from [`Surface::Labels`] because the split is real: a deployment can import its
    /// labels from files forever and still want the kappa history stored, and — more importantly —
    /// a missing calibration is a *load-bearing* answer (it is what makes trust `unknown` rather
    /// than `untrusted`), so it must never be something a missing table says by accident.
    Calibrations,
    /// Versioned eval corpora (M24): forking a frozen dataset into its next version, mining stored
    /// rows into one by a declared sampling strategy, and reading a name's version history.
    ///
    /// Its own surface rather than part of [`Surface::EventsCore`], whose dataset methods are the
    /// flat CRUD — create, freeze, list items. This is the *lineage*, and a backend can serve every
    /// one of those and have no way to express a stratified quota or a fork. The answer that must
    /// never be accidental is the version: a backend that quietly declined to fork would leave
    /// `version` pinned at 1 forever, which is precisely the state M24 exists to end — and a
    /// paired-test guard comparing 1 with 1 reports "comparable" about two different corpora.
    DatasetLineage,
}

impl Surface {
    /// Every surface, in declaration order — the row order of the generated parity matrix.
    pub const ALL: &'static [Surface] = &[
        Surface::EventsCore,
        Surface::EventFilters,
        Surface::Rollup,
        Surface::RedactionPosture,
        Surface::RevenueReprice,
        Surface::ScoreFilters,
        Surface::Traces,
        Surface::Forecast,
        Surface::MarginBreakdowns,
        Surface::Prompts,
        Surface::Relay,
        Surface::Collective,
        Surface::ProjectAdmin,
        Surface::KeyAdmin,
        Surface::LimitLifecycle,
        Surface::MarginPolicies,
        Surface::JobLeases,
        Surface::Schedules,
        Surface::Alerts,
        Surface::AlertRouting,
        Surface::Maintenance,
        Surface::Metrics,
        Surface::Pricing,
        Surface::Devices,
        Surface::Contributions,
        Surface::ScoreSummaries,
        Surface::Labels,
        Surface::Calibrations,
        Surface::DatasetLineage,
    ];

    /// Stable wire/doc name (`snake_case`, matching the `Serialize` impl).
    pub fn as_str(self) -> &'static str {
        match self {
            Surface::EventsCore => "events_core",
            Surface::EventFilters => "event_filters",
            Surface::Rollup => "rollup",
            Surface::RedactionPosture => "redaction_posture",
            Surface::RevenueReprice => "revenue_reprice",
            Surface::ScoreFilters => "score_filters",
            Surface::Traces => "traces",
            Surface::Forecast => "forecast",
            Surface::MarginBreakdowns => "margin_breakdowns",
            Surface::Prompts => "prompts",
            Surface::Relay => "relay",
            Surface::Collective => "collective",
            Surface::ProjectAdmin => "project_admin",
            Surface::KeyAdmin => "key_admin",
            Surface::LimitLifecycle => "limit_lifecycle",
            Surface::MarginPolicies => "margin_policies",
            Surface::JobLeases => "job_leases",
            Surface::Schedules => "schedules",
            Surface::Alerts => "alerts",
            Surface::AlertRouting => "alert_routing",
            Surface::Maintenance => "maintenance",
            Surface::Metrics => "metrics",
            Surface::Pricing => "pricing",
            Surface::Devices => "devices",
            Surface::Contributions => "contributions",
            Surface::ScoreSummaries => "score_summaries",
            Surface::Labels => "labels",
            Surface::Calibrations => "calibrations",
            Surface::DatasetLineage => "dataset_lineage",
        }
    }

    /// The trait methods this surface owns.
    pub fn methods(self) -> &'static [&'static str] {
        SURFACE_METHODS
            .iter()
            .find(|(s, _)| *s == self)
            .map(|(_, m)| *m)
            .unwrap_or(&[])
    }
}

/// One backend's declaration: who it is, what it serves, and whether its admission control is a
/// single critical section (i.e. whether a configured cap is genuinely enforced under concurrency).
#[derive(Debug, Clone, Serialize)]
pub struct Capabilities {
    pub backend: &'static str,
    pub surfaces: BTreeSet<Surface>,
    pub atomic_admission: bool,
    /// A short hash of the logical schema this build carries (M14, `crate::schema::fingerprint`).
    /// The same on every backend, because the *model* is: it answers "are these two deployments
    /// running the same schema", which `backend` cannot and which nothing else here could be asked.
    pub schema_fingerprint: String,
}

impl Capabilities {
    pub fn new(backend: &'static str, surfaces: &[Surface], atomic_admission: bool) -> Self {
        Self {
            backend,
            surfaces: surfaces.iter().copied().collect(),
            atomic_admission,
            schema_fingerprint: crate::schema::fingerprint(),
        }
    }

    pub fn has(&self, s: Surface) -> bool {
        self.surfaces.contains(&s)
    }

    /// The surfaces this backend refuses — what an operator is actually missing.
    pub fn missing(&self) -> Vec<Surface> {
        Surface::ALL
            .iter()
            .copied()
            .filter(|s| !self.has(*s))
            .collect()
    }
}

/// Every method declared on `trait Store`, assigned to exactly one [`Surface`].
///
/// This is the join that makes the manifest checkable: the unit test below re-parses `lib.rs` and
/// fails if a method was added to the trait without being claimed here, so a new capability cannot
/// slip in unsurfaced.
pub const SURFACE_METHODS: &[(Surface, &[&str])] = &[
    (
        Surface::EventsCore,
        &[
            "capabilities",
            "init_schema",
            "ping",
            "insert_event",
            "insert_event_checked",
            "insert_events_checked",
            "admission_is_atomic",
            "list_events",
            "cost_summary",
            "usage_since",
            "create_project",
            "get_project",
            "list_projects",
            "create_api_key",
            "find_api_key_by_prefix",
            "touch_api_key",
            "create_limit_rule",
            "list_limit_rules",
            "get_event",
            "insert_score",
            "list_scores",
            "list_run_scores",
            "scored_event_ids",
            "list_unscored_events",
            "create_benchmark",
            "get_benchmark",
            "list_benchmarks",
            "create_benchmark_run",
            "list_benchmark_runs",
            "upsert_price",
            "list_prices",
            "create_dataset",
            "get_dataset",
            "list_datasets",
            "set_dataset_frozen",
            "create_dataset_item",
            "list_dataset_items",
            "create_rubric",
            "get_rubric",
            "list_rubrics",
            "create_job",
            "claim_job",
            "update_job_progress",
            "finish_job",
            "get_job",
            "list_jobs",
            "insert_revenue_event",
            "insert_revenue_events",
            "list_revenue_events",
            "cost_by_dimension",
        ],
    ),
    (
        Surface::EventFilters,
        &[
            "list_events_filtered",
            "cost_summary_windowed",
            "usecase_costs",
            "usage_since_scoped",
            "usage_by_scope",
        ],
    ),
    (Surface::Rollup, &["rollup"]),
    (Surface::RedactionPosture, &["redaction_posture"]),
    (Surface::RevenueReprice, &["reprice_revenue"]),
    (Surface::ScoreFilters, &["list_scores_filtered"]),
    (
        Surface::Traces,
        &[
            "serves_traces",
            "list_traces",
            "list_traces_filtered",
            "list_trace_events",
            "list_trace_scores",
            "get_trace",
        ],
    ),
    (
        Surface::Forecast,
        &["daily_usage", "daily_cost_by_dimension"],
    ),
    (
        Surface::MarginBreakdowns,
        &[
            "tokens_by_dimension",
            "customer_cost_by_model",
            "customer_cost_by_name",
        ],
    ),
    (
        Surface::Prompts,
        &[
            "create_prompt",
            "update_prompt",
            "get_prompt",
            "get_prompt_by_id",
            "list_prompts",
            "create_prompt_version",
            "get_prompt_version",
            "list_prompt_versions",
        ],
    ),
    (
        Surface::Relay,
        &[
            "create_relay_task",
            "get_relay_task",
            "find_relay_task_by_key",
            "list_relay_tasks",
            "list_relay_tasks_by_action",
            "lease_relay_tasks",
            "sweep_relay_dead",
            "settle_relay_task",
            "renew_relay_lease",
            "update_relay_progress",
            "cancel_relay_task",
        ],
    ),
    (
        Surface::Collective,
        &[
            "upsert_collective_entry",
            "delete_collective_entries",
            "list_collective_entries",
            "purge_collective_entries_before",
            "replace_collective_contribution",
            "latest_collective_receipt",
            "list_collective_entries_filtered",
        ],
    ),
    (Surface::ProjectAdmin, &["update_project"]),
    (
        Surface::KeyAdmin,
        &["list_api_keys", "set_api_key_revoked", "set_api_key_expiry"],
    ),
    (
        Surface::LimitLifecycle,
        &["get_limit_rule", "update_limit_rule", "delete_limit_rule"],
    ),
    (
        Surface::MarginPolicies,
        &[
            "create_margin_policy",
            "list_margin_policies",
            "get_margin_policy",
            "delete_margin_policy",
        ],
    ),
    (Surface::JobLeases, &["cancel_job", "renew_job_lease"]),
    (
        Surface::Schedules,
        &[
            "create_schedule",
            "get_schedule",
            "list_schedules",
            "update_schedule",
            "delete_schedule",
            "due_schedules",
        ],
    ),
    (
        Surface::Alerts,
        &[
            "insert_alert_dedup",
            "mark_delivery",
            "list_alerts",
            "get_alert",
            "ack_alert",
            "attach_alert_resolution",
        ],
    ),
    (
        Surface::AlertRouting,
        &[
            "create_alert_channel",
            "get_alert_channel",
            "list_alert_channels",
            "delete_alert_channel",
            "channels_for",
        ],
    ),
    (
        Surface::Maintenance,
        &["storage_report", "maintenance_pass"],
    ),
    (Surface::Metrics, &["db_metrics"]),
    (
        Surface::Pricing,
        &["list_unpriced", "fill_unpriced_cost", "list_price_history"],
    ),
    (
        Surface::Devices,
        &[
            "create_device",
            "get_device",
            "list_devices",
            "find_device_by_key_prefix",
            "touch_device",
            "revoke_device",
            "count_eligible_devices",
        ],
    ),
    (
        Surface::Contributions,
        &[
            "insert_contribution",
            "list_contributions",
            "latest_contribution",
        ],
    ),
    (Surface::ScoreSummaries, &["score_summary_by_dimension"]),
    (
        Surface::Labels,
        &["insert_label", "list_labels", "labels_for_dataset"],
    ),
    (
        Surface::Calibrations,
        &[
            "insert_calibration",
            "latest_calibration",
            "list_calibrations",
        ],
    ),
    (
        Surface::DatasetLineage,
        &[
            "fork_dataset",
            "import_dataset_items",
            "list_dataset_versions",
        ],
    ),
];

/// Method names declared inside `pub trait Store` in `lib.rs`, in source order.
///
/// Parsed rather than reflected: Rust has no way to enumerate a trait's methods at runtime, and a
/// hand-kept list is exactly the thing that goes stale. A trait method is the only thing in that
/// block indented four spaces and starting with `fn `.
#[cfg(test)]
fn declared_trait_methods(src: &str) -> Vec<&str> {
    src.lines()
        .skip_while(|l| !l.starts_with("pub trait Store"))
        .filter_map(|l| l.strip_prefix("    fn "))
        .filter_map(|rest| rest.split_once('('))
        .map(|(name, _)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// The manifest's whole value rests on being complete: a trait method claimed by no surface is
    /// a capability no backend has to declare, no conformance run asserts, and no parity doc shows.
    /// Adding one to `lib.rs` must therefore fail here until it is filed under a surface.
    #[test]
    fn every_trait_method_maps_to_exactly_one_surface() {
        let declared = declared_trait_methods(include_str!("../lib.rs"));
        assert!(
            declared.len() > 80,
            "parser found only {} trait methods — the `    fn ` shape it keys on must have \
             changed; fix the parser rather than the assertion",
            declared.len()
        );

        let mut owner: BTreeMap<&str, Vec<Surface>> = BTreeMap::new();
        for (surface, methods) in SURFACE_METHODS {
            for m in *methods {
                owner.entry(m).or_default().push(*surface);
            }
        }

        let unmapped: Vec<&str> = declared
            .iter()
            .copied()
            .filter(|m| !owner.contains_key(m))
            .collect();
        assert!(
            unmapped.is_empty(),
            "trait methods with no surface in SURFACE_METHODS: {unmapped:?}"
        );

        let duplicated: Vec<&&str> = owner.keys().filter(|m| owner[*m].len() > 1).collect();
        assert!(
            duplicated.is_empty(),
            "methods claimed by more than one surface: {duplicated:?}"
        );

        let phantom: Vec<&&str> = owner.keys().filter(|m| !declared.contains(m)).collect();
        assert!(
            phantom.is_empty(),
            "SURFACE_METHODS names methods the trait no longer declares: {phantom:?}"
        );
    }

    /// `Surface::ALL` is what the conformance driver iterates and what the parity doc renders as
    /// rows; a variant missing from it would simply never be checked or shown.
    #[test]
    fn all_lists_every_surface_once() {
        let unique: BTreeSet<Surface> = Surface::ALL.iter().copied().collect();
        assert_eq!(
            unique.len(),
            Surface::ALL.len(),
            "duplicate in Surface::ALL"
        );
        assert_eq!(
            unique.len(),
            SURFACE_METHODS.len(),
            "every surface owns a SURFACE_METHODS entry and vice versa"
        );
        for (s, _) in SURFACE_METHODS {
            assert!(unique.contains(s), "{s:?} missing from Surface::ALL");
        }
    }

    #[test]
    fn missing_is_the_complement_of_declared() {
        let caps = Capabilities::new("test", &[Surface::EventsCore, Surface::Traces], true);
        assert!(caps.has(Surface::Traces));
        assert!(!caps.has(Surface::Relay));
        assert_eq!(caps.missing().len(), Surface::ALL.len() - 2);
        assert!(caps.missing().contains(&Surface::Relay));
    }
}
