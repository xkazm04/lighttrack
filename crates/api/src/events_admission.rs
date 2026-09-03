//! Post-admission side effects shared by the single- and batch-ingest doors — breach logging, the
//! rejection ledger, alert fan-out, error-spike feed — and the human-facing reason an admission was
//! refused.

use chrono::Utc;

use lighttrack_core::{LimitStatus, LlmEvent, Status};
use lighttrack_store::Admission;

use crate::state::AppState;

/// Post-admission side effects shared by the single- and batch-ingest paths: log and best-effort
/// deliver breach alerts, count a rejected event into the rejection ledger, and (for an admitted
/// non-success call) feed error-spike detection. Returns the breached statuses so the caller can
/// shape its response (429 vs. observe-only flag).
pub(crate) fn on_admission(
    st: &AppState,
    ev: &LlmEvent,
    admission: &Admission,
) -> Vec<LimitStatus> {
    let breached: Vec<LimitStatus> = admission
        .statuses
        .iter()
        .filter(|s| s.breached)
        .cloned()
        .collect();
    for b in &breached {
        tracing::warn!(
            project_id = %b.project_id,
            metric = ?b.metric,
            window = ?b.window,
            value = b.current,
            threshold = b.threshold,
            action = ?b.action,
            "usage limit breached",
        );
    }
    // A rejected event is never stored (that would corrupt usage/cost), so count it out-of-band in the
    // best-effort rejection ledger — the running per-key count then rides along on the breach alert.
    // Its estimated cost is the priced `cost_usd` if we resolved one, else $0 (unpriced).
    // Rejection is not always a breach: an enforcing cost cap whose window cannot be priced at all
    // refuses ingest without any status reading "breached", so the ledger is fed from every status
    // that rejects, not just the breached ones.
    let rej_counts = if admission.admitted {
        std::collections::HashMap::new()
    } else {
        record_rejection(st, ev, &admission.statuses)
    };
    for s in admission.statuses.iter().filter(|s| s.shedding) {
        tracing::info!(
            project_id = %s.project_id,
            metric = ?s.metric,
            window = ?s.window,
            ratio = s.ratio,
            shed_pct = s.shed_fraction * 100.0,
            event_id = %ev.id,
            "throttling ingest: graduated back-pressure, not a breach",
        );
    }
    // Best-effort, off the request path: deliver breaches to webhook/ntfy (deduped per cooldown).
    st.alerts.notify(&breached, &rej_counts);
    // Soft-warning tier: for an *admitted* event, alert on any rule that crossed its warn_at without
    // breaching — the operator's early heads-up before the cap actually bites. Only when admitted, so
    // the usage the warning reports genuinely includes a recorded event (a rejected event isn't stored).
    if admission.admitted {
        let warnings: Vec<LimitStatus> = admission
            .statuses
            .iter()
            .filter(|s| s.warning)
            .cloned()
            .collect();
        if !warnings.is_empty() {
            st.alerts.notify_warnings(&warnings);
        }
    }
    // Best-effort error-spike detection: only admitted non-success calls count toward the threshold.
    if admission.admitted && ev.status != Status::Success {
        st.alerts.record_error(ev);
    }
    breached
}

/// Fold a just-rejected event into the rejection ledger — once per enforcing breach that turned it
/// away — and return the running rejection count for each, keyed the same way the alerter dedups
/// breaches ([`LimitStatus::alert_key`], which includes the scope) so the count can be attached to
/// the outgoing alert.
fn record_rejection(
    st: &AppState,
    ev: &LlmEvent,
    statuses: &[LimitStatus],
) -> std::collections::HashMap<String, u64> {
    let cost = ev.cost_usd.unwrap_or(0.0);
    let now = Utc::now();
    let mut counts = std::collections::HashMap::new();
    // Every status that turned this event away, hard stop or graduated shed alike — otherwise the
    // ledger would go blind exactly while throttling is doing its job.
    for b in statuses.iter().filter(|s| s.rejects_ingest() || s.shedding) {
        let count = st.rejections.record(
            &b.project_id,
            b.metric,
            b.window,
            b.scope.clone(),
            cost,
            now,
        );
        counts.insert(b.alert_key(), count);
    }
    counts
}

/// Human-facing reason an admission was rejected — pass the full status set, since neither an
/// unpriceable cost cap nor a graduated throttle shed reads as "breached".
pub(crate) fn breach_reason(statuses: &[LimitStatus]) -> String {
    // A graduated shed is not a breach and must not be described as one: nothing is over budget, the
    // caller is being asked to slow down on the approach. Only reported when no hard stop applies.
    if !statuses.iter().any(|s| s.rejects_ingest()) {
        if let Some(s) = statuses.iter().find(|s| s.shedding) {
            let scope = match &s.scope {
                Some(sc) => format!(" [scope {}]", sc.label()),
                None => String::new(),
            };
            return format!(
                "ingest throttled: project '{}'{scope} is at {:.0}% of its {:?}/{:?} limit \
                 ({:.4} of {:.4}); {:.0}% of ingest is being shed on the approach. Not over budget — \
                 slow down and retry in {}s.",
                s.project_id,
                s.ratio * 100.0,
                s.metric,
                s.window,
                s.current,
                s.threshold,
                s.shed_fraction * 100.0,
                s.retry_after_secs()
            );
        }
    }
    statuses
        .iter()
        .find(|s| s.rejects_ingest())
        .map(|s| {
            let scope = match &s.scope {
                Some(sc) => format!(" [scope {}]", sc.label()),
                None => String::new(),
            };
            if s.unpriceable() && !s.breached {
                // The distinct, visible condition: we are not over budget, we simply cannot measure
                // the budget. Say exactly that, and how to fix it.
                return format!(
                    "ingest blocked: project '{}'{scope} has an enforcing {:?}/{:?} cost limit but \
                     no priced traffic in the window — this model is absent from the price book, so \
                     the cap cannot be measured. Add a price for it (POST /v1/prices) or cap on \
                     calls/tokens instead.",
                    s.project_id, s.metric, s.window
                );
            }
            let estimated = if s.estimated() { " (includes imputed cost for unpriced calls)" } else { "" };
            // A derived threshold has to explain itself here above all: "$329.60" with no story is
            // a number the caller cannot act on, while "80% of $412.00 recognized customer revenue"
            // tells them both why they were stopped and what would move the cap.
            let basis = match s.basis.describe() {
                Some(d) => format!(" — {d}"),
                None => String::new(),
            };
            format!(
                "ingest blocked: project '{}'{scope} is over its {:?}/{:?} limit \
                 ({:.4} >= {:.4}, action={:?}){estimated}{basis}",
                s.project_id, s.metric, s.window, s.current, s.threshold, s.action
            )
        })
        .unwrap_or_else(|| "ingest blocked: usage limit exceeded".to_string())
}
