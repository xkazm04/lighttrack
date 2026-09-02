//! The other half of the contract: what an **undeclared** surface must do.
//!
//! A backend that has not ported a surface has to refuse every one of its methods with
//! [`StoreError::Unsupported`] (which the API renders as HTTP 501). The failure this exists to
//! prevent is the quiet one — an empty `Vec`, a `None`, an `Ok(())` that dropped the write — because
//! an empty page reads as *authoritative zero* to whoever is looking at it, and a dropped write is
//! discovered months later.
//!
//! Coverage is asserted, not assumed: each arm returns the method names it exercised and the driver
//! checks them against [`Surface::methods`], so adding a method to a surface without giving it a
//! refusal check fails here.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{new_id, RelayOutcome};

use super::fixtures::{sample_entry, sample_project, sample_rule};
use crate::{MaintenanceRequest, Result, Store, StoreError, Surface, TraceFilter};

/// Methods that cannot refuse because they do not return a `Result`. They are pure readers of the
/// manifest (or of a per-item result vector), so there is nothing for them to refuse *with* — the
/// assertions on them live in [`assert_all_refuse`]'s own arms where they mean something.
const INFALLIBLE: &[&str] = &[
    "capabilities",
    "admission_is_atomic",
    "insert_events_checked",
    "serves_traces",
];

/// Assert that every method of an undeclared `surface` refuses.
pub(super) fn assert_all_refuse(store: &dyn Store, surface: Surface) -> Result<()> {
    let checked = match surface {
        // Not a surface a backend may decline: without it there is no ingest, no project, no key,
        // no verdict — nothing the rest of the system can be built on. Say so plainly rather than
        // letting the refusal walk run against a store that cannot answer anything.
        Surface::EventsCore | Surface::EventFilters => panic!(
            "backend `{}` does not declare {:?} — that is not an optional surface; every backend \
             must implement the ingest/read floor and its filter set",
            store.capabilities().backend,
            surface
        ),
        Surface::Traces => traces(store),
        Surface::Forecast => forecast(store),
        Surface::MarginBreakdowns => margin(store),
        Surface::Prompts => prompts(store),
        Surface::Relay => relay(store),
        Surface::Collective => collective(store),
        Surface::ProjectAdmin => project_admin(store),
        Surface::KeyAdmin => key_admin(store),
        Surface::LimitLifecycle => limit_lifecycle(store),
        Surface::JobLeases => job_leases(store),
        Surface::Schedules => schedules(store),
        Surface::Maintenance => maintenance(store),
        Surface::Metrics => metrics(store),
    };

    let uncovered: Vec<&str> = surface
        .methods()
        .iter()
        .copied()
        .filter(|m| !checked.contains(m) && !INFALLIBLE.contains(m))
        .collect();
    assert!(
        uncovered.is_empty(),
        "{surface:?} is refused by this backend but these methods were never checked for a \
         refusal: {uncovered:?} — an unchecked method is exactly where a silent empty page hides"
    );
    Ok(())
}

/// Assert one call refused, naming the method in the failure.
fn refused<T: std::fmt::Debug>(what: &str, r: Result<T>) {
    match r {
        Err(StoreError::Unsupported(_)) => {}
        got => panic!(
            "{what} must refuse with Unsupported on a backend that does not declare its surface, \
             got {got:?}"
        ),
    }
}

fn traces(store: &dyn Store) -> Vec<&'static str> {
    let (p, t) = (new_id(), new_id());
    assert!(
        !store.serves_traces(),
        "serves_traces must follow the manifest: the surface is undeclared"
    );
    refused("list_traces", store.list_traces(Some(&p), 10));
    refused(
        "list_traces_filtered",
        store.list_traces_filtered(Some(&p), &TraceFilter::default(), 10),
    );
    refused(
        "list_trace_events",
        store.list_trace_events(Some(&p), &t, 10),
    );
    refused("list_trace_scores", store.list_trace_scores(Some(&p), &t));
    refused("get_trace", store.get_trace(Some(&p), &t, 10));
    vec![
        "list_traces",
        "list_traces_filtered",
        "list_trace_events",
        "list_trace_scores",
        "get_trace",
    ]
}

fn forecast(store: &dyn Store) -> Vec<&'static str> {
    let (p, now) = (new_id(), Utc::now());
    let since = now - chrono::Duration::days(7);
    refused("daily_usage", store.daily_usage(&p, since, now));
    refused(
        "daily_cost_by_dimension",
        store.daily_cost_by_dimension(Some(&p), "customer", since, now),
    );
    vec!["daily_usage", "daily_cost_by_dimension"]
}

fn margin(store: &dyn Store) -> Vec<&'static str> {
    let (p, now) = (new_id(), Utc::now());
    let since = now - chrono::Duration::days(7);
    refused(
        "tokens_by_dimension",
        store.tokens_by_dimension(Some(&p), "customer", since, now),
    );
    refused(
        "customer_cost_by_model",
        store.customer_cost_by_model(Some(&p), "cus-1", since, now),
    );
    refused(
        "customer_cost_by_name",
        store.customer_cost_by_name(Some(&p), "cus-1", since, now),
    );
    vec![
        "tokens_by_dimension",
        "customer_cost_by_model",
        "customer_cost_by_name",
    ]
}

