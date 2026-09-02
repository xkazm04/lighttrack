//! `list_schedules` — the recurring-workload table.
//!
//! The view that answers "what runs on a schedule here", which before M7 had no answer at all: the
//! four non-benchmark workloads recurred by a process being kept alive with `--interval`, and the
//! fifth by a key hidden inside a benchmark's `target`.

use serde_json::Value;

use crate::md::{opt_s, s, short_ts, trunc, u, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No schedules — nothing recurs on this instance._".to_string());
    }
    let mut t = Table::new(&[
        ("", Align::Left),
        ("Kind", Align::Left),
        ("Every", Align::Right),
        ("Next due", Align::Left),
        ("Target", Align::Left),
        ("Project", Align::Left),
        ("Schedule id", Align::Left),
    ]);
    for r in rows {
        let enabled = r.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        t.row(vec![
            // A paused schedule is still listed — an operator has to be able to see the thing they
            // paused — so it needs a glyph that reads as "off", not as "missing".
            if enabled { "●" } else { "○" }.to_string(),
            s(r, "kind").to_string(),
            every(u(r, "interval_secs")),
            if enabled {
                short_ts(s(r, "next_due"))
            } else {
                "paused".into()
            },
            target(r.get("payload")),
            trunc(s(r, "project_id"), 20),
            s(r, "id").to_string(),
        ]);
    }
    Some(t.render())
}

/// A human interval: the number an operator recognises, not 86400 seconds.
fn every(secs: u64) -> String {
    match secs {
        0 => "—".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3_600 == 0 => format!("{}h", s / 3_600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// What the schedule actually acts on, pulled out of the kind-specific payload — a table of five
/// identical `bench_run` rows tells an operator nothing.
fn target(payload: Option<&Value>) -> String {
    let Some(p) = payload else {
        return "—".into();
    };
    for key in ["benchmark_id", "project", "file"] {
        if let Some(v) = p.get(key).and_then(Value::as_str).filter(|v| !v.is_empty()) {
            return trunc(v, 24);
        }
    }
    opt_s(p, "name_prefix")
        .filter(|v| !v.is_empty())
        .map(|v| trunc(v, 24))
        .unwrap_or_else(|| "—".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn intervals_read_as_humans_write_them() {
        assert_eq!(every(86_400), "1d");
        assert_eq!(every(7_200), "2h");
        assert_eq!(every(300), "5m");
        assert_eq!(every(90), "90s");
        assert_eq!(every(0), "—");
    }

    #[test]
    fn the_target_column_names_what_each_kind_acts_on() {
        assert_eq!(target(Some(&json!({ "benchmark_id": "b-1" }))), "b-1");
        assert_eq!(target(Some(&json!({ "project": "proj-a" }))), "proj-a");
        assert_eq!(
            target(Some(&json!({ "file": "golden.jsonl" }))),
            "golden.jsonl"
        );
        assert_eq!(
            target(Some(&json!({ "name_prefix": "nightly" }))),
            "nightly"
        );
        assert_eq!(target(Some(&json!({}))), "—");
        assert_eq!(target(None), "—");
    }

    #[test]
    fn a_paused_schedule_is_shown_as_paused_not_omitted() {
        let out = list(&json!([
            { "id": "s1", "project_id": "p", "kind": "bench_run", "interval_secs": 3600,
              "next_due": "2026-09-02T10:00:00.000000000Z", "enabled": true,
              "payload": { "benchmark_id": "b-1" } },
            { "id": "s2", "project_id": "p", "kind": "score_traces", "interval_secs": 86400,
              "next_due": "2026-09-02T10:00:00.000000000Z", "enabled": false,
              "payload": { "project": "proj-a" } }
        ]))
        .expect("renders");
        assert!(out.contains("bench_run"), "{out}");
        assert!(out.contains("score_traces"), "{out}");
        assert!(
            out.contains("paused"),
            "a disabled schedule must say so: {out}"
        );
        assert!(out.contains("b-1") && out.contains("proj-a"), "{out}");
    }

    #[test]
    fn an_empty_list_says_nothing_recurs_rather_than_rendering_a_bare_header() {
        let out = list(&json!([])).expect("renders");
        assert!(out.contains("No schedules"), "{out}");
        assert!(list(&json!({})).is_none(), "a non-array is not a listing");
    }
}
