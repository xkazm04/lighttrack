//! `jobs` collection + the concurrency-safe claim (optimistic `updateTime` precondition instead of
//! SQL `FOR UPDATE SKIP LOCKED`).

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::{job_is_terminal, Job, JobCancel, JobFinish, JOB_ERROR_WORKER_LOST};
use lighttrack_store::{Result, StoreError};

use crate::codec::*;
use crate::rest::Rest;

pub(crate) fn create_job(rest: &Rest, j: &Job) -> Result<()> {
    rest.put_doc("jobs", &j.id, &to_fields(j)?)
}

pub(crate) fn get_job(rest: &Rest, project: Option<&str>, id: &str) -> Result<Option<Job>> {
    let j = rest
        .get_doc("jobs", id)?
        .as_ref()
        .map(from_fields)
        .transpose()?;
    Ok(crate::scope::keep(project, j, |j| j.project_id.as_deref()))
}

/// The queue as one scope sees it. A project reads only the work stamped with its own id; the
/// operator additionally reads the project-less rows (sweeps, and anything enqueued before the
/// field existed).
pub(crate) fn list_jobs(
    rest: &Rest,
    project: Option<&str>,
    status: Option<&str>,
    limit: usize,
) -> Result<Vec<Job>> {
    let mut filters: Vec<(&str, &str, Value)> = match status {
        Some(s) => vec![("status", "EQUAL", json!(s))],
        None => vec![],
    };
    crate::scope::push_filter(&mut filters, project);
    let docs = rest.query("jobs", &filters, Some(("created_at", true)), Some(limit))?;
    docs.iter().map(from_fields).collect()
}

pub(crate) fn update_job_progress(rest: &Rest, id: &str, progress: &str) -> Result<()> {
    let mut m = Fields::new();
    m.insert("progress".into(), json!(progress));
    m.insert("updated_at".into(), json!(fmt_ts(Utc::now())));
    rest.patch_fields("jobs", id, &m, &["progress", "updated_at"])
}

/// Extend the holder's lease. Firestore has no conditional `UPDATE … WHERE`, so the condition is an
/// `updateTime` precondition over a read-compare-commit loop — the same mechanism `claim_job` and
/// `cancel_job` already use here, and it gives the same guarantee: the write lands only if nobody
/// changed the document in between.
///
/// `None` means this caller no longer holds the job (its `claimed_at` moved, or the job left the
/// live set). That is affirmative evidence its work loop must read and stop on, not a guess.
///
/// `cancelling` is renewable on purpose: a run being asked to stop is still running, still
/// spending, and still has to reach its next case boundary and finish honestly.
pub(crate) fn renew_job_lease(
    rest: &Rest,
    id: &str,
    fence: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let want = fmt_ts(fence);
    for _ in 0..5 {
        let Some(doc) = doc_by_id(rest, id)? else {
            return Ok(None); // no such job — nothing to renew, and nothing to pretend about
        };
        let (name, update_time) = doc_handle(&doc);
        let fields = decode_doc(&doc);
        if fstr(&fields, "claimed_at").as_deref() != Some(want.as_str()) {
            return Ok(None); // someone else's lease now
        }
        if !matches!(
            fields.get("status").and_then(Value::as_str),
            Some("running") | Some("cancelling")
        ) {
            return Ok(None); // not live: nothing to keep alive
        }
        let now = Utc::now();
        let mut m = Fields::new();
        m.insert("claimed_at".into(), json!(fmt_ts(now)));
        m.insert("updated_at".into(), json!(fmt_ts(now)));
        if rest.commit_update(&name, &m, &["claimed_at", "updated_at"], Some(&update_time))? {
            return Ok(Some(now));
        }
        // Precondition failed: the doc changed under us. Re-read and decide against the new state
        // rather than retrying blind — the change may BE the takeover we are checking for.
    }
    Err(StoreError::Conflict(format!(
        "job '{id}' changed under every lease-renewal attempt; retry"
    )))
}

