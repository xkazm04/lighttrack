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
//!   LIGHTTRACK_REDACT_INGEST  unset           → **redact every project** (the default; see D14)
//!                             `off`/`0`       → disabled: client text is stored verbatim
//!                             `all`/`*`/`1`   → redact every project (the default, stated)
//!                             `p1,p2,…`       → redact only these project_ids
//!
//! **The default scrubs.** It used to be `off`, which meant an operator who never read this file ran
//! an instance that stored raw prompts — emails, card numbers, whatever the application sent — and
//! found out at the compliance questionnaire. An observability tool's unset state should be the one
//! that cannot create a liability; an operator who *wants* raw text says so (`off`), which is a
//! decision someone made rather than one nobody made. The cost is real and accepted: the scrub is a
//! regex pass over every captured payload on ingest, and a false positive silently mangles content
//! (an order id shaped like a card number becomes `<CARD>`), so debugging a mangled payload requires
//! knowing this is on — which is why [`Redactor::log_posture`] says so at every boot.
//!
//! The PII scrub is heuristic (the same regex pass used for dataset building); free-text PII
//! (names, places) is out of scope here — use the runner's optional LLM scrub for datasets.

use std::collections::HashSet;

use serde_json::{json, Value};

use lighttrack_core::{LlmEvent, Redaction};

pub(crate) const ENV_REDACT: &str = "LIGHTTRACK_REDACT_INGEST";

enum Mode {
    Off,
    All,
    Projects(HashSet<String>),
}

pub(crate) struct Redactor {
    mode: Mode,
    /// Whether the mode came from an unset env var. Only used to phrase the startup line: "you are
    /// scrubbing because you asked" and "you are scrubbing because nobody said otherwise" are
    /// different things to an operator reading logs after an upgrade changed the default.
    defaulted: bool,
}

impl Redactor {
    pub(crate) fn from_env() -> Self {
        Self::from_raw(std::env::var(ENV_REDACT).ok().as_deref())
    }

