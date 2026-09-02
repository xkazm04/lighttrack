//! One-time migration: benchmarks carrying `target.schedule_interval_secs` become `Schedule` rows.
//!
//! Recurrence used to be a key smuggled into a benchmark's `target`, read by a sweep inside
//! `lt-runner serve`. That sweep is gone, so without this every benchmark an operator had opted
//! into continuous monitoring would quietly stop running — the exact failure mode the whole
//! milestone exists to make impossible. The key stays *readable* for one release (nothing deletes
//! it); this only ensures a schedule exists beside it.
//!
//! Idempotent by construction: a benchmark that already has a `bench_run` schedule naming it is
//! skipped, so the migration is safe to run on every boot — which is how it is invoked, because a
//! migration that runs once and is never checked again is a migration that half-ran on the deploy
//! that crashed.

use chrono::Utc;

use lighttrack_core::{new_id, JobKind, Schedule, RECURRENCE_KEY, SCHEDULE_MIN_INTERVAL_SECS};

use crate::state::{spawn_db, AppState};

/// Create a schedule for every recurring benchmark that does not already have one. Returns how many
/// were created. Never propagates: a backend without the `Schedules` surface answers `Unsupported`,
/// which is a declared capability gap, and a migration is not worth refusing to boot over.
pub(crate) async fn migrate_benchmark_recurrence(st: &AppState) -> usize {
    let store = st.store.clone();
    let created = spawn_db(move || {
        let mut created = 0usize;
        for project in store.list_projects()? {
            let existing = store.list_schedules(&project.id)?;
            for b in store.list_benchmarks(&project.id)? {
                let Some(secs) = b
                    .target
                    .get(RECURRENCE_KEY)
                    .and_then(serde_json::Value::as_u64)
                    .filter(|s| *s > 0)
                else {
                    continue;
                };
                let already = existing.iter().any(|s| {
                    s.kind == JobKind::BenchRun.as_str()
                        && s.payload.get("benchmark_id").and_then(|v| v.as_str()) == Some(&b.id)
                });
                if already {
                    continue;
                }
                let now = Utc::now();
                store.create_schedule(&Schedule {
                    id: new_id(),
                    project_id: project.id.clone(),
                    kind: JobKind::BenchRun.as_str().to_string(),
                    payload: serde_json::json!({ "benchmark_id": b.id, "samples": 1 }),
                    interval_secs: (secs as u32).max(SCHEDULE_MIN_INTERVAL_SECS),
                    // Due one interval out, not immediately: a migration must not fire every
                    // recurring benchmark in the deployment the moment the new build boots.
                    next_due: now + chrono::Duration::seconds(secs as i64),
                    last_job_id: None,
                    enabled: true,
                    created_at: now,
                })?;
                created += 1;
            }
        }
        Ok(created)
    })
    .await;
    match created {
        Ok(0) => 0,
        Ok(n) => {
            tracing::info!(
                created = n,
                "migrated benchmark `target.schedule_interval_secs` recurrence into stored \
                 schedules (the key stays readable for one release)"
            );
            n
        }
        Err(e) => {
            tracing::debug!(error = %e, "recurrence migration skipped");
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::redact::Redactor;
    use crate::tests_ingest::{make_key, setup};
    use lighttrack_store::Store;

    fn benchmark(store: &dyn Store, project: &str, target: serde_json::Value) -> String {
        let b: lighttrack_core::Benchmark = serde_json::from_value(serde_json::json!({
            "project_id": project,
            "name": format!("b-{}", new_id()),
            "rubric": "helpfulness",
            "target": target,
        }))
        .unwrap();
        store.create_benchmark(&b).unwrap();
        b.id
    }

    #[tokio::test]
    async fn recurring_benchmarks_keep_recurring_and_the_migration_never_doubles_up() {
        let (state, store) = setup(Redactor::off());
        make_key(&store, "proj-a");
        let recurring = benchmark(
            store.as_ref(),
            "proj-a",
            serde_json::json!({ RECURRENCE_KEY: 3600, "endpoint": "x" }),
        );
        let plain = benchmark(
            store.as_ref(),
            "proj-a",
            serde_json::json!({ "endpoint": "x" }),
        );

        assert_eq!(migrate_benchmark_recurrence(&state).await, 1);
        let scheds = store.list_schedules("proj-a").unwrap();
        assert_eq!(scheds.len(), 1, "only the opted-in benchmark gets one");
        assert_eq!(scheds[0].kind, "bench_run");
        assert_eq!(scheds[0].payload["benchmark_id"], recurring.as_str());
        assert_eq!(scheds[0].interval_secs, 3600);
        assert!(
            scheds[0].next_due > Utc::now(),
            "a migration must not fire every recurring benchmark the moment a new build boots"
        );
        assert!(!scheds
            .iter()
            .any(|s| s.payload["benchmark_id"] == plain.as_str()));

        // Runs on every boot, so it has to be idempotent — a second pass creates nothing.
        assert_eq!(migrate_benchmark_recurrence(&state).await, 0);
        assert_eq!(store.list_schedules("proj-a").unwrap().len(), 1);
    }
}
