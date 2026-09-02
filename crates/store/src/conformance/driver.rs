//! The surface-by-surface driver.
//!
//! One rule, applied uniformly: for every [`Surface`] the backend either serves it — in which case
//! its full semantics run — or it does not, in which case every method the surface owns must refuse
//! with [`StoreError::Unsupported`](crate::StoreError::Unsupported). A backend cannot sit between
//! the two, which is what "advisory" capabilities used to mean in practice.

use lighttrack_core::new_id;

use super::{
    admission, alerts, catalog, collective, devices, events, forecast, job_leases, jobs,
    maintenance, margin, margin_policy, pricing, projects, prompts, refusals, relay, revenue,
    rollup, schedules, score_summary, scores, traces,
};
use crate::{Result, Store, Surface};

pub(super) fn run(store: &dyn Store) -> Result<()> {
    let caps = store.capabilities();
    // One project for the whole run, as before: the sections that read a rollup depend on the events
    // an earlier section wrote, and `Surface::ALL` is ordered so `EventsCore` lays that foundation
    // first. Everything else is keyed by fresh ids and tolerant of a shared database.
    let pid = new_id();

    for &surface in Surface::ALL {
        if caps.has(surface) {
            section(store, &pid, surface)?;
        } else {
            refusals::assert_all_refuse(store, surface)?;
        }
    }
    Ok(())
}

/// The full semantics of one declared surface.
fn section(store: &dyn Store, pid: &str, surface: Surface) -> Result<()> {
    match surface {
        Surface::EventsCore => {
            events::events(store, pid)?;
            events::open_provider_identity(store)?;
            projects::projects_keys_limits(store, pid)?;
            scores::scores(store, pid)?;
            catalog::prices(store)?;
            catalog::benchmarks(store, pid)?;
            catalog::datasets(store, pid)?;
            catalog::rubrics(store, pid)?;
            jobs::jobs(store)?;
            admission::admission(store)?;
            admission::admission_batch(store)?;
            admission::admission_race(store)?;
            revenue::revenue(store)?;
        }
        Surface::EventFilters => {
            events::scoped_usage(store)?;
            events::parity_gap_methods(store)?;
        }
        Surface::Rollup => rollup::rollup(store)?,
        Surface::RedactionPosture => events::redaction_posture(store)?,
        Surface::RevenueReprice => revenue::reprice(store)?,
        Surface::ScoreFilters => scores::score_filters(store)?,
        Surface::Traces => traces::traces(store)?,
        Surface::Forecast => forecast::forecast(store)?,
        Surface::MarginBreakdowns => margin::margin(store)?,
        Surface::Prompts => prompts::prompts(store)?,
        Surface::Relay => relay::relay(store, pid)?,
        Surface::Collective => collective::collective(store)?,
        Surface::ProjectAdmin => projects::project_admin(store)?,
        Surface::KeyAdmin => projects::key_admin(store, pid)?,
        Surface::LimitLifecycle => projects::limit_lifecycle(store, pid)?,
        Surface::MarginPolicies => margin_policy::margin_policies(store, pid)?,
        Surface::JobLeases => {
            job_leases::job_cancellation(store)?;
            job_leases::job_leases(store)?;
        }
        Surface::Schedules => schedules::schedules(store, pid)?,
        Surface::Alerts => alerts::alerts(store, pid)?,
        Surface::AlertRouting => alerts::alert_routing(store, pid)?,
        Surface::Maintenance => maintenance::maintenance(store)?,
        Surface::Metrics => maintenance::metrics(store)?,
        Surface::Pricing => pricing::pricing(store)?,
        Surface::Devices => devices::devices(store)?,
        Surface::ScoreSummaries => score_summary::score_summaries(store)?,
    }
    Ok(())
}
