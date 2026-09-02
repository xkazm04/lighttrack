//! Every table in the logical schema, declared as const data.
//!
//! [`ALL`] is the source of truth the three renderers, the migration lists, the select lists and
//! `schema_fingerprint()` all read. Adding a column is one edit here plus the intent that motivated
//! it; nothing else in the repository has to be told about it.
//!
//! **Order matters twice.** Tables appear in `ALL` in dependency order (a table is declared before
//! anything that references it), and a table's columns appear in *wire* order — the order every
//! generated select list uses — with post-ship columns carrying `.added(..)` wherever they sit.

mod eval;
mod ingest;
mod ops;
mod registry;

use super::model::Table;

pub use eval::{
    BENCHMARKS, BENCHMARK_RUNS, CALIBRATIONS, DATASETS, DATASET_ITEMS, LABELS, RUBRICS, SCORES,
};
pub use ingest::{API_KEYS, EVENTS, JOBS, LIMIT_RULES, PROJECTS, SCHEDULES};
pub use ops::{
    ALERTS, ALERT_CHANNELS, COLLECTIVE_CONTRIBUTIONS, COLLECTIVE_ENTRIES, DEVICES, RELAY_TASKS,
};
pub use registry::{MARGIN_POLICIES, MODEL_PRICES, PROMPTS, PROMPT_VERSIONS, REVENUE_EVENTS};

/// The whole logical schema, in creation order.
pub fn all() -> &'static [&'static Table] {
    ALL
}

/// The whole logical schema, in creation order.
pub static ALL: &[&Table] = &[
    &PROJECTS,
    &API_KEYS,
    &EVENTS,
    &LIMIT_RULES,
    &SCORES,
    &BENCHMARKS,
    &RUBRICS,
    &JOBS,
    &PROMPTS,
    &PROMPT_VERSIONS,
    &BENCHMARK_RUNS,
    &MODEL_PRICES,
    &DATASETS,
    &DATASET_ITEMS,
    &REVENUE_EVENTS,
    &COLLECTIVE_ENTRIES,
    &RELAY_TASKS,
    &MARGIN_POLICIES,
    &SCHEDULES,
    &DEVICES,
    &ALERTS,
    &ALERT_CHANNELS,
    &COLLECTIVE_CONTRIBUTIONS,
    &LABELS,
    &CALIBRATIONS,
];

/// The table with this name, if the model declares one.
pub fn find(name: &str) -> Option<&'static Table> {
    all().iter().copied().find(|t| t.name == name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn every_table_is_declared_once_and_has_a_key() {
        let mut seen = BTreeSet::new();
        for t in all() {
            assert!(seen.insert(t.name), "{} declared twice", t.name);
            let inline_pk = t.columns.iter().filter(|c| c.pk).count();
            assert!(
                inline_pk == 1 || !t.primary_key.is_empty(),
                "{} has no primary key",
                t.name
            );
            let mut cols = BTreeSet::new();
            for c in t.columns {
                assert!(cols.insert(c.name), "{}.{} declared twice", t.name, c.name);
            }
            for k in t.primary_key {
                assert!(cols.contains(k), "{}: pk column {k} not declared", t.name);
            }
        }
    }

    /// A `NOT NULL` column added after the table shipped must carry a default, or the `ALTER` fails
    /// on the first database that already has rows — the one failure mode a migration list cannot
    /// be tested into on a developer's fresh machine.
    #[test]
    fn an_added_not_null_column_has_a_default() {
        for t in all() {
            for c in t.added_columns() {
                assert!(
                    c.nullable || c.default.is_some(),
                    "{}.{} is NOT NULL and added later, but has no default",
                    t.name,
                    c.name
                );
            }
        }
    }

    /// Index names are global in SQLite and Postgres alike.
    #[test]
    fn index_names_are_unique_across_the_schema() {
        let mut seen = BTreeSet::new();
        for t in all() {
            for i in t.indexes {
                assert!(seen.insert(i.name), "duplicate index name {}", i.name);
            }
        }
    }
}