fn prompts(store: &dyn Store) -> Vec<&'static str> {
    let p = super::prompts::sample_prompt(&new_id());
    let v = super::prompts::sample_version(&p.id, 1);
    refused("create_prompt", store.create_prompt(&p));
    refused("update_prompt", store.update_prompt(&p));
    refused("get_prompt", store.get_prompt(&p.project_id, &p.name));
    refused("get_prompt_by_id", store.get_prompt_by_id(&p.id));
    refused("list_prompts", store.list_prompts(&p.project_id));
    refused("create_prompt_version", store.create_prompt_version(&v));
    refused("get_prompt_version", store.get_prompt_version(&p.id, 1));
    refused("list_prompt_versions", store.list_prompt_versions(&p.id));
    vec![
        "create_prompt",
        "update_prompt",
        "get_prompt",
        "get_prompt_by_id",
        "list_prompts",
        "create_prompt_version",
        "get_prompt_version",
        "list_prompt_versions",
    ]
}

fn relay(store: &dyn Store) -> Vec<&'static str> {
    let pid = new_id();
    let t = super::relay::sample_task(&pid, 3);
    refused("create_relay_task", store.create_relay_task(&t));
    refused("get_relay_task", store.get_relay_task(&t.id));
    refused(
        "find_relay_task_by_key",
        store.find_relay_task_by_key(&pid, "k"),
    );
    refused(
        "list_relay_tasks",
        store.list_relay_tasks(Some(&pid), None, 10),
    );
    refused("lease_relay_tasks", store.lease_relay_tasks("dev-1", 60, 5));
    refused("sweep_relay_dead", store.sweep_relay_dead());
    refused(
        "settle_relay_task",
        store.settle_relay_task(&t.id, None, &RelayOutcome::Succeeded(json!({}))),
    );
    refused(
        "renew_relay_lease",
        store.renew_relay_lease(&t.id, Utc::now(), 60),
    );
    refused(
        "update_relay_progress",
        store.update_relay_progress(&t.id, Utc::now(), "x"),
    );
    refused("cancel_relay_task", store.cancel_relay_task(&t.id));
    vec![
        "create_relay_task",
        "get_relay_task",
        "find_relay_task_by_key",
        "list_relay_tasks",
        "lease_relay_tasks",
        "sweep_relay_dead",
        "settle_relay_task",
        "renew_relay_lease",
        "update_relay_progress",
        "cancel_relay_task",
    ]
}

fn schedules(store: &dyn Store) -> Vec<&'static str> {
    let s = super::schedules::sample_schedule(&new_id());
    refused("create_schedule", store.create_schedule(&s));
    refused("get_schedule", store.get_schedule(&s.id));
    refused("list_schedules", store.list_schedules(&s.project_id));
    refused("update_schedule", store.update_schedule(&s));
    refused("delete_schedule", store.delete_schedule(&s.id));
    refused("due_schedules", store.due_schedules(Utc::now()));
    vec![
        "create_schedule",
        "get_schedule",
        "list_schedules",
        "update_schedule",
        "delete_schedule",
        "due_schedules",
    ]
}

fn collective(store: &dyn Store) -> Vec<&'static str> {
    let e = sample_entry();
    refused("upsert_collective_entry", store.upsert_collective_entry(&e));
    refused(
        "delete_collective_entries",
        store.delete_collective_entries(&e.contributor_id),
    );
    refused("list_collective_entries", store.list_collective_entries());
    refused(
        "purge_collective_entries_before",
        store.purge_collective_entries_before(Utc::now()),
    );
    vec![
        "upsert_collective_entry",
        "delete_collective_entries",
        "list_collective_entries",
        "purge_collective_entries_before",
    ]
}

fn project_admin(store: &dyn Store) -> Vec<&'static str> {
    refused("update_project", store.update_project(&sample_project()));
    vec!["update_project"]
}

fn key_admin(store: &dyn Store) -> Vec<&'static str> {
    let id = new_id();
    refused("list_api_keys", store.list_api_keys(&id));
    refused("set_api_key_revoked", store.set_api_key_revoked(&id, true));
    refused("set_api_key_expiry", store.set_api_key_expiry(&id, None));
    vec!["list_api_keys", "set_api_key_revoked", "set_api_key_expiry"]
}

fn limit_lifecycle(store: &dyn Store) -> Vec<&'static str> {
    let r = sample_rule();
    refused("get_limit_rule", store.get_limit_rule(&r.id));
    refused("update_limit_rule", store.update_limit_rule(&r));
    refused("delete_limit_rule", store.delete_limit_rule(&r.id));
    vec!["get_limit_rule", "update_limit_rule", "delete_limit_rule"]
}

fn job_leases(store: &dyn Store) -> Vec<&'static str> {
    let id = new_id();
    refused("cancel_job", store.cancel_job(&id));
    refused("renew_job_lease", store.renew_job_lease(&id, Utc::now()));
    vec!["cancel_job", "renew_job_lease"]
}

fn maintenance(store: &dyn Store) -> Vec<&'static str> {
    refused("storage_report", store.storage_report());
    refused(
        "maintenance_pass",
        store.maintenance_pass(MaintenanceRequest {
            truncate_wal: false,
            reclaim_pages: 0,
        }),
    );
    vec!["storage_report", "maintenance_pass"]
}

fn metrics(store: &dyn Store) -> Vec<&'static str> {
    refused("db_metrics", store.db_metrics());
    vec!["db_metrics"]
}
