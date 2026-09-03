//! The worker's three calls to the queue: claim, renew, finish.
//!
//! Split from the loop that uses them because they share one property the loop does not: each is a
//! **conditioned** write whose refusal is information. A 409 on renew or finish is not a failure to
//! retry — it is the API saying this worker no longer holds the job, which is exactly what its work
//! loop needs to stop on.

use anyhow::Result;
use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use lighttrack_core::Job;

use crate::cli::Cli;
use crate::http::post;

pub(crate) fn claim(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    stale_secs: i64,
    kinds: &[String],
    providers: &[String],
) -> Result<Option<Job>> {
    let v = post(
        cli,
        http,
        "/v1/jobs/claim",
        &json!({ "stale_secs": stale_secs, "kinds": kinds, "providers": providers }),
    )?;
    if v.is_null() {
        Ok(None)
    } else {
        Ok(Some(serde_json::from_value(v)?))
    }
}

/// Extend this worker's lease, returning the new one. `Ok(None)` means the lease is no longer ours
/// (the API answered 409) - affirmative evidence of a takeover, which is why it is a distinct value
/// from `Err`, i.e. "I could not tell".
pub(crate) fn renew_lease(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
    fence: Option<DateTime<Utc>>,
) -> Result<Option<DateTime<Utc>>> {
    // No lease was stamped at claim, so there is nothing to prove and nothing to lose.
    let Some(fence) = fence else {
        return Ok(None);
    };
    match post(
        cli,
        http,
        &format!("/v1/jobs/{id}/renew"),
        &json!({ "claimed_at": fence }),
    ) {
        Ok(v) => Ok(v
            .get("claimed_at")
            .and_then(|c| serde_json::from_value::<DateTime<Utc>>(c.clone()).ok())),
        Err(e) if is_conflict(&e) => Ok(None),
        Err(e) => Err(e),
    }
}

/// Whether an API error is the 409 that means "you do not hold this any more".
pub(crate) fn is_conflict(e: &anyhow::Error) -> bool {
    e.to_string().contains("409")
}

/// Write the verdict, FENCED on the lease this worker holds. The API refuses with 409 if the job
/// moved on - which is precisely the write that used to be unconditioned, letting a worker that had
/// already been reclaimed overwrite its replacement's verdict.
#[allow(clippy::too_many_arguments)]
pub(crate) fn finish(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    id: &str,
    status: &str,
    result: &Value,
    error: Option<&str>,
    fence: Option<DateTime<Utc>>,
) -> Result<()> {
    post(
        cli,
        http,
        &format!("/v1/jobs/{id}/finish"),
        &json!({ "status": status, "result": result, "error": error, "claimed_at": fence }),
    )?;
    Ok(())
}
