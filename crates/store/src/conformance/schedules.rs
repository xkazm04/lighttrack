//! `Surface::Schedules`: recurrence as a row — write it, list it, disable it, and see it come due.

use chrono::{Duration, Utc};
use serde_json::json;

use lighttrack_core::{new_id, Schedule};

use crate::{Result, Store};

pub(super) fn sample_schedule(pid: &str) -> Schedule {
    Schedule {
        id: new_id(),
        project_id: pid.into(),
        kind: "bench_run".into(),
        payload: json!({ "benchmark_id": "b-conf", "samples": 2 }),
        interval_secs: 3600,
        next_due: Utc::now() - Duration::seconds(1),
        last_job_id: None,
        enabled: true,
        created_at: Utc::now(),
    }
}

pub(super) fn schedules(store: &dyn Store, pid: &str) -> Result<()> {
    let s = sample_schedule(pid);
    store.create_schedule(&s)?;

    let got = store.get_schedule(&s.id)?.expect("get_schedule Some");
    assert_eq!(got.kind, "bench_run");
    assert_eq!(
        got.payload,
        json!({ "benchmark_id": "b-conf", "samples": 2 }),
        "schedule payload round-trip"
    );
    assert_eq!(got.interval_secs, 3600);
    assert!(got.enabled);

    // Listing is project-scoped: a schedule must not be visible from a project that does not own it.
    assert!(store.list_schedules(pid)?.iter().any(|x| x.id == s.id));
    assert!(!store
        .list_schedules(&new_id())?
        .iter()
        .any(|x| x.id == s.id));

    // Due: `next_due` is in the past, so the sweep sees it. The read is global (every project's
    // due work in one pass), so on a shared DB assert on our id and tolerate other rows.
    assert!(
        store
            .due_schedules(Utc::now())?
            .iter()
            .any(|x| x.id == s.id),
        "an enabled schedule whose next_due has passed must come back due"
    );

    // The sweep's own write: record the job it produced and push next_due out.
    let mut fired = got;
    fired.last_job_id = Some("job-conf".into());
    fired.next_due = fired.advance_from(Utc::now());
    assert!(store.update_schedule(&fired)?, "update finds the row");
    let after = store.get_schedule(&s.id)?.expect("get after update");
    assert_eq!(after.last_job_id.as_deref(), Some("job-conf"));
    assert!(
        !store
            .due_schedules(Utc::now())?
            .iter()
            .any(|x| x.id == s.id),
        "a fired schedule must not still be due — that is how a sweep stacks duplicate jobs"
    );

    // Disabled is not deleted: the row stays readable and listable, and simply stops firing. An
    // operator pausing a schedule must be able to see the thing they paused.
    let mut off = after;
    off.enabled = false;
    off.next_due = Utc::now() - Duration::days(1);
    store.update_schedule(&off)?;
    assert!(
        !store
            .due_schedules(Utc::now())?
            .iter()
            .any(|x| x.id == s.id),
        "a disabled schedule is never due, however long overdue it looks"
    );
    assert!(store.get_schedule(&s.id)?.is_some());

    // Updating something that is not there says so rather than silently creating it.
    let mut ghost = sample_schedule(pid);
    ghost.id = new_id();
    assert!(!store.update_schedule(&ghost)?);

    assert!(store.delete_schedule(&s.id)?);
    assert!(store.get_schedule(&s.id)?.is_none());
    assert!(
        !store.delete_schedule(&s.id)?,
        "a second delete finds nothing"
    );
    Ok(())
}
