//! `list_labels`, `list_calibrations` and `get_judge_trust` (M11) — the human verdict ledger and
//! whether the judge may be believed.
//!
//! The trust view leads with the verdict word and then the numbers behind it, because "untrusted"
//! on its own is not actionable: κ, the bar it missed, and the `n` it rests on are what tell an
//! operator whether to recalibrate or to change judges. `unknown` is rendered as its own line
//! rather than as a dash — it is a distinct answer, not a missing one.

use serde_json::Value;

use crate::md::{opt_b, opt_f, opt_u, pass_glyph, s, short_ts, trunc, Align, Table};

/// `{ "labels": [...], "next_cursor": … }`, or a bare array.
pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v
        .get("labels")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())?;
    if rows.is_empty() {
        return Some("_No labels._".to_string());
    }
    let mut t = Table::new(&[
        ("", Align::Left),
        ("Value", Align::Right),
        ("Subject", Align::Left),
        ("Labeler", Align::Left),
        ("Rubric", Align::Left),
        ("When", Align::Left),
        ("Note", Align::Left),
    ]);
    for r in rows {
        let subject = r.get("subject").map(subject_str).unwrap_or_default();
        t.row(vec![
            pass_glyph(opt_b(r, "pass")).to_string(),
            format!("{:.2}", opt_f(r, "value").unwrap_or(0.0)),
            trunc(&subject, 30),
            trunc(s(r, "labeler"), 22),
            trunc(s(r, "rubric_id"), 12),
            short_ts(s(r, "created_at")),
            trunc(s(r, "note"), 34),
        ]);
    }
    let mut out = format!("**{} label(s)**\n\n{}", rows.len(), t.render());
    if let Some(c) = v.get("next_cursor").and_then(Value::as_str) {
        out.push_str(&format!("\n_More: cursor `{c}`._"));
    }
    Some(out)
}

/// `{"type":"event","id":"…"}` → `event:…`, the same form the API's `subject=` filter accepts, so a
/// row can be copied straight back into a query.
fn subject_str(v: &Value) -> String {
    match (
        v.get("type").and_then(Value::as_str),
        v.get("id").and_then(Value::as_str),
    ) {
        (Some(k), Some(id)) => format!("{k}:{id}"),
        _ => String::new(),
    }
}

/// `{ "calibrations": [...] }`, or a bare array.
pub(crate) fn calibrations(v: &Value) -> Option<String> {
    let rows = v
        .get("calibrations")
        .and_then(Value::as_array)
        .or_else(|| v.as_array())?;
    if rows.is_empty() {
        return Some(
            "_No calibrations — every judge here is `unknown`, not untrusted._".to_string(),
        );
    }
    let mut t = Table::new(&[
        ("", Align::Left),
        ("κ", Align::Right),
        ("bar", Align::Right),
        ("n", Align::Right),
        ("MAE", Align::Right),
        ("Judge", Align::Left),
        ("Rubric", Align::Left),
        ("When", Align::Left),
    ]);
    for r in rows {
        t.row(vec![
            pass_glyph(opt_b(r, "trusted")).to_string(),
            format!("{:.3}", opt_f(r, "kappa").unwrap_or(0.0)),
            format!("{:.2}", opt_f(r, "kappa_bar").unwrap_or(0.0)),
            opt_u(r, "n").unwrap_or(0).to_string(),
            format!("{:.3}", opt_f(r, "mae").unwrap_or(0.0)),
            trunc(s(r, "judge"), 28),
            trunc(s(r, "rubric_id"), 14),
            short_ts(s(r, "created_at")),
        ]);
    }
    Some(format!(
        "**{} calibration(s)**\n\n{}",
        rows.len(),
        t.render()
    ))
}

/// `{ "trust": "...", "calibration": {...} | absent }`.
pub(crate) fn trust(v: &Value) -> Option<String> {
    let verdict = v.get("trust").and_then(Value::as_str)?;
    let (glyph, gloss) = match verdict {
        "trusted" => (
            "✓",
            "this judge has been checked against a human and cleared the bar",
        ),
        "untrusted" => (
            "✗",
            "this judge has been checked against a human and did NOT clear the bar",
        ),
        _ => (
            "?",
            "nobody has measured this (rubric, judge) pair — not a failed check, no check. \
             A new rubric version starts here, inheriting nothing",
        ),
    };
    let mut out = format!("{glyph} **{verdict}** — {gloss}.\n");
    let Some(c) = v.get("calibration") else {
        out.push_str("\n_Run `lt-runner calibrate --dataset <id>` to measure it._");
        return Some(out);
    };
    let mut t = Table::new(&[
        ("κ", Align::Right),
        ("bar", Align::Right),
        ("n", Align::Right),
        ("Pearson", Align::Right),
        ("MAE", Align::Right),
        ("RMSE", Align::Right),
        ("Judge", Align::Left),
        ("Measured", Align::Left),
    ]);
    t.row(vec![
        format!("{:.3}", opt_f(c, "kappa").unwrap_or(0.0)),
        format!("{:.2}", opt_f(c, "kappa_bar").unwrap_or(0.0)),
        opt_u(c, "n").unwrap_or(0).to_string(),
        format!("{:.3}", opt_f(c, "pearson").unwrap_or(0.0)),
        format!("{:.3}", opt_f(c, "mae").unwrap_or(0.0)),
        format!("{:.3}", opt_f(c, "rmse").unwrap_or(0.0)),
        trunc(s(c, "judge"), 28),
        short_ts(s(c, "created_at")),
    ]);
    out.push('\n');
    out.push_str(&t.render());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_subject_renders_in_the_form_the_filter_accepts() {
        let out = list(&json!({"labels":[{
            "id":"l1","subject":{"type":"event","id":"ev1"},"value":0.9,
            "labeler":"rev@x","created_at":"2026-01-02T03:04:05.000000000Z"
        }]}))
        .expect("rendered");
        assert!(out.contains("event:ev1"), "{out}");
        assert!(out.contains("rev@x"), "{out}");
    }

    #[test]
    fn an_empty_ledger_says_so_rather_than_rendering_an_empty_table() {
        assert!(list(&json!({"labels":[]})).unwrap().contains("No labels"));
        assert!(calibrations(&json!([])).unwrap().contains("unknown"));
    }

    /// `unknown` must read as its own answer, and must never be printed as "untrusted".
    #[test]
    fn unknown_trust_is_explained_as_no_check_rather_than_a_failed_one() {
        let out = trust(&json!({"trust":"unknown"})).expect("rendered");
        assert!(out.contains("unknown"), "{out}");
        assert!(out.contains("no check"), "{out}");
        assert!(!out.contains("untrusted"), "{out}");
        assert!(out.contains("calibrate"), "the fix is named: {out}");
    }

    #[test]
    fn a_decided_verdict_shows_the_numbers_behind_it() {
        let out = trust(&json!({
            "trust":"untrusted",
            "calibration":{"judge":"anthropic/haiku","kappa":0.12,"kappa_bar":0.6,"n":12,
                           "pearson":0.3,"mae":0.2,"rmse":0.25,
                           "created_at":"2026-01-02T03:04:05.000000000Z"}
        }))
        .expect("rendered");
        assert!(out.contains("untrusted"), "{out}");
        assert!(out.contains("0.120") && out.contains("0.60"), "{out}");
        assert!(out.contains("12"), "the sample size is load-bearing: {out}");
    }
}
