//! Backend-agnostic conformance suite: exercises the full [`Store`] trait and asserts round-trips,
//! so SQLite, Postgres, and Firestore can be held to identical behavior.
//!
//! Each backend crate has an integration test that constructs its store and calls [`run`]. The
//! SQLite (in-memory) test runs in CI always; the Postgres / Firestore tests run only when a test
//! env var points at one. Safe against a **non-empty** database: everything is scoped to a fresh
//! unique project + unique ids, and the inherently-global checks (prices, the job claim) are tolerant.
//!
//! ## The manifest drives the run
//!
//! Sections used to opt out of themselves — a `match` on `Err(Unsupported)` that printed "skipping"
//! and returned `Ok`. That is indistinguishable from a section that was never written, which is how
//! forecast, margin, prompts, collective, `update_project` and the maintenance surfaces went years
//! with no coverage at all. [`driver`] instead walks [`Surface::ALL`] against the backend's
//! [`Capabilities`](crate::Capabilities): a declared surface runs its full semantics, an undeclared
//! one must **refuse every one of its methods** with [`StoreError::Unsupported`]
//! ([`refusals`]). There is no third answer, so a silent empty page cannot pass.

mod admission;
mod alerts;
mod catalog;
mod collective;
mod devices;
mod driver;
mod events;
mod fixtures;
mod forecast;
mod job_leases;
mod jobs;
mod labels;
mod maintenance;
mod margin;
mod margin_policy;
mod pricing;
mod projects;
mod prompts;
mod refusals;
mod relay;
mod relay_lease;
mod revenue;
mod rollup;
mod schedules;
mod scores;
mod traces;

use crate::{Result, Store};

pub use admission::{admission_race_probe, RaceOutcome};

/// Run the full conformance suite against `store` (assumed already schema-initialized by its
/// constructor). Panics on a failed assertion; returns `Err` on a backend error.
pub fn run(store: &dyn Store) -> Result<()> {
    driver::run(store)
}
