//! Events: ingest, listing, cost rollups, rolling-window usage, single lookup.
//!
//! Split by concern rather than by layer: [`cols`] owns the `COLS` ↔ `from_row` positional contract
//! and the aggregate select lists, [`write`] the INSERT shared with the admission transaction,
//! [`filters`] the `WHERE` accumulator the reads share, and [`list`] / [`cost`] / [`usage`] the three
//! read families. Sibling modules and [`crate::admission`] keep importing from `crate::events`, so
//! the split is invisible outside this directory.

mod cols;
mod cost;
mod filters;
mod list;
mod usage;
mod write;

pub(crate) use cols::{from_row, map_usage, COLS, RECEIVED, USAGE_COLS};
pub(crate) use cost::{cost_summary, cost_summary_windowed, usecase_costs};
pub(crate) use list::{get, list, list_filtered};
pub(crate) use usage::{scope_expr, usage_by_scope, usage_since, usage_since_scoped};
pub(crate) use write::{insert, insert_err, insert_query};
