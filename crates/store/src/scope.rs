//! Tenant scope as a value, so every `Store` read carries who is asking.
//!
//! D13 (see `docs/DECISIONS.md`) fixed cross-tenant trace reads by putting the project filter *in
//! the query*: a foreign id is simply not found, so a 404 never confirms that someone else's row
//! exists. [`Scope`] generalises that to the whole trait — a point read that takes a bare id and is
//! authorised afterwards is an existence oracle no matter how careful the handler is, and the
//! handler is where the compensating `forbidden(...)` used to live.
//!
//! Two values only: a project key sees exactly its own rows ([`Scope::Project`]); an operator
//! (admin/dev key, background sweeps, the runner) sees everything ([`Scope::Operator`]). Rows whose
//! `project_id` is `NULL` are operator/legacy rows: `Operator` sees them, a project scope does not.

/// Who is asking. Constructed by the API from the request principal, and by background sweeps as
/// [`Scope::Operator`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope<'a> {
    /// A single tenant. Reads see only rows whose `project_id` equals this.
    Project(&'a str),
    /// The operator: every project, plus the `NULL`-project rows no tenant owns.
    Operator,
}

impl<'a> Scope<'a> {
    /// The project this scope is confined to, if any. `None` for [`Scope::Operator`].
    pub fn project(&self) -> Option<&'a str> {
        match self {
            Scope::Project(p) => Some(p),
            Scope::Operator => None,
        }
    }

    /// Whether a row carrying `project_id` is visible in this scope. `None` means the row has no
    /// project (operator/legacy) — only [`Scope::Operator`] sees those. Backends that cannot push
    /// the predicate into the query (a Firestore point read by document id) filter with this.
    pub fn allows(&self, project_id: Option<&str>) -> bool {
        match self {
            Scope::Operator => true,
            Scope::Project(p) => project_id == Some(*p),
        }
    }

    /// A **sargable** scope predicate, mirroring `sqlite::project_pred`: a concrete project is an
    /// index-seekable equality, while the operator arm is a constant TRUE that still consumes the
    /// same placeholder (bound to `NULL`), so callers bind exactly one parameter in both arms.
    ///
    /// `placeholder` is the backend's own form for the slot — `"?3"` for SQLite, `"$3"` for
    /// Postgres. Returns the predicate text and the value to bind into that slot.
    pub fn sql_pred(&self, col: &str, placeholder: &str) -> (String, Option<&'a str>) {
        match self {
            Scope::Project(p) => (format!("{col} = {placeholder}"), Some(*p)),
            Scope::Operator => (format!("{placeholder} IS NULL"), None),
        }
    }
}

/// The migration window: `None` meant "all projects" everywhere before this existed.
impl<'a> From<Option<&'a str>> for Scope<'a> {
    fn from(p: Option<&'a str>) -> Self {
        match p {
            Some(p) => Scope::Project(p),
            None => Scope::Operator,
        }
    }
}

