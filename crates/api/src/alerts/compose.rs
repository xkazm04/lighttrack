//! Turning a typed condition into an [`Alert`] row.
//!
//! The row's `payload` **is** the body a webhook receiver gets — `{event, text, content, subject,
//! …extra}` — so the ledger is not a summary of what was sent but the thing itself. That matters for
//! the two questions the ledger exists to answer: "what exactly did you tell them", and "resend it".
//! `text`/`content` are the Slack/Discord field names the existing receivers already switch on, and
//! `subject` is the email subject a webhook receiver simply ignores.

use std::collections::HashMap;

use serde_json::{json, Value};

use lighttrack_core::{Alert, AlertKind, LimitStatus, RelayTask, Severity};

use super::attribution::Attribution;
use super::detectors::{ErrorSpike, ScoreDrop};
use crate::forecast_alerts::ForecastAlert;

/// A finished benchmark run — the payload of a completion webhook (the CI gate contract).
#[derive(Clone)]
pub(crate) struct BenchRunAlert {
    pub(crate) benchmark: String,
    pub(crate) run_id: String,
    pub(crate) status: String,
    pub(crate) mean: Option<f64>,
    pub(crate) baseline: Option<f64>,
}

/// Assemble the delivered body. Every alert carries the same envelope, so a receiver can switch on
/// `event` and a channel driver never has to know which kind it is holding.
fn body(kind: AlertKind, msg: &str, subject: &str, extra: Value) -> Value {
    let mut b = json!({
        "event": kind.as_str(),
        "text": msg,
        "content": msg,
        "subject": subject,
    });
    if let (Some(obj), Some(add)) = (b.as_object_mut(), extra.as_object()) {
        for (k, v) in add {
            obj.insert(k.clone(), v.clone());
        }
    }
    b
}

pub(crate) fn breach(
    b: &LimitStatus,
    rejected: Option<&u64>,
    attribution: Option<&Attribution>,
    dedup_key: String,
) -> Alert {
    let msg = breach_message(b, rejected, attribution);
    let subject = format!("LightTrack: limit breach in '{}'", b.project_id);
    let extra = json!({
        "breach": b,
        "rejected_count": rejected,
        "attribution": attribution.map(|a| a.to_json()),
    });
    Alert::new(
        AlertKind::LimitBreach,
        Some(b.project_id.clone()),
        dedup_key,
        body(AlertKind::LimitBreach, &msg, &subject, extra),
    )
}

pub(crate) fn warning(w: &LimitStatus, dedup_key: String) -> Alert {
    let msg = warning_message(w);
    let subject = format!("LightTrack: approaching limit in '{}'", w.project_id);
    Alert::new(
        AlertKind::LimitWarning,
        Some(w.project_id.clone()),
        dedup_key,
        body(
            AlertKind::LimitWarning,
            &msg,
            &subject,
            json!({ "warning": w }),
        ),
    )
}

pub(crate) fn forecast(a: &ForecastAlert) -> Alert {
    Alert::new(
        AlertKind::ForecastAlert,
        Some(a.project_id.clone()),
        a.dedup_key(),
        body(
            AlertKind::ForecastAlert,
            &a.message,
            "LightTrack: spend forecast alert",
            json!({ "forecast": a }),
        ),
    )
    // The forecast's own `severity` is what decides whether a channel's floor lets it through: an
    // event three days out is not the same page as one three weeks out.
    .with_severity(match a.severity {
        "high" => Severity::Critical,
        _ => Severity::Warning,
    })
}

pub(crate) fn relay_dead(t: &RelayTask) -> Alert {
    let msg = format!(
        "LightTrack alert: relay task '{}' ({}) in project '{}' dead-lettered after {} \
         attempt(s) — {}",
        t.id,
        t.action_type,
        t.project_id,
        t.attempts,
        t.error.as_deref().unwrap_or("no error recorded"),
    );
    let subject = format!("LightTrack: relay task dead in '{}'", t.project_id);
    // Not the full row: payload/result can be large and may carry app data.
    let extra = json!({ "task": {
        "id": t.id, "project_id": t.project_id, "action_type": t.action_type,
        "source": t.source, "attempts": t.attempts, "error": t.error,
    }});
    Alert::new(
        AlertKind::RelayTaskDead,
        Some(t.project_id.clone()),
        format!("relay-dead:{}", t.id),
        body(AlertKind::RelayTaskDead, &msg, &subject, extra),
    )
}

pub(crate) fn error_spike(s: &ErrorSpike) -> Alert {
    let mins = (s.window_secs / 60).max(1);
    let sample = s.error.as_deref().unwrap_or("(no error message)");
    let msg = format!(
        "LightTrack alert: project '{}' logged {} failed call(s) within {}m. \
         Latest: {} on model '{}'. Sample error: {}",
        s.project_id, s.count, mins, s.status, s.model, sample
    );
    let subject = format!("LightTrack: error spike in '{}'", s.project_id);
    let extra = json!({ "spike": {
        "project_id": s.project_id, "count": s.count, "window_secs": s.window_secs,
        "model": s.model, "status": s.status, "error": s.error,
    }});
    Alert::new(
        AlertKind::ErrorSpike,
        Some(s.project_id.clone()),
        format!("error-spike:{}", s.project_id),
        body(AlertKind::ErrorSpike, &msg, &subject, extra),
    )
}

