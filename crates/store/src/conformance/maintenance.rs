//! `Surface::Maintenance` and `Surface::Metrics`: what the store reports about its own disk and its
//! own latency.
//!
//! These are the two surfaces where an *empty* answer is the most dangerous one — "0 bytes, no
//! tables" and "no slow operations" both read as good news. So a backend that measures nothing must
//! refuse (see `refusals`), and one that declares the surface has to produce a report that is
//! actually about something.

use crate::{MaintenanceOutcome, MaintenanceRequest, Result, Store};

pub(super) fn maintenance(store: &dyn Store) -> Result<()> {
    let report = store.storage_report()?;
    assert!(
        !report.objects.is_empty(),
        "a declared storage report names the objects it measured — an empty one reads as \
         'nothing is stored', which is never true of an initialized schema"
    );
    assert!(
        report.db_bytes > 0,
        "an initialized database occupies bytes: {report:?}"
    );

    // Lossless by construction: there is no pruning parameter, and a pass must not lose rows. Run
    // the routine (non-truncating, no reclamation) form — the escalation rungs need the writer.
    let before = store.list_projects()?.len();
    let pass = store.maintenance_pass(MaintenanceRequest {
        truncate_wal: false,
        reclaim_pages: 0,
    })?;
    assert!(
        matches!(
            pass.outcome,
            MaintenanceOutcome::Ran | MaintenanceOutcome::NothingToDo
        ),
        "a routine pass either does work or has none to do; it does not fail: {pass:?}"
    );
    assert_eq!(
        store.list_projects()?.len(),
        before,
        "maintenance is lossless — it never removes a row"
    );
    Ok(())
}

pub(super) fn metrics(store: &dyn Store) -> Result<()> {
    // The suite has issued plenty of reads and writes by now, so a store that instruments itself has
    // something to say. Families nobody called are omitted rather than rendered as rows of zeros.
    let report = store.db_metrics()?;
    assert!(
        report.ring_capacity > 0,
        "the percentile ring has a declared bound: {report:?}"
    );
    assert!(
        !report.ops.is_empty(),
        "after a full conformance run the store has observed its own operations — an empty \
         profile reads as 'everything is fast'"
    );
    assert!(
        report.ops.iter().all(|o| o.count > 0),
        "only families that actually ran are reported: {:?}",
        report.ops
    );
    Ok(())
}
