//! Labels and judge trust (M11) — the human verdicts a judge is calibrated against, and the
//! answer to "may I believe the judge behind this green badge?".
//!
//! `lt judges trust` is the one an operator reaches for before trusting a gate: it answers
//! `trusted` / `untrusted` / `unknown` for one `(rubric, judge)` pair, and `unknown` is a real
//! answer — a judge nobody has measured has taken no check, not failed one.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Cli, JudgesCmd, LabelsCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &LabelsCmd) -> Result<()> {
    match action {
        LabelsCmd::List {
            project,
            subject,
            rubric_id,
            limit,
        } => {
            let mut path = format!("/v1/labels?limit={limit}");
            push(&mut path, "project", project.as_deref());
            push(&mut path, "subject", subject.as_deref());
            push(&mut path, "rubric_id", rubric_id.as_deref());
            call(cli, Method::GET, &path, None, "list_labels")
        }
        LabelsCmd::Add {
            project,
            subject,
            value,
            pass,
            rubric_id,
            labeler,
            note,
        } => {
            if !(0.0..=1.0).contains(value) {
                bail!("--value must be in 0..1 (the scale a judge verdict normalizes to)");
            }
            let body = build_body(
                project.as_deref(),
                subject,
                *value,
                *pass,
                rubric_id.as_deref(),
                labeler,
                note.as_deref(),
            );
            call(cli, Method::POST, "/v1/labels", Some(body), "list_labels")
        }
    }
}

pub(crate) fn run_judges(cli: &Cli, action: &JudgesCmd) -> Result<()> {
    match action {
        JudgesCmd::Trust {
            judge,
            project,
            rubric_id,
        } => {
            let mut path = format!("/v1/judges/trust?judge={}", urlencode(judge));
            push(&mut path, "project", project.as_deref());
            push(&mut path, "rubric_id", rubric_id.as_deref());
            call(cli, Method::GET, &path, None, "get_judge_trust")
        }
        JudgesCmd::History { project, limit } => {
            let mut path = format!("/v1/calibrations?limit={limit}");
            push(&mut path, "project", project.as_deref());
            call(cli, Method::GET, &path, None, "list_calibrations")
        }
    }
}

/// Pure so the "omitted flags send nothing" rule is testable — a `null` in the body would be a
/// deliberate erasure rather than an omission.
fn build_body(
    project: Option<&str>,
    subject: &str,
    value: f64,
    pass: Option<bool>,
    rubric_id: Option<&str>,
    labeler: &str,
    note: Option<&str>,
) -> Value {
    let mut b = json!({ "subject": subject, "value": value, "labeler": labeler });
    for (k, v) in [
        ("project_id", project),
        ("rubric_id", rubric_id),
        ("note", note),
    ] {
        if let Some(v) = v {
            b[k] = json!(v);
        }
    }
    if let Some(p) = pass {
        b["pass"] = json!(p);
    }
    b
}

fn push(path: &mut String, key: &str, value: Option<&str>) {
    if let Some(v) = value.filter(|s| !s.is_empty()) {
        path.push_str(&format!("&{key}={}", urlencode(v)));
    }
}

/// A judge is `provider/model` and a subject is `kind:id`, so neither can be pasted into a query
/// string raw.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_omitted_flag_sends_nothing_rather_than_null() {
        let b = build_body(None, "event:e1", 0.9, None, None, "me", None);
        assert_eq!(b["subject"], "event:e1");
        assert_eq!(b["labeler"], "me");
        assert!(b.get("project_id").is_none(), "{b}");
        assert!(b.get("pass").is_none(), "{b}");
        assert!(b.get("note").is_none(), "{b}");
    }

    #[test]
    fn an_explicit_false_pass_is_sent_not_dropped() {
        let b = build_body(
            Some("p1"),
            "score:s1",
            0.9,
            Some(false),
            Some("rb"),
            "me",
            Some("n"),
        );
        assert_eq!(b["pass"], false);
        assert_eq!(b["project_id"], "p1");
        assert_eq!(b["rubric_id"], "rb");
        assert_eq!(b["note"], "n");
    }

    /// A `/` in a judge name would otherwise land in the query string as a path character and the
    /// lookup would silently miss.
    #[test]
    fn query_values_are_encoded() {
        let mut p = "/v1/judges/trust?judge=x".to_string();
        push(&mut p, "subject", Some("event:e1"));
        push(&mut p, "project", Some(""));
        assert!(p.ends_with("&subject=event%3Ae1"), "{p}");
        assert!(!p.contains("project"), "an empty flag sends nothing: {p}");
        assert_eq!(urlencode("anthropic/haiku"), "anthropic%2Fhaiku");
    }
}