/// Finish a job — the last write in the lifecycle, and a conditioned one like every other.
///
/// Two conditions, enforced here by read-compare-commit under an `updateTime` precondition because
/// Firestore has no conditional update: **still non-terminal** (a verdict is final) and, when a
/// `fence` is supplied, **still mine** (`claimed_at` is exactly what the caller was handed at
/// claim). Without the second, a worker reclaimed as stale while it was busy finishes later and
/// overwrites the verdict its replacement already wrote — silently, with a plausible result.
///
/// An error means the job RAN and the work failed, so it consumes the retry budget (`failures`); a
/// clean finish — including a cancellation — never does. Firestore has no atomic
/// `failures = failures + 1`, but the counter is now read inside the same precondition-guarded
/// round as the write, so a concurrent change invalidates the commit instead of racing it.
pub(crate) fn finish_job(
    rest: &Rest,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
    fence: Option<DateTime<Utc>>,
) -> Result<JobFinish> {
    let want = fence.map(fmt_ts);
    for _ in 0..5 {
        let Some(doc) = doc_by_id(rest, id)? else {
            return Ok(JobFinish::NoSuchJob);
        };
        let (name, update_time) = doc_handle(&doc);
        let fields = decode_doc(&doc);
        let current = freq(&fields, "status")?;
        let claimed_at = fstr(&fields, "claimed_at");
        let refused = job_is_terminal(&current)
            || want
                .as_ref()
                .is_some_and(|w| claimed_at.as_deref() != Some(w.as_str()));
        if refused {
            return Ok(JobFinish::NotHeld {
                status: current,
                claimed_at: claimed_at.as_deref().map(parse_ts).transpose()?,
            });
        }

        let mut m = Fields::new();
        m.insert("status".into(), json!(status));
        m.insert("result".into(), json!(json_or_null_str(result)?));
        m.insert("error".into(), json!(error));
        m.insert("updated_at".into(), json!(fmt_ts(Utc::now())));
        let mut mask: Vec<&str> = vec!["status", "result", "error", "updated_at"];
        if error.is_some() {
            m.insert(
                "failures".into(),
                json!(fi64(&fields, "failures").unwrap_or(0) + 1),
            );
            mask.push("failures");
        }
        if rest.commit_update(&name, &m, &mask, Some(&update_time))? {
            return Ok(JobFinish::Finished);
        }
        // The doc changed between the read and the write — which may be exactly the takeover this
        // is guarding against. Re-read and re-decide.
    }
    Err(StoreError::Conflict(format!(
        "job '{id}' changed under every finish attempt; retry"
    )))
}

/// A document's `(name, updateTime)` — the handle a precondition-guarded commit needs.
fn doc_handle(doc: &Value) -> (String, String) {
    let get = |k: &str| {
        doc.get(k)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    };
    (get("name"), get("updateTime"))
}

/// Ask a job to stop: `queued` → `cancelled`, `running` → `cancelling` (which neither the queued nor
/// the stale-reclaim query matches, so a cancelled run is never restarted). The flip is guarded by
/// the document's `updateTime`, so a claim landing at the same moment loses or wins cleanly — on a
/// lost race we re-read and decide again against the new status.
pub(crate) fn cancel_job(
    rest: &Rest,
    project: Option<&str>,
    id: &str,
) -> Result<Option<JobCancel>> {
    for _ in 0..5 {
        let Some(doc) = doc_by_id(rest, id)? else {
            return Ok(None);
        };
        if !crate::scope::allows(project, fstr(&decode_doc(&doc), "project_id").as_deref()) {
            return Ok(None); // not this tenant's job: indistinguishable from no such job
        }
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let update_time = doc
            .get("updateTime")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let fields = decode_doc(&doc);
        let status = freq(&fields, "status")?;
        let (next, outcome) = match status.as_str() {
            "queued" => ("cancelled", JobCancel::Cancelled),
            "running" => ("cancelling", JobCancel::Cancelling),
            other => {
                return Ok(Some(JobCancel::AlreadyFinished {
                    status: other.into(),
                }))
            }
        };
        let mut m = Fields::new();
        m.insert("status".into(), json!(next));
        m.insert("updated_at".into(), json!(fmt_ts(Utc::now())));
        if rest.commit_update(&name, &m, &["status", "updated_at"], Some(&update_time))? {
            return Ok(Some(outcome));
        }
    }
    // Never report a cancel that didn't land as if it had — that is the one lie this endpoint
    // must not tell.
    Err(StoreError::Conflict(format!(
        "job '{id}' changed under every cancel attempt; retry"
    )))
}

fn doc_by_id(rest: &Rest, id: &str) -> Result<Option<Value>> {
    let filters: Vec<(&str, &str, Value)> = vec![("id", "EQUAL", json!(id))];
    Ok(rest
        .query_raw("jobs", &filters, None, Some(1))?
        .into_iter()
        .next())
}

