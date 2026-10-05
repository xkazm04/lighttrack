//! The one authority on what a lease answer means.
//!
//! Every ownership decision this agent makes — keep running, stop, abandon, retry — is made here,
//! from the HTTP status and the typed body, and nowhere else. What it replaces was a single
//! `e.to_string().contains("HTTP 409")` run against an error string the HTTP module had formatted
//! itself *with the response body interpolated into it*, consulted from two call sites. So a proxy
//! page that happened to mention 409 abandoned a healthy multi-hour run, and a reworded server
//! error would have silently changed the policy — message matching is classification by
//! superstition, and the structured evidence (a status code, a tagged `LeaseHeld`) was sitting at
//! the boundary and being thrown away one line earlier.
//!
//! The rule the shape encodes: **the fallback is `Unknown`, never a decision.** A failure the
//! taxonomy does not recognise must leave a live run alone, because "I could not tell" and "you
//! lost the lease" have opposite correct responses and only one of them is reversible.

use chrono::{DateTime, Utc};

use lighttrack_core::LeaseHeld;

use crate::cloud::Answer;

/// What a conditioned write against the lease actually told this device to do. A closed set: every
/// boundary answer maps into exactly one of these, and the run loop consumes the class.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Verdict {
    /// Still ours, nobody has asked us to stop. `deadline` is the lease's new expiry when the call
    /// extended it.
    Held { deadline: Option<DateTime<Utc>> },
    /// Still ours, and an operator has asked this run to **stop**. The lease is kept — so the run
    /// settles honestly — but the irreversible half must not happen.
    Stop { reason: String },
    /// Somebody else holds the record now. Do not deliver, and do not settle: the result belongs to
    /// a run the cloud has already handed away.
    Reclaimed { status: Option<String> },
    /// There is no such task any more. Same settlement as `Reclaimed`, a different fact.
    NoSuchTask,
    /// Could not tell. The only safe reading of an unrecognised failure: keep running and try the
    /// next renewal.
    Unknown { err: String },
}

/// Classify one lease answer. Takes the transport result too, so there is no path by which a caller
/// can reach a decision without coming through here.
pub(crate) fn classify(answer: anyhow::Result<Answer>) -> Verdict {
    match answer {
        Ok(a) => from_http(a.status, &a.body),
        // A transport failure is evidence about the network, never about ownership — whatever its
        // text happens to say.
        Err(e) => Verdict::Unknown {
            err: format!("{e:#}"),
        },
    }
}

fn from_http(status: u16, body: &str) -> Verdict {
    let held = serde_json::from_str::<LeaseHeld>(body).ok();
    match status {
        200..=299 => match held {
            // The stop request rides the renewal because that is the only channel that reaches a
            // running device (see `LeaseHeld::Held::cancel_requested`).
            Some(LeaseHeld::Held {
                cancel_requested: true,
                ..
            }) => Verdict::Stop {
                reason: "an operator cancelled this task".to_string(),
            },
            Some(LeaseHeld::Held { deadline, .. }) => Verdict::Held { deadline },
            // A success whose body is not a lease verdict — the settle door answers with the task
            // itself — still means the write landed, which is the only thing being asked.
            _ => Verdict::Held { deadline: None },
        },
        // Read what the record holds now from the TYPED body when the server sends one. A server
        // that wraps the refusal in a generic error envelope leaves `status` unknown; that is a
        // missing field, not a licence to go parsing prose for it.
        409 => Verdict::Reclaimed {
            status: match held {
                Some(LeaseHeld::NotHeld { status, .. }) => Some(status),
                _ => None,
            },
        },
        404 => Verdict::NoSuchTask,
        other => Verdict::Unknown {
            err: format!("HTTP {other}: {}", truncate(body)),
        },
    }
}

/// Error text is for a human to read in a log line, so it is bounded — a proxy's HTML page must not
/// become the device's console.
fn truncate(s: &str) -> String {
    const MAX: usize = 300;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    let head: String = s.chars().take(MAX).collect();
    format!("{head}… ({} bytes)", s.len())
}