impl<'a> From<&'a str> for Scope<'a> {
    fn from(p: &'a str) -> Self {
        Scope::Project(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operator_sees_null_project_rows_and_a_tenant_does_not() {
        assert!(Scope::Operator.allows(None));
        assert!(Scope::Operator.allows(Some("p1")));
        assert!(!Scope::Project("p1").allows(None));
        assert!(Scope::Project("p1").allows(Some("p1")));
        assert!(!Scope::Project("p1").allows(Some("p2")));
    }

    #[test]
    fn predicate_is_an_equality_for_a_tenant_and_a_bound_null_for_the_operator() {
        let (sql, bind) = Scope::Project("p1").sql_pred("project_id", "?2");
        assert_eq!(sql, "project_id = ?2");
        assert_eq!(bind, Some("p1"));
        let (sql, bind) = Scope::Operator.sql_pred("project_id", "$2");
        assert_eq!(sql, "$2 IS NULL");
        assert_eq!(bind, None);
    }

    #[test]
    fn option_round_trips_through_the_migration_conversion() {
        assert_eq!(Scope::from(Some("p1")), Scope::Project("p1"));
        assert_eq!(Scope::from(None), Scope::Operator);
    }
}

/// The trait-signature property M17 exists to keep: **no `Store` read on a project-bearing surface
/// can be called without saying who is asking.**
///
/// Parsed rather than reflected, for the same reason the capability manifest is
/// ([`crate::capabilities`]): Rust cannot enumerate a trait's methods at runtime, and a hand-kept
/// list of "which reads are scoped" is exactly the thing that goes stale — silently, and in the
/// direction that reopens a cross-tenant hole.
#[cfg(test)]
mod trait_signature {
    use crate::capabilities::Surface;

    /// Every trait method's full signature text, keyed by name.
    fn signatures(src: &str) -> Vec<(&str, String)> {
        let mut out = Vec::new();
        let mut lines = src
            .lines()
            .skip_while(|l| !l.starts_with("pub trait Store"))
            .peekable();
        while let Some(l) = lines.next() {
            let Some(rest) = l.strip_prefix("    fn ") else {
                continue;
            };
            let Some((name, _)) = rest.split_once('(') else {
                continue;
            };
            let mut sig = l.to_string();
            // A multi-line signature runs until the line that opens the body or ends the decl.
            while !sig.trim_end().ends_with('{') && !sig.trim_end().ends_with(';') {
                match lines.next() {
                    Some(next) => sig.push_str(next),
                    None => break,
                }
            }
            out.push((name, sig));
        }
        out
    }

    /// Surfaces whose rows carry a `project_id`. The rest are operator-global by design:
    /// `Collective` is the hub's own aggregate, `Pricing`/prices are the operator's rate book,
    /// `Contributions` is what *this instance* sent, and `Maintenance`/`Metrics` are about the
    /// database rather than about anyone's data.
    const TENANT_SURFACES: &[Surface] = &[
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
        Surface::ProjectAdmin,
        Surface::KeyAdmin,
        Surface::LimitLifecycle,
        Surface::MarginPolicies,
        Surface::JobLeases,
        Surface::Schedules,
        Surface::Alerts,
        Surface::AlertRouting,
        Surface::Devices,
        Surface::ScoreSummaries,
        Surface::Labels,
        Surface::Calibrations,
    ];

    /// Methods on a tenant surface that legitimately take no scope, each for a stated reason.
    /// Adding a name here is a decision about tenancy, which is why it needs one.
    const NO_SCOPE_NEEDED: &[(&str, &str)] = &[
        // Writes whose argument already carries the row's `project_id`; the scope would be a
        // second, disagreeable source of truth for the same fact.
        ("insert_event", "the event carries its project"),
        ("insert_event_checked", "the event carries its project"),
        ("insert_events_checked", "the events carry their project"),
        ("insert_score", "the score carries its project"),
        ("create_project", "it IS the project"),
        ("update_project", "the project carries its own id"),
        ("create_api_key", "the key carries its project"),
        ("create_limit_rule", "the rule carries its project"),
        ("create_margin_policy", "the policy carries its project"),
        ("create_benchmark", "the benchmark carries its project"),
        (
            "create_benchmark_run",
            "the run's project is its benchmark's",
        ),
        ("create_dataset", "the dataset carries its project"),
        ("create_dataset_item", "the item's project is its dataset's"),
        ("create_rubric", "the rubric carries its project"),
        (
            "create_job",
            "the job carries its project (nullable = operator)",
        ),
        ("create_schedule", "the schedule carries its project"),
        ("create_prompt", "the prompt carries its project"),
        ("update_prompt", "the prompt carries its project"),
        (
            "create_prompt_version",
            "the version's project is its prompt's",
        ),
        ("insert_revenue_event", "the event carries its project"),
        ("insert_revenue_events", "the events carry their project"),
        ("create_relay_task", "the task carries its project"),
        (
            "create_device",
            "the device carries its project (nullable = shared)",
        ),
        ("insert_alert_dedup", "the alert carries its project"),
        ("create_alert_channel", "the channel carries its project"),
        ("insert_label", "the label carries its project"),
        ("insert_calibration", "the record carries its project"),
        // Filter/param structs that carry the project as a FIELD rather than a parameter.
        ("rollup", "RollupQuery::project"),
        ("list_alerts", "AlertFilter::project"),
        ("list_labels", "LabelFilter::project"),
        ("list_contributions", "ContributionFilter"),
        ("fill_unpriced_cost", "PriceFill carries the scope it fills"),
        // Reads of a row the caller already proved it holds, by a secret or a fence rather than by
        // a project: scoping them would break the very lookup that establishes the identity.
        (
            "find_api_key_by_prefix",
            "the presented secret IS the scope",
        ),
        ("touch_api_key", "reached only via find_api_key_by_prefix"),
        (
            "set_api_key_revoked",
            "admin lifecycle on an id already authorized",
        ),
        (
            "set_api_key_expiry",
            "admin lifecycle on an id already authorized",
        ),
        ("find_relay_task_by_key", "takes a required project already"),
        (
            "find_device_by_key_prefix",
            "the presented device key IS the scope",
        ),
        ("touch_device", "reached only via find_device_by_key_prefix"),
        // Worker-side queue mechanics, held by a fence rather than by a tenant: a worker claims
        // across projects by design, and every write it then makes is checked against that fence.
        (
            "claim_job",
            "the worker is the operator; the fence is the authority",
        ),
        ("update_job_progress", "held by the claim"),
        ("renew_job_lease", "held by the fence"),
        ("finish_job", "held by the fence"),
        ("due_schedules", "the sweep is the operator"),
        ("lease_relay_tasks", "the device is the authority"),
        ("sweep_relay_dead", "the sweep is the operator"),
        ("settle_relay_task", "held by the lease fence"),
        ("renew_relay_lease", "held by the lease fence"),
        ("update_relay_progress", "held by the lease fence"),
        (
            "count_eligible_devices",
            "a fleet-wide routability question",
        ),
        (
            "mark_delivery",
            "the delivery attaches to an alert already read in scope",
        ),
        (
            "channels_for",
            "takes a Scope; the default composes list_alert_channels",
        ),
        // The price book is the operator'+chr(39)+'s, shared by every project: see the dedicated test below.
        ("list_prices", "one rate card for the instance"),
        ("upsert_price", "one rate card for the instance"),
        // Whole-instance facts, not anyone'+chr(39)+'s rows.
        ("capabilities", "about the backend"),
        ("init_schema", "about the database"),
        ("admission_is_atomic", "about the backend"),
        ("serves_traces", "about the backend"),
        ("get_project", "a project id IS the scope"),
        (
            "list_projects",
            "operator-only listing of the tenants themselves",
        ),
        ("list_api_keys", "takes a required project already"),
        ("list_limit_rules", "takes a required project already"),
        ("list_margin_policies", "takes a required project already"),
        ("list_benchmarks", "takes a required project already"),
        ("list_rubrics", "takes a required project already"),
        ("list_schedules", "takes a required project already"),
        ("get_prompt", "takes a required project already"),
        ("list_prompts", "takes a required project already"),
        ("usage_since", "takes a required project already"),
        ("usage_since_scoped", "takes a required project already"),
        ("usage_by_scope", "takes a required project already"),
        ("daily_usage", "takes a required project already"),
        (
            "labels_for_dataset_version",
            "takes a required project already",
        ),
        ("latest_calibration", "takes a required project already"),
    ];

    /// The migration is complete: `Option<&str>` no longer stands in for "which tenant".
    ///
    /// It is the shape that made every one of these reads ambiguous — `None` meant "all projects"
    /// on some methods, "the project-less ones" on others, and nothing at all on the point reads
    /// that did not take it. `Scope` is two named values instead.
    #[test]
    fn no_store_method_still_takes_an_untyped_project_option() {
        let offenders: Vec<&str> = signatures(include_str!("lib.rs"))
            .into_iter()
            .filter(|(_, sig)| sig.contains("project: Option<&str>"))
            .map(|(name, _)| name)
            .collect();
        assert!(
            offenders.is_empty(),
            "these Store methods still take a bare `project: Option<&str>` instead of a `Scope`: \
             {offenders:?}"
        );
    }

    /// Every read on a project-bearing surface says who is asking — by a `Scope`, by a required
    /// `project`, or by an entry in [`NO_SCOPE_NEEDED`] that states why it does not have to.
    #[test]
    fn every_tenant_surface_method_carries_a_scope() {
        let sigs = signatures(include_str!("lib.rs"));
        assert!(
            sigs.len() > 80,
            "the signature parser found only {}",
            sigs.len()
        );

        let mut unscoped: Vec<&str> = Vec::new();
        for surface in TENANT_SURFACES {
            for m in surface.methods() {
                let Some((_, sig)) = sigs.iter().find(|(n, _)| n == m) else {
                    continue; // the manifest test owns "method exists"
                };
                let scoped = sig.contains("Scope<'_>") || sig.contains("project: &str");
                if !scoped && !NO_SCOPE_NEEDED.iter().any(|(n, _)| n == m) {
                    unscoped.push(m);
                }
            }
        }
        unscoped.sort_unstable();
        unscoped.dedup();
        assert!(
            unscoped.is_empty(),
            "these methods read project-bearing rows without a tenant scope in the signature: \
             {unscoped:?}. Add a `Scope` parameter, or file the method in NO_SCOPE_NEEDED with the \
             reason it does not need one."
        );
    }

    /// The one exemption that is a design decision rather than a mechanical fact: the price book is
    /// the operator's, shared by every project, so scoping it would fragment one rate card into N.
    #[test]
    fn the_price_book_stays_operator_global() {
        let sigs = signatures(include_str!("lib.rs"));
        for m in ["list_prices", "upsert_price", "list_price_history"] {
            let (_, sig) = sigs.iter().find(|(n, _)| *n == m).expect(m);
            assert!(
                !sig.contains("Scope<'_>"),
                "{m} must stay operator-global: prices are one book for the instance, not one per \
                 tenant (see docs/DECISIONS.md D13 addendum)"
            );
        }
    }
}
