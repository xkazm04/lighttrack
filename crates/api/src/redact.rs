//! Server-side payload redaction on ingest — two layers, applied in order:
//!
//! **1. Per-project persistence policy** ([`apply_policy`]): the project's stored
//! [`Redaction`] setting — `none` (store as sent), `hash` (store only a sha256 of each payload:
//! presence/diff without content), `drop` (never persist payloads). This is the policy the projects
//! API accepts and the operator table displays; it is resolved per event from the policy cache in
//! `AppState` (see `state::redaction_policy_for`) and enforced here on the ingest path.
//!
//! **2. PII scrub** ([`Redactor::redact_event`]): scrubs structured PII (emails, cards, SSNs,
//! secrets, IPs, phones — the `lighttrack_anon` regex pass) from captured `input`/`output` **and**
//! the `error` string and `tags` (all client-supplied free text) before storage. Config is
//! server-global via env and acts as a floor under the per-project policy:
//!   LIGHTTRACK_REDACT_INGEST  unset/`off`/`0` → disabled (default)
//!                             `all`/`*`/`1`   → redact every project
//!                             `p1,p2,…`       → redact only these project_ids
//!
//! The PII scrub is heuristic (the same regex pass used for dataset building); free-text PII
//! (names, places) is out of scope here — use the runner's optional LLM scrub for datasets.

use std::collections::HashSet;

use serde_json::{json, Value};

use lighttrack_core::{LlmEvent, Redaction};

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

    /// Scrub structured PII in place from every client-supplied free-text surface of the event —
    /// captured `input`/`output`, the `error` string, and `tags` — when redaction is enabled for its
    /// project. (`error` and `tags` previously bypassed the scrub, contradicting this module's
    /// "raw PII never lands in the DB" promise: a provider error message happily echoes the request
    /// content, including whatever PII it carried.) Returns the number of spans redacted.
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
        if let Some(error) = ev.error.as_mut() {
            n += scrub_string(error);
        }
        for tag in ev.tags.iter_mut() {
            n += scrub_string(tag);
        }
        n
    }
}

/// Enforce a project's persistence policy on the event's captured payloads, in place. Returns `true`
/// when the payloads were transformed (hash/drop applied to at least one present payload). Runs
/// BEFORE the PII scrub: `drop` removes the payloads outright, `hash` leaves nothing scrubbable.
pub(crate) fn apply_policy(ev: &mut LlmEvent, policy: Redaction) -> bool {
    match policy {
        Redaction::None => false,
        Redaction::Hash => {
            let mut applied = false;
            for payload in [&mut ev.input, &mut ev.output] {
                if let Some(v) = payload.as_ref() {
                    // Hash the canonical JSON serialization: presence + change-detection without
                    // content — exactly what the `Redaction::Hash` doc comment promises.
                    let digest = crate::auth::sha256_hex(&v.to_string());
                    *payload = Some(json!({ "sha256": digest }));
                    applied = true;
                }
            }
            applied
        }
        Redaction::Drop => {
            let had = ev.input.is_some() || ev.output.is_some();
            ev.input = None;
            ev.output = None;
            had
        }
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

/// Scrub one plain string in place; returns the redaction count.
fn scrub_string(s: &mut String) -> usize {
    let r = lighttrack_anon::scrub(s);
    if r.redactions > 0 {
        *s = r.text;
    }
    r.redactions
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
    fn error_and_tags_are_scrubbed_too() {
        let r = Redactor { mode: Mode::All };
        let mut ev = event("p1", json!("clean"), json!("clean"));
        ev.error = Some("upstream 400: invalid email jane@example.com in prompt".to_string());
        ev.tags = vec!["cust:jane@example.com".to_string(), "clean-tag".to_string()];
        let n = r.redact_event(&mut ev);
        assert!(n >= 2, "redactions={n}");
        assert!(!ev.error.as_deref().unwrap().contains("jane@example.com"));
        assert!(ev.error.as_deref().unwrap().contains("<EMAIL>"));
        assert!(!ev.tags[0].contains("jane@example.com"));
        assert_eq!(ev.tags[1], "clean-tag");
    }

    #[test]
    fn policy_hash_replaces_payloads_with_digests() {
        let mut ev = event("p1", json!({ "q": "secret prompt" }), json!("secret answer"));
        assert!(apply_policy(&mut ev, Redaction::Hash));
        let input = ev.input.clone().unwrap();
        let output = ev.output.clone().unwrap();
        let ih = input.get("sha256").and_then(Value::as_str).expect("input digest");
        let oh = output.get("sha256").and_then(Value::as_str).expect("output digest");
        assert_eq!(ih.len(), 64);
        assert_ne!(ih, oh, "different payloads hash differently");
        let blob = serde_json::to_string(&ev).unwrap();
        assert!(!blob.contains("secret"), "no plaintext survives hashing: {blob}");
        // Same payload → same digest (presence/diff semantics).
        let mut ev2 = event("p1", json!({ "q": "secret prompt" }), json!("x"));
        apply_policy(&mut ev2, Redaction::Hash);
        assert_eq!(ev2.input.unwrap().get("sha256").and_then(Value::as_str).unwrap(), ih);
    }

    #[test]
    fn policy_drop_removes_payloads_and_none_is_a_noop() {
        let mut ev = event("p1", json!("secret"), json!("secret"));
        assert!(apply_policy(&mut ev, Redaction::Drop));
        assert!(ev.input.is_none() && ev.output.is_none());
        // Drop on an already-empty event reports nothing to do.
        assert!(!apply_policy(&mut ev, Redaction::Drop));

        let mut ev = event("p1", json!("as sent"), json!("as sent"));
        assert!(!apply_policy(&mut ev, Redaction::None));
        assert_eq!(ev.input, Some(json!("as sent")));
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
