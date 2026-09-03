//! Labels and judge trust (M11) — the human verdicts a judge is calibrated against, and the
//! answer to "may I believe the judge behind this green badge?".
//!
//! `lt judges trust` is the one an operator reaches for before trusting a gate: it answers
//! `trusted` / `untrusted` / `unknown` for one `(rubric, judge)` pair, and `unknown` is a real
//! answer — a judge nobody has measured has taken no check, not failed one.

use std::fs;

use anyhow::{bail, Context, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Cli, JudgesCmd, LabelsCmd};
use crate::http::call;
use crate::query::encode as urlencode;

pub(crate) fn run(cli: &Cli, action: &LabelsCmd) -> Result<()> {
    match action {
        LabelsCmd::List {
            project,
            subject,
            rubric_id,
            limit,
            cursor,
        } => {
            let mut path = format!("/v1/labels?limit={limit}");
            push(&mut path, "project", project.as_deref());
            push(&mut path, "subject", subject.as_deref());
            push(&mut path, "rubric_id", rubric_id.as_deref());
            push(&mut path, "cursor", cursor.as_deref());
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
        JudgesCmd::History {
            project,
            limit,
            cursor,
        } => {
            let mut path = format!("/v1/calibrations?limit={limit}");
            push(&mut path, "project", project.as_deref());
            push(&mut path, "cursor", cursor.as_deref());
            call(cli, Method::GET, &path, None, "list_calibrations")
        }
        // The record is structured data (κ, Pearson, MAE, RMSE, n, the bar it was judged against),
        // so it comes from the file the measurement produced rather than from a wall of flags.
        JudgesCmd::Calibrate { file, project } => {
            let text = fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
            let record: Value =
                serde_json::from_str(&text).with_context(|| format!("{file}: invalid JSON"))?;
            let body = calibration_body(record, project.as_deref(), file)?;
            call(
                cli,
                Method::POST,
                "/v1/calibrations",
                Some(body),
                "list_calibrations",
            )
        }
    }
}

/// `--project` supplies the field an admin key cannot derive, and never overrides one the file
/// already carries — the record's own attribution is the auditable one.
fn calibration_body(record: Value, project: Option<&str>, file: &str) -> Result<Value> {
    let Value::Object(mut o) = record else {
        bail!("{file}: a calibration record is a JSON object");
    };
    if let Some(p) = project {
        o.entry("project_id").or_insert_with(|| json!(p));
    }
    if !o.contains_key("judge") {
        bail!("{file}: a calibration record must name the `judge` it measured");
    }
    Ok(Value::Object(o))
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

    /// A record with no judge names no half of the pair a trust verdict is about, and one whose
    /// file already says which project it belongs to must keep saying so.
    #[test]
    fn a_calibration_record_must_name_its_judge_and_keeps_its_own_project() {
        assert!(calibration_body(json!({ "kappa": 0.8 }), Some("p1"), "f").is_err());
        assert!(calibration_body(json!([]), None, "f").is_err());

        let b =
            calibration_body(json!({ "judge": "anthropic/haiku" }), Some("p1"), "f").expect("body");
        assert_eq!(b["project_id"], "p1");
        let b = calibration_body(
            json!({ "judge": "anthropic/haiku", "project_id": "from-file" }),
            Some("p1"),
            "f",
        )
        .expect("body");
        assert_eq!(b["project_id"], "from-file");
    }
}