/// Claim the oldest `queued` (or stale `running`) job atomically: read a candidate, then commit the
/// `running` flip guarded by the doc's `updateTime`. A lost race fails the precondition → re-query the
/// next candidate (which now skips the just-claimed one). A few rounds handle contention; single
/// workers always win first try.
pub(crate) fn claim_job(
    rest: &Rest,
    stale_before: DateTime<Utc>,
    kinds: &[&str],
) -> Result<Option<Job>> {
    let now = fmt_ts(Utc::now());
    let stale = fmt_ts(stale_before);

    for _ in 0..5 {
        let candidate = match oldest_queued(rest, kinds)? {
            Some(d) => Some(d),
            None => oldest_stale(rest, &stale, kinds)?,
        };
        let Some(doc) = candidate else {
            return Ok(None);
        };
        let name = doc
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let update_time = doc
            .get("updateTime")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        let fields = decode_doc(&doc);
        let attempts = fi64(&fields, "attempts").unwrap_or(0) + 1;
        // Reclaiming a `running` job means its worker never finished: a WORKER DEATH, counted apart
        // from `failures` (the retry budget) and stamped into `error`, so the job row distinguishes
        // "the worker was killed" from "the benchmark failed".
        let reclaimed = fields.get("status").and_then(Value::as_str) == Some("running");
        let stale_reclaims = fi64(&fields, "stale_reclaims").unwrap_or(0) + i64::from(reclaimed);

        let mut claim = Fields::new();
        claim.insert("status".into(), json!("running"));
        claim.insert("claimed_at".into(), json!(now.clone()));
        claim.insert("updated_at".into(), json!(now.clone()));
        claim.insert("attempts".into(), json!(attempts));
        claim.insert("stale_reclaims".into(), json!(stale_reclaims));
        if reclaimed {
            claim.insert("error".into(), json!(JOB_ERROR_WORKER_LOST));
        }

        let mut mask = vec![
            "status",
            "claimed_at",
            "updated_at",
            "attempts",
            "stale_reclaims",
        ];
        if reclaimed {
            mask.push("error");
        }
        if rest.commit_update(&name, &claim, &mask, Some(&update_time))? {
            let mut job = from_fields(&fields)?;
            job.status = "running".into();
            job.claimed_at = Some(parse_ts(&now)?);
            job.updated_at = parse_ts(&now)?;
            job.attempts = attempts as u32;
            job.stale_reclaims = stale_reclaims as u32;
            if reclaimed {
                job.error = Some(JOB_ERROR_WORKER_LOST.to_string());
            }
            return Ok(Some(job));
        }
        // precondition failed: another worker won this one — loop and pick the next.
    }
    Ok(None)
}

/// The worker's capability declaration as a query filter, or nothing when it declared none.
///
/// Firestore's `IN` caps at 30 values, which the five-kind vocabulary is nowhere near, and pairing
/// it with the existing `status`/`created_at` predicates needs a composite index in a real project
/// (the emulator builds one on demand) — the same operational requirement the status+created_at
/// query already carries.
fn kind_filter(kinds: &[&str]) -> Option<(&'static str, &'static str, Value)> {
    (!kinds.is_empty()).then(|| ("type", "IN", json!(kinds)))
}

fn oldest_queued(rest: &Rest, kinds: &[&str]) -> Result<Option<Value>> {
    let mut filters: Vec<(&str, &str, Value)> = vec![("status", "EQUAL", json!("queued"))];
    filters.extend(kind_filter(kinds));
    Ok(rest
        .query_raw("jobs", &filters, Some(("created_at", false)), Some(1))?
        .into_iter()
        .next())
}

fn oldest_stale(rest: &Rest, stale: &str, kinds: &[&str]) -> Result<Option<Value>> {
    // status == running AND claimed_at < stale. (No orderBy: avoids the inequality-order constraint.)
    let mut filters: Vec<(&str, &str, Value)> = vec![
        ("status", "EQUAL", json!("running")),
        ("claimed_at", "LESS_THAN", json!(stale)),
    ];
    filters.extend(kind_filter(kinds));
    Ok(rest
        .query_raw("jobs", &filters, None, Some(1))?
        .into_iter()
        .next())
}

fn to_fields(j: &Job) -> Result<Fields> {
    let mut m = Fields::new();
    m.insert("id".into(), json!(j.id));
    m.insert("type".into(), json!(j.job_type));
    m.insert("payload".into(), json!(json_or_null_str(&j.payload)?));
    m.insert("status".into(), json!(j.status));
    m.insert("attempts".into(), json!(j.attempts as i64));
    m.insert("max_attempts".into(), json!(j.max_attempts as i64));
    m.insert("progress".into(), json!(j.progress));
    m.insert("error".into(), json!(j.error));
    m.insert("result".into(), json!(json_or_null_str(&j.result)?));
    m.insert("claimed_at".into(), json!(j.claimed_at.map(fmt_ts)));
    m.insert("failures".into(), json!(j.failures as i64));
    m.insert("stale_reclaims".into(), json!(j.stale_reclaims as i64));
    m.insert("created_at".into(), json!(fmt_ts(j.created_at)));
    m.insert("updated_at".into(), json!(fmt_ts(j.updated_at)));
    m.insert("project_id".into(), json!(j.project_id));
    Ok(m)
}

fn from_fields(m: &Fields) -> Result<Job> {
    Ok(Job {
        id: freq(m, "id")?,
        job_type: freq(m, "type")?,
        payload: fjson(m, "payload")?,
        status: freq(m, "status")?,
        attempts: fi64(m, "attempts").unwrap_or(0) as u32,
        max_attempts: fi64(m, "max_attempts").unwrap_or(3) as u32,
        progress: fstr(m, "progress"),
        error: fstr(m, "error"),
        result: fjson(m, "result")?,
        claimed_at: match fstr(m, "claimed_at") {
            Some(s) => Some(parse_ts(&s)?),
            None => None,
        },
        created_at: parse_ts(&freq(m, "created_at")?)?,
        updated_at: parse_ts(&freq(m, "updated_at")?)?,
        failures: fi64(m, "failures").unwrap_or(0) as u32,
        stale_reclaims: fi64(m, "stale_reclaims").unwrap_or(0) as u32,
        project_id: fstr(m, "project_id"),
    })
}