pub(crate) fn score_drop(d: &ScoreDrop, dedup_key: String) -> Alert {
    let msg = format!(
        "LightTrack alert: quality regression in '{}' — rubric '{}' down {:.0}% (recent mean \
         {:.2} vs baseline {:.2} over {} scores, judge {}).",
        d.project_id, d.rubric, d.drop_pct, d.recent_avg, d.baseline_avg, d.samples, d.scored_by
    );
    let subject = format!("LightTrack: quality regression in '{}'", d.project_id);
    let extra = json!({ "drop": {
        "project_id": d.project_id, "rubric": d.rubric, "recent_avg": d.recent_avg,
        "baseline_avg": d.baseline_avg, "drop_pct": d.drop_pct, "samples": d.samples,
        "scored_by": d.scored_by,
    }});
    Alert::new(
        AlertKind::ScoreDrop,
        Some(d.project_id.clone()),
        dedup_key,
        body(AlertKind::ScoreDrop, &msg, &subject, extra),
    )
}

pub(crate) fn bench_run(r: &BenchRunAlert) -> Alert {
    let msg = format!(
        "LightTrack benchmark '{}' run {} finished: {}{}",
        r.benchmark,
        r.run_id,
        r.status,
        match (r.mean, r.baseline) {
            (Some(m), Some(b)) => format!(" (mean {m:.3} vs baseline {b:.3})"),
            (Some(m), None) => format!(" (mean {m:.3})"),
            _ => String::new(),
        },
    );
    let extra = json!({
        "benchmark": r.benchmark, "run_id": r.run_id, "status": r.status,
        "mean": r.mean, "baseline": r.baseline,
    });
    Alert::new(
        AlertKind::BenchRun,
        None,
        format!("bench-run:{}:{}", r.benchmark, r.status),
        body(
            AlertKind::BenchRun,
            &msg,
            "LightTrack: benchmark run",
            extra,
        ),
    )
}

/// One flushed rejection bucket: how many ingest attempts a cap turned away since the last flush.
///
/// An alert row and never an event — a rejected call is deliberately not stored as an event,
/// because it would corrupt the usage rollups every cap is evaluated against.
///
/// The dedup key carries `flushed_at`, so **every flush is its own row**. A flush is a delta the
/// in-process counter has already thrown away; a key without the instant collided with the previous
/// flush inside the alert cooldown, the store answered `Suppressed`, and the delta was gone — at the
/// default cadence (900s) against the default cooldown (3600s), three of every four flushes.
pub(crate) fn ingest_rejected(
    project: &str,
    buckets: &HashMap<String, (u64, f64)>,
    flushed_at: chrono::DateTime<chrono::Utc>,
) -> Alert {
    let total: u64 = buckets.values().map(|(n, _)| n).sum();
    let cost: f64 = buckets.values().map(|(_, c)| c).sum();
    let mut names: Vec<&String> = buckets.keys().collect();
    names.sort();
    let msg = format!(
        "LightTrack: project '{project}' had {total} ingest attempt(s) rejected by its limits \
         (~${cost:.4} of spend turned away) across {}.",
        names
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    let extra = json!({ "rejected": {
        "project_id": project,
        "count": total,
        "est_missed_cost_usd": cost,
        "rules": buckets.iter().map(|(k, (n, c))| json!({
            "rule": k, "count": n, "est_missed_cost_usd": c,
        })).collect::<Vec<_>>(),
    }});
    Alert::new(
        AlertKind::IngestRejected,
        Some(project.to_string()),
        format!(
            "ingest-rejected:{project}:{}",
            lighttrack_store::codec::fmt_ts(flushed_at)
        ),
        body(
            AlertKind::IngestRejected,
            &msg,
            &format!("LightTrack: limits are rejecting traffic in '{project}'"),
            extra,
        ),
    )
    .with_severity(Severity::Warning)
}

fn warning_message(w: &LimitStatus) -> String {
    let warn_pct = w.warn_at.map(|f| f * 100.0).unwrap_or(0.0);
    format!(
        "LightTrack warning: project '{}' is approaching its {:?}/{:?} limit — current {:.4} is \
         {:.0}% of threshold {:.4} (warns at {:.0}%). No traffic has been blocked.",
        w.project_id,
        w.metric,
        w.window,
        w.current,
        w.ratio * 100.0,
        w.threshold,
        warn_pct
    )
}

fn breach_message(
    b: &LimitStatus,
    rejected: Option<&u64>,
    attribution: Option<&Attribution>,
) -> String {
    let tail = match rejected {
        Some(n) => format!(" — {n} ingest attempt(s) rejected so far in this window"),
        None => String::new(),
    };
    // Name the scoped dimension so a "cap gpt-4o" breach reads differently from a project-wide one.
    let scope = match &b.scope {
        Some(s) => format!(" [scope {}]", s.label()),
        None => String::new(),
    };
    let spenders = attribution
        .and_then(|a| a.message_tail())
        .unwrap_or_default();
    format!(
        "LightTrack alert: project '{}'{scope} breached {:?}/{:?} limit — current {:.4} >= \
         threshold {:.4} ({:.0}% of limit), action={:?}{tail}.{spenders}",
        b.project_id,
        b.metric,
        b.window,
        b.current,
        b.threshold,
        b.ratio * 100.0,
        b.action
    )
}

/// The message a channel driver renders. Reads the composed body rather than re-deriving it, so a
/// delivered alert and its stored row can never disagree.
pub(crate) fn text_of(a: &Alert) -> &str {
    a.payload
        .get("text")
        .and_then(Value::as_str)
        .unwrap_or("LightTrack alert")
}

pub(crate) fn subject_of(a: &Alert) -> &str {
    a.payload
        .get("subject")
        .and_then(Value::as_str)
        .unwrap_or("LightTrack alert")
}