impl Verdict {
    /// The log line this verdict earns, for the one place that prints it.
    pub(crate) fn describe(&self) -> String {
        match self {
            Verdict::Held { .. } => "lease renewed".to_string(),
            Verdict::Stop { reason } => reason.clone(),
            Verdict::Reclaimed { status } => match status {
                Some(s) => format!("the record now holds '{s}'"),
                None => "the cloud refused the write as no longer ours".to_string(),
            },
            Verdict::NoSuchTask => "the task no longer exists".to_string(),
            Verdict::Unknown { err } => err.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().expect("timestamp")
    }

    /// The whole table, in one place, because the point of the module is that there is only one.
    #[test]
    fn the_status_and_the_body_decide_and_nothing_else_does() {
        // A healthy renewal: keep running, and record the deadline the server actually granted.
        assert_eq!(
            from_http(
                200,
                r#"{"outcome":"held","deadline":"2026-10-05T12:00:00Z"}"#
            ),
            Verdict::Held {
                deadline: Some(at("2026-10-05T12:00:00Z"))
            }
        );
        // Today's bytes from a server that has never heard of the flag: unchanged behaviour.
        assert_eq!(
            from_http(200, r#"{"outcome":"held"}"#),
            Verdict::Held { deadline: None }
        );
        // The cancel, arriving on the one channel that reaches a running device.
        assert!(matches!(
            from_http(200, r#"{"outcome":"held","cancel_requested":true}"#),
            Verdict::Stop { .. }
        ));
        // A 409 carrying a typed body names what the record holds now, read as a field.
        assert_eq!(
            from_http(409, r#"{"outcome":"not_held","status":"queued"}"#),
            Verdict::Reclaimed {
                status: Some("queued".into())
            }
        );
        // A 409 wrapped in a generic error envelope is still a reclaim — with the status missing,
        // which is honest, rather than scraped out of the message.
        assert_eq!(
            from_http(
                409,
                r#"{"error":{"code":"conflict","message":"is queued"}}"#
            ),
            Verdict::Reclaimed { status: None }
        );
        assert_eq!(from_http(404, "{}"), Verdict::NoSuchTask);
        // The settle door answers with the task, not a lease verdict: the write still landed.
        assert_eq!(
            from_http(200, r#"{"id":"t-1","status":"succeeded"}"#),
            Verdict::Held { deadline: None }
        );
    }

    /// The defect this module exists to kill. A 502 whose BODY mentions 409 — a proxy page echoed
    /// into the error — used to abandon a healthy run, because the classifier was a substring test
    /// against a string the HTTP module had formatted with that body inside it.
    #[test]
    fn a_fake_substring_is_not_an_ownership_decision() {
        let v = from_http(
            502,
            "<html><body>upstream returned HTTP 409 Conflict</body></html>",
        );
        assert!(
            matches!(v, Verdict::Unknown { .. }),
            "a 5xx is 'I could not tell', whatever its body says: got {v:?}"
        );
        // Same for the transport path: the error text is not evidence about ownership.
        let v = classify(Err(anyhow::anyhow!(
            "POST /v1/relay/tasks/t1/renew -> HTTP 409: connection reset"
        )));
        assert!(matches!(v, Verdict::Unknown { .. }), "got {v:?}");
        // And an unrecognised status defaults to Unknown rather than to either decision.
        for s in [301, 401, 403, 418, 500, 503] {
            assert!(
                matches!(
                    from_http(s, r#"{"outcome":"not_held","status":"x"}"#),
                    Verdict::Unknown { .. }
                ),
                "HTTP {s} must not decide ownership"
            );
        }
    }

    #[test]
    fn a_runaway_body_cannot_become_the_console() {
        let huge = "x".repeat(10_000);
        let Verdict::Unknown { err } = from_http(500, &huge) else {
            panic!("a 500 is Unknown");
        };
        assert!(err.len() < 400, "{} chars", err.len());
        assert!(err.contains("10000 bytes"));
    }
}
