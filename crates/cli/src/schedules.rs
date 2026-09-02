//! `lt schedules` and `lt jobs` — the two views onto the work this instance does on its own.
//!
//! `schedules` answers "what recurs here", which before M7 required reading five daemons' command
//! lines and one field buried in a benchmark's `target`. `jobs` answers "what is it doing right
//! now", including the four workloads that only became jobs in M7.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Map, Value};

use crate::cli::{Cli, JobsCmd, SchedulesCmd};
use crate::http::call;

/// Every job kind the API accepts. Checked client-side so a typo costs a round trip, not a stored
/// schedule that can only ever enqueue rejects.
const KINDS: &[&str] = &[
    "bench_run",
    "score_events",
    "score_traces",
    "dataset_sample",
    "calibrate",
];

pub(crate) fn run(cli: &Cli, action: &SchedulesCmd) -> Result<()> {
    match action {
        SchedulesCmd::List { project } => call(
            cli,
            Method::GET,
            &match project {
                Some(p) => format!("/v1/projects/{p}/schedules"),
                None => "/v1/schedules".to_string(),
            },
            None,
            "list_schedules",
        ),
        SchedulesCmd::Create {
            project,
            kind,
            every,
            payload,
            start_in_secs,
            paused,
        } => {
            check_kind(kind)?;
            let mut body = Map::new();
            body.insert("type".into(), json!(kind));
            body.insert("interval_secs".into(), json!(parse_every(every)?));
            body.insert("payload".into(), parse_payload(payload.as_deref())?);
            body.insert("start_in_secs".into(), json!(start_in_secs));
            body.insert("enabled".into(), json!(!paused));
            call(
                cli,
                Method::POST,
                &format!("/v1/projects/{project}/schedules"),
                Some(Value::Object(body)),
                "get_schedule",
            )
        }
        SchedulesCmd::Set {
            id,
            every,
            payload,
            enable,
            disable,
        } => {
            let mut body = Map::new();
            if let Some(e) = every {
                body.insert("interval_secs".into(), json!(parse_every(e)?));
            }
            if let Some(p) = payload {
                body.insert("payload".into(), parse_payload(Some(p))?);
            }
            if *enable || *disable {
                body.insert("enabled".into(), json!(*enable));
            }
            if body.is_empty() {
                bail!("nothing to change: pass --every, --payload, --enable or --disable");
            }
            call(
                cli,
                Method::PUT,
                &format!("/v1/schedules/{id}"),
                Some(Value::Object(body)),
                "get_schedule",
            )
        }
        SchedulesCmd::Delete { id } => call(
            cli,
            Method::DELETE,
            &format!("/v1/schedules/{id}"),
            None,
            "delete_schedule",
        ),
        SchedulesCmd::Runs { id } => call(
            cli,
            Method::GET,
            &format!("/v1/schedules/{id}/runs"),
            None,
            "list_jobs",
        ),
    }
}

pub(crate) fn run_jobs(cli: &Cli, action: &JobsCmd) -> Result<()> {
    match action {
        JobsCmd::List { status, limit } => {
            let mut q = format!("/v1/jobs?limit={limit}");
            if let Some(s) = status {
                q.push_str(&format!("&status={s}"));
            }
            call(cli, Method::GET, &q, None, "list_jobs")
        }
        JobsCmd::Show { id } => call(cli, Method::GET, &format!("/v1/jobs/{id}"), None, "get_job"),
        JobsCmd::Enqueue { kind, payload } => {
            check_kind(kind)?;
            call(
                cli,
                Method::POST,
                "/v1/jobs",
                Some(json!({ "type": kind, "payload": parse_payload(payload.as_deref())? })),
                "get_job",
            )
        }
        JobsCmd::Cancel { id } => call(
            cli,
            Method::POST,
            &format!("/v1/jobs/{id}/cancel"),
            Some(json!({})),
            "cancel_job",
        ),
    }
}

fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.contains(&kind) {
        bail!(
            "unknown job kind '{kind}': expected one of {}",
            KINDS.join(" | ")
        );
    }
    Ok(())
}

/// Parse a human interval (`30m`, `6h`, `1d`, or bare seconds) into seconds.
///
/// Bare seconds stay accepted so scripts that already pass a number keep working; the suffixes exist
/// because "86400" is how a schedule gets typed wrong.
pub(crate) fn parse_every(s: &str) -> Result<u64> {
    let s = s.trim();
    let (num, mult) = match s.chars().last() {
        Some('s') => (&s[..s.len() - 1], 1),
        Some('m') => (&s[..s.len() - 1], 60),
        Some('h') => (&s[..s.len() - 1], 3_600),
        Some('d') => (&s[..s.len() - 1], 86_400),
        _ => (s, 1),
    };
    let n: u64 = num.parse().map_err(|_| {
        anyhow::anyhow!("bad interval '{s}': expected e.g. 30m, 6h, 1d, or seconds")
    })?;
    if n == 0 {
        bail!("interval must be greater than zero");
    }
    Ok(n * mult)
}

/// The payload as JSON. Empty is an empty object, not null: every kind's payload is an object, and
/// `null` would be refused by the API for a reason the operator did not cause.
fn parse_payload(raw: Option<&str>) -> Result<Value> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(json!({})),
        Some(s) => {
            let v: Value = serde_json::from_str(s)
                .map_err(|e| anyhow::anyhow!("--payload is not valid JSON: {e}"))?;
            if !v.is_object() {
                bail!("--payload must be a JSON object, e.g. '{{\"benchmark_id\":\"b-1\"}}'");
            }
            Ok(v)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn intervals_accept_the_units_an_operator_actually_types() {
        assert_eq!(parse_every("30m").unwrap(), 1_800);
        assert_eq!(parse_every("6h").unwrap(), 21_600);
        assert_eq!(parse_every("1d").unwrap(), 86_400);
        assert_eq!(parse_every("90s").unwrap(), 90);
        // A bare number is still seconds, so existing scripts keep working.
        assert_eq!(parse_every("3600").unwrap(), 3_600);
        assert!(parse_every("0").is_err());
        assert!(parse_every("soon").is_err());
    }

    #[test]
    fn a_payload_is_an_object_or_a_clear_refusal() {
        assert_eq!(parse_payload(None).unwrap(), json!({}));
        assert_eq!(parse_payload(Some("  ")).unwrap(), json!({}));
        assert_eq!(
            parse_payload(Some(r#"{"benchmark_id":"b-1"}"#)).unwrap(),
            json!({ "benchmark_id": "b-1" })
        );
        assert!(
            parse_payload(Some("[1,2]")).is_err(),
            "an array is not a payload"
        );
        assert!(parse_payload(Some("{oops")).is_err());
    }

    #[test]
    fn an_unknown_kind_is_refused_before_the_round_trip() {
        assert!(check_kind("bench_run").is_ok());
        let e = check_kind("bench-run").unwrap_err().to_string();
        assert!(
            e.contains("bench_run"),
            "the refusal must name what IS accepted: {e}"
        );
    }
}