    /// The env parse, as a pure function of the raw value — `None` for unset. Split out so the
    /// default (the part that decides whether an unconfigured instance stores PII) is testable
    /// without mutating process-global state from a parallel test harness.
    fn from_raw(raw: Option<&str>) -> Self {
        let t = raw.unwrap_or_default().trim().to_string();
        // An exported-but-empty value is a deployment accident (`FOO=${MISSING}` in a compose file),
        // not an instruction to store raw PII — it takes the safe default like a truly unset var.
        if t.is_empty() {
            return Self { mode: Mode::All, defaulted: true };
        }
        let mode = if t.eq_ignore_ascii_case("off") || t == "0" {
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
        Self { mode, defaulted: false }
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
        let base = match &self.mode {
            Mode::Off => "off".to_string(),
            Mode::All => "all projects".to_string(),
            Mode::Projects(set) => format!("{} project(s)", set.len()),
        };
        if self.defaulted {
            format!("{base} (default)")
        } else {
            base
        }
    }

    /// Say, at every boot, what this instance does to client text before storing it.
    ///
    /// This exists because the default flipped (D14): an operator upgrading an instance that has been
    /// storing raw prompts gets a behavior change they did not ask for, and the only honest way to
    /// ship that is to make it impossible to miss in the logs. `off` is the loud case in the other
    /// direction — it is a deliberate choice, but it is also the configuration that puts raw customer
    /// PII in your database, so it warns rather than informs.
    pub(crate) fn log_posture(&self) {
        match (&self.mode, self.defaulted) {
            (Mode::Off, _) => tracing::warn!(
                redact = "off",
                env = ENV_REDACT,
                "PII scrubbing on ingest is OFF: captured input/output/error/tags are stored exactly \
                 as the client sent them, PII included. Unset {ENV_REDACT} to scrub every project.",
            ),
            (Mode::All, true) => tracing::info!(
                redact = "all",
                env = ENV_REDACT,
                defaulted = true,
                "PII scrubbing on ingest is ON for every project (the default since v0.1: {ENV_REDACT} \
                 is unset). Emails, cards, SSNs, IPs, phones and secrets are replaced with markers \
                 before storage. Set {ENV_REDACT}=off to store client text verbatim.",
            ),
            (Mode::All, false) => tracing::info!(
                redact = "all",
                env = ENV_REDACT,
                defaulted = false,
                "PII scrubbing on ingest is ON for every project ({ENV_REDACT}=all).",
            ),
            (Mode::Projects(set), _) => {
                let mut names: Vec<&str> = set.iter().map(String::as_str).collect();
                names.sort_unstable();
                tracing::warn!(
                    redact = "projects",
                    env = ENV_REDACT,
                    projects = %names.join(","),
                    "PII scrubbing on ingest is ON for {} named project(s) only — every OTHER project \
                     stores client text verbatim. Unset {ENV_REDACT} to scrub all of them.",
                    names.len(),
                )
            }
        }
    }

    /// Scrub structured PII in place from every client-supplied free-text surface of the event —
    /// captured `input`/`output`, the `error` string, and `tags` — when redaction is enabled for its
    /// project. (`error` and `tags` previously bypassed the scrub, contradicting this module's
    /// "raw PII never lands in the DB" promise: a provider error message happily echoes the request
    /// content, including whatever PII it carried.) Returns the number of spans redacted.
    ///
    /// `persistence` is the policy [`apply_policy`] already ran, and it decides whether the payloads
    /// are still worth scrubbing. Under `hash`/`drop` they are no longer client text — they are a
    /// digest, or gone — and scrubbing a digest is actively **destructive**: a sha256 hex string
    /// matches the scrubber's "32+ hex characters is a secret" rule, so every hashed payload would
    /// collapse to the same `<SECRET>` marker and the `hash` policy's whole promise (presence and
    /// change-detection without content) would silently evaporate. Before the default flipped this
    /// only bit operators who opted into both; now it would be the default pairing, so it is closed.
    /// `error` and `tags` are scrubbed either way — no persistence policy covers them.
    pub(crate) fn redact_event(&self, ev: &mut LlmEvent, persistence: Redaction) -> usize {
        if !self.enabled_for(&ev.project_id) {
            return 0;
        }
        let mut n = 0;
        if matches!(persistence, Redaction::None) {
            if let Some(input) = ev.input.as_mut() {
                n += scrub_value(input);
            }
            if let Some(output) = ev.output.as_mut() {
                n += scrub_value(output);
            }
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
        Self { mode: Mode::Off, defaulted: false }
    }
    /// Test constructor: redact every project.
    pub(crate) fn all() -> Self {
        Self { mode: Mode::All, defaulted: false }
    }
    /// Test constructor: exactly what an operator who configured nothing gets. Distinct from
    /// [`Redactor::all`] so an end-to-end test asserts the *default*, not a posture it opted into.
    pub(crate) fn defaulted() -> Self {
        Self::from_raw(None)
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
    fn an_unconfigured_instance_scrubs_and_only_an_explicit_off_stores_raw_pii() {
        // The default is the whole point of D14: an operator who never read redact.rs must not be
        // storing raw prompts. `from_raw(None)` is exactly what `from_env` sees when the var is unset.
        for unset in [None, Some(""), Some("   ")] {
            let r = Redactor::from_raw(unset);
            assert!(r.enabled_for("anything"), "unset/blank must scrub: {unset:?}");
            assert!(r.defaulted, "{unset:?}");
            assert_eq!(r.describe(), "all projects (default)");
            let mut ev = event("p1", json!("mail jane@example.com"), json!("clean"));
            assert!(r.redact_event(&mut ev, Redaction::None) > 0);
            assert!(!serde_json::to_string(&ev).unwrap().contains("jane@example.com"));
        }

        // The opt-out still exists and still means off — an operator who wants raw text keeps it.
        for off in ["off", "OFF", " Off ", "0"] {
            let r = Redactor::from_raw(Some(off));
            assert!(!r.enabled_for("anything"), "explicit off must not scrub: {off}");
            assert!(!r.defaulted);
            assert_eq!(r.describe(), "off");
            let mut ev = event("p1", json!("mail jane@example.com"), json!("clean"));
            assert_eq!(r.redact_event(&mut ev, Redaction::None), 0, "{off}");
            assert_eq!(ev.input, Some(json!("mail jane@example.com")));
        }

        // `all` spelled out is the same posture, just not defaulted (the startup line differs).
        for all in ["all", "ALL", "*", "1"] {
            let r = Redactor::from_raw(Some(all));
            assert!(r.enabled_for("anything"), "{all}");
            assert_eq!(r.describe(), "all projects");
        }

        // A CSV still narrows to the named projects — the scoped posture is unchanged by the flip.
        let r = Redactor::from_raw(Some("p1, p2"));
        assert!(r.enabled_for("p1") && r.enabled_for("p2"));
        assert!(!r.enabled_for("p3"));
        assert_eq!(r.describe(), "2 project(s)");
    }

    #[test]
    fn from_env_reads_the_real_variable() {
        // `from_raw` is where the logic lives, but nothing would catch a `from_env` that read the
        // wrong key or dropped the unset case. Both env states are exercised in ONE test so no other
        // test in this binary can observe the mutation half-applied.
        let saved = std::env::var(ENV_REDACT).ok();
        std::env::remove_var(ENV_REDACT);
        assert!(Redactor::from_env().enabled_for("p1"), "unset env must scrub");
        std::env::set_var(ENV_REDACT, "off");
        assert!(!Redactor::from_env().enabled_for("p1"), "env=off must not scrub");
        match saved {
            Some(v) => std::env::set_var(ENV_REDACT, v),
            None => std::env::remove_var(ENV_REDACT),
        }
    }

    #[test]
    fn a_project_persistence_policy_still_overrides_the_env_floor() {
        // Layer 1 runs before layer 2 and is unaffected by the default flip: `drop` still removes
        // payloads outright, and `none` still stores what the scrub left (not what the client sent).
        let scrubbing = Redactor::from_raw(None);

        let mut dropped = event("p1", json!("jane@example.com"), json!("x"));
        assert!(apply_policy(&mut dropped, Redaction::Drop));
        assert_eq!(scrubbing.redact_event(&mut dropped, Redaction::Drop), 0, "nothing left to scrub");
        assert!(dropped.input.is_none() && dropped.output.is_none());

        let mut hashed = event("p1", json!("jane@example.com"), json!("x"));
        assert!(apply_policy(&mut hashed, Redaction::Hash));
        assert_eq!(scrubbing.redact_event(&mut hashed, Redaction::Hash), 0);
        assert!(hashed.input.unwrap().get("sha256").is_some());

        // `none` + the new default = scrubbed, which is the behavior change D14 describes.
        let mut kept = event("p1", json!("jane@example.com"), json!("x"));
        assert!(!apply_policy(&mut kept, Redaction::None));
        assert!(scrubbing.redact_event(&mut kept, Redaction::None) > 0);
        assert_eq!(kept.input, Some(json!("<EMAIL>")));
    }

    #[test]
    fn redacts_strings_nested() {
        let r = Redactor::all();
        let mut ev = event(
            "p1",
            json!({ "q": "email me at jane@example.com" }),
            json!("call +1 (415) 555-2671 or card 4111 1111 1111 1111"),
        );
        let n = r.redact_event(&mut ev, Redaction::None);
        assert!(n >= 3, "redactions={n}");
        let blob = serde_json::to_string(&ev).unwrap();
        assert!(!blob.contains("jane@example.com"), "{blob}");
        assert!(blob.contains("<EMAIL>"), "{blob}");
        assert!(!blob.contains("4111"), "{blob}");
    }

    #[test]
    fn error_and_tags_are_scrubbed_too() {
        let r = Redactor::all();
        let mut ev = event("p1", json!("clean"), json!("clean"));
        ev.error = Some("upstream 400: invalid email jane@example.com in prompt".to_string());
        ev.tags = vec!["cust:jane@example.com".to_string(), "clean-tag".to_string()];
        let n = r.redact_event(&mut ev, Redaction::None);
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
        let off = Redactor::off();
        let mut ev = event("p1", json!("jane@example.com"), json!("clean"));
        assert_eq!(off.redact_event(&mut ev, Redaction::None), 0);
        assert_eq!(ev.input, Some(json!("jane@example.com")));

        // Scoped → only the listed project is redacted.
        let scoped = Redactor::from_raw(Some("p1"));
        let mut a = event("p1", json!("jane@example.com"), json!("x"));
        let mut b = event("p2", json!("jane@example.com"), json!("x"));
        assert!(scoped.redact_event(&mut a, Redaction::None) > 0);
        assert_eq!(scoped.redact_event(&mut b, Redaction::None), 0);
    }
}
