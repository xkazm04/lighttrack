//! The endpoint table, split by domain so no one file grows past the repo's size rule.
//!
//! Order matters only for readability: each file is kept in the same order as the corresponding
//! block of `crates/api/src/main.rs::build_router`, so the two read as one list — which is what
//! makes the bijection test's failure message actionable rather than a set difference.

use crate::types::Endpoint;

mod alerts;
mod benchmarks;
mod collective;
mod costs;
mod datasets;
mod jobs;
mod labels;
mod limits;
mod observability;
mod platform;
mod projects;
mod prompts;
mod relay;
mod revenue;
mod rubrics;
mod scores;

/// Every group, in router order. A `&[&[Endpoint]]` rather than one flat `const` because a const
/// slice cannot be concatenated at compile time and a 130-row table in one file would break the
/// size rule; [`crate::endpoints`] flattens it for every caller.
pub(crate) const GROUPS: &[&[Endpoint]] = &[
    platform::ENDPOINTS,
    observability::ENDPOINTS,
    costs::ENDPOINTS,
    scores::ENDPOINTS,
    datasets::ENDPOINTS,
    labels::ENDPOINTS,
    rubrics::ENDPOINTS,
    benchmarks::ENDPOINTS,
    prompts::ENDPOINTS,
    jobs::ENDPOINTS,
    projects::ENDPOINTS,
    limits::ENDPOINTS,
    relay::ENDPOINTS,
    revenue::ENDPOINTS,
    collective::ENDPOINTS,
    alerts::ENDPOINTS,
];
