//! Optional server-side PII redaction on ingest.
//!
//! Events now carry captured `input`/`output` content (the SDKs' content capture). When enabled,
//! the ingest path scrubs structured PII (emails, cards, SSNs, secrets, IPs, phones — the
//! `lighttrack_anon` regex pass) from that content **before it is stored**, so raw PII never lands
//! in the DB. Token counts, model, usage, latency, and metadata are untouched.
//!
//! Config is server-global via env (per-project routing without a schema/`Store` change — the
//! event already carries `project_id`):
//!   LIGHTTRACK_REDACT_INGEST  unset/`off`/`0` → disabled (default)
//!                             `all`/`*`/`1`   → redact every project
//!                             `p1,p2,…`       → redact only these project_ids
//!
//! Redaction is heuristic (the same regex pass used for dataset building); free-text PII (names,
//! places) is out of scope here — use the runner's optional LLM scrub for datasets.

use std::collections::HashSet;

use serde_json::Value;

use lighttrack_core::LlmEvent;

enum Mode {
    Off,
    All,
    Projects(HashSet<String>),
}

pub(crate) struct Redactor {
    mode: Mode,
}

impl Redactor {
    pub(crate) fn from_env() -> Self {
        let raw = std::env::var("LIGHTTRACK_REDACT_INGEST").unwrap_or_default();
        let t = raw.trim();
        let mode = if t.is_empty() || t.eq_ignore_ascii_case("off") || t == "0" {
            Mode::Off
        } else if t.eq_ignore_ascii_case("all") || t == "*" || t == "1" {
            Mode::All
        } else {
            Mode::Projects(
                t.split(',')
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .collect(),
            )
        };
        Self { mode }
    }

    fn enabled_for(&self, project: &str) -> bool {
        match &self.mode {
            Mode::Off => false,
            Mode::All => true,
            Mode::Projects(set) => set.contains(project),
        }
    }

    /// One-line summary for the startup banner.
    pub(crate) fn describe(&self) -> String {
        match &self.mode {
            Mode::Off => "off".to_string(),
            Mode::All => "all projects".to_string(),
            Mode::Projects(set) => format!("{} project(s)", set.len()),
        }
    }

    /// Scrub structured PII from the event's captured `input`/`output` in place when redaction is
    /// enabled for its project. Returns the number of spans redacted (0 if disabled or clean).
    pub(crate) fn redact_event(&self, ev: &mut LlmEvent) -> usize {
        if !self.enabled_for(&ev.project_id) {
            return 0;
        }
        let mut n = 0;
        if let Some(input) = ev.input.as_mut() {
            n += scrub_value(input);
        }
        if let Some(output) = ev.output.as_mut() {
            n += scrub_value(output);
        }
        n
    }
}

#[cfg(test)]
impl Redactor {
    /// Test constructor: redaction disabled.
    pub(crate) fn off() -> Self {
        Self { mode: Mode::Off }
    }
    /// Test constructor: redact every project.
    pub(crate) fn all() -> Self {
        Self { mode: Mode::All }
    }
}

/// Recursively scrub every string leaf of a JSON value, preserving structure. Returns the total
/// redaction count.
fn scrub_value(v: &mut Value) -> usize {
    match v {
        Value::String(s) => {
            let r = lighttrack_anon::scrub(s);
            if r.redactions > 0 {
                *s = r.text;
            }
            r.redactions
        }
        Value::Array(arr) => arr.iter_mut().map(scrub_value).sum(),
        Value::Object(map) => map.values_mut().map(scrub_value).sum(),
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn event(project: &str, input: Value, output: Value) -> LlmEvent {
        serde_json::from_value(json!({
            "project_id": project,
            "provider": "openai",
            "model": "gpt-4o",
            "input": input,
            "output": output,
        }))
        .unwrap()
    }

    #[test]
    fn redacts_strings_nested() {
        let r = Redactor { mode: Mode::All };
        let mut ev = event(
            "p1",
            json!({ "q": "email me at jane@example.com" }),
            json!("call +1 (415) 555-2671 or card 4111 1111 1111 1111"),
        );
        let n = r.redact_event(&mut ev);
        assert!(n >= 3, "redactions={n}");
        let blob = serde_json::to_string(&ev).unwrap();
        assert!(!blob.contains("jane@example.com"), "{blob}");
        assert!(blob.contains("<EMAIL>"), "{blob}");
        assert!(!blob.contains("4111"), "{blob}");
    }

    #[test]
    fn disabled_and_scoped() {
        // Off → nothing touched.
        let off = Redactor { mode: Mode::Off };
        let mut ev = event("p1", json!("jane@example.com"), json!("clean"));
        assert_eq!(off.redact_event(&mut ev), 0);
        assert_eq!(ev.input, Some(json!("jane@example.com")));

        // Scoped → only the listed project is redacted.
        let scoped = Redactor { mode: Mode::Projects(["p1".to_string()].into_iter().collect()) };
        let mut a = event("p1", json!("jane@example.com"), json!("x"));
        let mut b = event("p2", json!("jane@example.com"), json!("x"));
        assert!(scoped.redact_event(&mut a) > 0);
        assert_eq!(scoped.redact_event(&mut b), 0);
    }
}
