//! Server-side payload redaction on ingest — two layers, applied in order:
//!
//! **1. Per-project persistence policy** ([`apply_policy`]): the project's stored
//! [`Redaction`] setting — `none` (store as sent), `hash` (store only a sha256 of each payload:
//! presence/diff without content), `drop` (never persist payloads). This is the policy the projects
//! API accepts and the operator table displays; it is resolved per event from the policy cache in
//! `AppState` (see `state::project_policy_for`) and enforced here on the ingest path.
//!
//! **2. PII scrub** ([`Redactor::redact_event`]): scrubs structured PII (emails, cards, SSNs,
//! secrets, IPs, phones — the `lighttrack_anon` regex pass) from **every client-supplied surface**
//! before storage — captured `input`/`output`, the `error` string, `tags`, `name`, `source`, and
//! `metadata` (except the accounting keys enumerated in `METADATA_PASSTHROUGH`, each with its
//! reason). Nothing a caller can write is left un-dispositioned: an emit-site inventory with a field
//! missing from it is an inventory that has not been taken. Config is
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

use lighttrack_core::{LlmEvent, Redaction, RedactionStamp, REDACTION_KEY};

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
            return Self {
                mode: Mode::All,
                defaulted: true,
            };
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
        Self {
            mode,
            defaulted: false,
        }
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
        // Which rule set is doing it. The posture line said *that* text is rewritten but never *by
        // what*, and the rules have already changed shape once — so an operator comparing two
        // instances, or a database written across an upgrade, had no version to compare. This is the
        // same fingerprint stamped on every scrubbed row, so a log line and a row can be joined.
        if !matches!(self.mode, Mode::Off) {
            tracing::info!(
                rules = %lighttrack_anon::rules_fingerprint(),
                "PII scrub rule set in force; the same fingerprint is stamped on every scrubbed row \
                 (metadata.redaction.rules) and grouped by GET /v1/projects/:id/redaction",
            );
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
        // Server-owned, exactly like `api_key_id`: whatever a caller sent under the reserved key is
        // removed *before* anything else, so a client can never claim to have been scrubbed. Done
        // here rather than at the call sites because every door that scrubs must also stamp — a
        // stamp a door could forget is a stamp an operator cannot trust the absence of.
        strip_client_stamp(ev);
        if !self.enabled_for(&ev.project_id) {
            // A row nobody scrubbed still gets a stamp. `scrub: false` is a *decision recorded*;
            // no stamp at all is a row nobody can account for, and the whole point of M9 is that
            // the two stop looking alike.
            write_stamp(ev, persistence, false, 0);
            return 0;
        }
        // The scrubber's own failure is not consent to send. If anything in the walk panics — a
        // shape that defeats an assumption, a stack overflow the depth cap somehow missed — the
        // event must NOT fall back to the un-scrubbed original, which is the tempting default and
        // the one that turns a boundary failure into a disclosure. It falls back to a payload-free
        // record, and the failure is logged at `error` because a scrubber that has started panicking
        // is a boundary that has silently stopped existing.
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scrub_all(ev, persistence)));
        match outcome {
            Ok(n) => {
                // AFTER the walk, never before: the scrub rewrites `metadata`, and a stamp written
                // first would be scrubbed by the rules it is describing (a 12-hex fingerprint is
                // not far off the "32+ hex is a secret" rule's shape, and the policy string is
                // client-looking text).
                write_stamp(ev, persistence, true, n as u32);
                n
            }
            Err(_) => {
                ev.input = Some(json!("<REDACTION FAILED: payload discarded>"));
                ev.output = Some(json!("<REDACTION FAILED: payload discarded>"));
                ev.error = Some("<REDACTION FAILED>".to_string());
                ev.tags.clear();
                ev.metadata = Value::Null;
                // The stamp survives the discard: a row whose scrub panicked is a row the boundary
                // acted on, and the posture report must show it as scrubbed-with-nothing-left
                // rather than as one of the unaccounted-for rows.
                write_stamp(ev, persistence, true, 0);
                tracing::error!(
                    project_id = %ev.project_id,
                    event_id = %ev.id,
                    "PII scrub PANICKED; payloads were discarded rather than stored unscrubbed"
                );
                0
            }
        }
    }
}

/// Remove any client-sent value under the reserved [`REDACTION_KEY`]. Same discipline as
/// `api_key_id`: the field is server-owned, so a body carrying it is scrubbed of it, not trusted.
fn strip_client_stamp(ev: &mut LlmEvent) {
    if let Value::Object(map) = &mut ev.metadata {
        map.remove(REDACTION_KEY);
    }
}

/// Write this row's [`RedactionStamp`] into `metadata`, creating the object if the event had none.
///
/// A non-object `metadata` (a bare string or array, which the API accepts) is *not* clobbered — it
/// is moved under `value` so the stamp has somewhere to live without destroying client data. That
/// shape is vanishingly rare; silently dropping a payload to make room for provenance would be a
/// worse trade than a slightly odd envelope.
fn write_stamp(ev: &mut LlmEvent, policy: Redaction, scrub: bool, spans: u32) {
    let stamp = RedactionStamp {
        policy,
        scrub,
        spans,
        rules: if scrub {
            lighttrack_anon::rules_fingerprint().to_string()
        } else {
            String::new()
        },
    };
    let Ok(value) = serde_json::to_value(&stamp) else {
        return;
    };
    match &mut ev.metadata {
        Value::Object(map) => {
            map.insert(REDACTION_KEY.to_string(), value);
        }
        Value::Null => {
            ev.metadata = json!({ REDACTION_KEY: value });
        }
        other => {
            let kept = std::mem::replace(other, Value::Null);
            ev.metadata = json!({ "value": kept, REDACTION_KEY: value });
        }
    }
}

/// Server-owned or accounting keys inside `metadata` that pass the scrub **un-rewritten**, each with
/// its "passed, because…". These are join keys, not payloads: rewriting one does not protect anyone,
/// it silently merges or splits the buckets every cost, margin and budget number is grouped by.
///
/// * `api_key_id` — server-stamped from the authenticated principal (never read from the body); an
///   opaque `api_keys.id`, not key material.
/// * `customer_id` — the billing linkage `margin`/`cost_by_dimension` group on. If it were scrubbed,
///   every customer whose id happens to look like an email would collapse into one `<EMAIL>` bucket
///   and their spend would merge. An operator who uses a real address as a customer id should send a
///   pseudonym; that is a caller-side decision, and this is where it is written down.
/// * `product_id` — same, for per-product attribution.
/// * `cost_source` — a closed vocabulary (`client` | `book`) the server stamps.
/// * `pricing_mode` — a closed vocabulary the price book resolves against.
/// * `redaction` — the server's own [`RedactionStamp`] (M9). Written after the walk, so the scrub
///   never sees it in practice; listed here so a re-scrub of an already-stamped row cannot collapse
///   its rule fingerprint into `<SECRET>` and destroy the only record of what happened to the row.
const METADATA_PASSTHROUGH: [&str; 7] = [
    "api_key_id",
    "customer_id",
    "product_id",
    "cost_source",
    "pricing_mode",
    // A relay run's prompt fingerprint (M19), server-computed from a device report. It is 64 hex
    // characters, so the "32+ hex is a secret" rule would collapse every one of them to the same
    // `<SECRET>` — and a fingerprint that is identical for every row is not a fingerprint. Exactly
    // the reasoning that already exempts the `hash` persistence policy's digests.
    "prompt_sha256",
    REDACTION_KEY,
];

/// Every client-supplied surface, scrubbed. Split out of [`Redactor::redact_event`] so the whole
/// walk sits inside one panic guard.
fn scrub_all(ev: &mut LlmEvent, persistence: Redaction) -> usize {
    let mut n = 0;
    let mut capped = 0;
    if matches!(persistence, Redaction::None) {
        for payload in [&mut ev.input, &mut ev.output] {
            if let Some(v) = payload.as_mut() {
                let (r, c) = scrub_value_capped(v);
                n += r;
                capped += c;
            }
        }
    }
    if let Some(error) = ev.error.as_mut() {
        n += scrub_string(error);
    }
    for tag in ev.tags.iter_mut() {
        n += scrub_string(tag);
    }
    // `name` and `source` are client-set free text. Their disposition used to be "not mentioned",
    // which is not a disposition. Scrubbed: a legitimate call-site label ("summarize-email") matches
    // no PII rule and is untouched, and if one ever does contain an address, a fragmented rollup is
    // the lesser harm.
    if let Some(name) = ev.name.as_mut() {
        n += scrub_string(name);
    }
    if let Some(source) = ev.source.as_mut() {
        n += scrub_string(source);
    }
    // `metadata` is ARBITRARY client JSON and was never scrubbed at all — the largest hole in this
    // module's "raw PII never lands in the DB" promise, because it is the field applications
    // actually use for per-call context. It is scrubbed now, except the accounting keys above.
    if let Value::Object(map) = &mut ev.metadata {
        for (key, value) in map.iter_mut() {
            if METADATA_PASSTHROUGH.contains(&key.as_str()) {
                continue;
            }
            let (r, c) = scrub_value_capped(value);
            n += r;
            capped += c;
        }
    } else if !ev.metadata.is_null() {
        // A non-object `metadata` (a bare string or array) is still client text.
        let (r, c) = scrub_value_capped(&mut ev.metadata);
        n += r;
        capped += c;
    }
    if capped > 0 {
        tracing::warn!(
            project_id = %ev.project_id,
            event_id = %ev.id,
            dropped = capped,
            "payload hit a redaction traversal cap; the un-inspected parts were DROPPED (see the \
             `<UNSCANNED: …>` markers). A cap firing routinely means that field wants an explicit \
             shape, not a bigger limit."
        );
    }
    n
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
        Self {
            mode: Mode::Off,
            defaulted: false,
        }
    }
    /// Test constructor: redact every project.
    pub(crate) fn all() -> Self {
        Self {
            mode: Mode::All,
            defaulted: false,
        }
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

// ---- traversal caps -------------------------------------------------------------------------
//
// The walker below runs on an ingest path SECURITY.md names as attacker-reachable, over JSON the
// caller chose. It used to have no caps at all: any depth, any breadth, any string length, any total
// size. Two failure modes, and the second is the one that matters:
//
//   * cost — a deeply nested or enormous payload turns every ingest into an unbounded walk;
//   * disclosure — WHICH WAY a capped walker fails is the whole difference between a privacy
//     boundary and a formatter. A pretty-printer that hits its depth limit prints an ellipsis and
//     lets the rest through; its worst outcome is an ugly page. A REDACTOR that does the same has
//     emitted the one region it never inspected, and the selection is not random: object graphs
//     nest deepest exactly where they are richest.
//
// So: at every cap, DROP, and say which cap fired. An inspected-and-passed value is a decision; an
// uninspected-and-passed value is a disclosure. The numbers are the least interesting part and are
// set generously — a legitimate LLM payload should never meet one, and a cap that starts firing
// routinely is a signal that the field wants an explicit shape, not a bigger number.
//
// Two branches this technique normally demands are genuinely absent here, and are recorded rather
// than skipped: `serde_json::Value` is an acyclic tree (no cycle guard is possible or needed) and
// its variants are enumerated (there is no "exotic value" default branch that could pass an
// un-inspected foreign type through).

/// Nesting depth beyond which a subtree is dropped rather than walked.
const MAX_DEPTH: usize = 12;
/// Children walked per array/object. The rest are shed as one marker carrying the count.
const MAX_BREADTH: usize = 512;
/// Bytes of a single string leaf that are scrubbed. The prefix is inspected and kept; the tail is
/// dropped with a marker, because forwarding an un-scanned tail is the disclosure this guards.
const MAX_STRING_BYTES: usize = 32 * 1024;
/// Total nodes any one payload may consume. A wide-and-shallow payload evades depth and breadth
/// caps individually; this is the ceiling on the walk as a whole.
const MAX_NODES: usize = 20_000;

/// Marker for a value the boundary **did not inspect**, naming the cap that fired.
///
/// Deliberately not the same word as the scrubber's own `<EMAIL>` / `<SECRET>` markers: "nothing
/// sensitive here" and "I could not look" must not read alike. An investigator seeing
/// `<UNSCANNED: depth>` at a path knows there is a blind spot exactly there.
fn unscanned(cap: &str) -> Value {
    Value::String(format!("<UNSCANNED: {cap}>"))
}

/// One payload's traversal budget and tally.
struct Walk {
    nodes_left: usize,
    redactions: usize,
    /// How many values were dropped un-inspected. Non-zero means this event has blind spots.
    capped: usize,
}

/// Recursively scrub every string leaf of a JSON value, preserving structure, **within bounded
/// depth, breadth, string length and total node count** — dropping (never forwarding) whatever it
/// could not inspect. Returns the redaction count and the number of un-inspected drops.
fn scrub_value_capped(v: &mut Value) -> (usize, usize) {
    let mut w = Walk {
        nodes_left: MAX_NODES,
        redactions: 0,
        capped: 0,
    };
    walk(v, 0, &mut w);
    (w.redactions, w.capped)
}

fn walk(v: &mut Value, depth: usize, w: &mut Walk) {
    if depth >= MAX_DEPTH {
        *v = unscanned("depth");
        w.capped += 1;
        return;
    }
    if w.nodes_left == 0 {
        *v = unscanned("node-budget");
        w.capped += 1;
        return;
    }
    w.nodes_left -= 1;

    match v {
        Value::String(s) => {
            if s.len() > MAX_STRING_BYTES {
                // Inspect what fits and drop the rest. Splitting on a char boundary keeps the
                // prefix valid UTF-8; the marker names how much was shed so the size is not a
                // silent truncation.
                let mut cut = MAX_STRING_BYTES;
                while cut > 0 && !s.is_char_boundary(cut) {
                    cut -= 1;
                }
                let shed = s.len() - cut;
                let mut head = s[..cut].to_string();
                w.redactions += scrub_string(&mut head);
                *s = format!("{head}<UNSCANNED: string, {shed} bytes dropped>");
                w.capped += 1;
            } else {
                w.redactions += scrub_string(s);
            }
        }
        Value::Array(arr) => {
            if arr.len() > MAX_BREADTH {
                let shed = arr.len() - MAX_BREADTH;
                arr.truncate(MAX_BREADTH);
                arr.push(unscanned(&format!("breadth, {shed} entries dropped")));
                w.capped += 1;
            }
            for item in arr.iter_mut() {
                walk(item, depth + 1, w);
            }
        }
        Value::Object(map) => {
            if map.len() > MAX_BREADTH {
                let shed = map.len() - MAX_BREADTH;
                // Positional: `retain` visits in the map's own order, so the first MAX_BREADTH
                // entries survive without cloning and re-scanning the key set for each one.
                let mut seen = 0;
                map.retain(|_, _| {
                    seen += 1;
                    seen <= MAX_BREADTH
                });
                map.insert(
                    "<UNSCANNED>".to_string(),
                    unscanned(&format!("breadth, {shed} keys dropped")),
                );
                w.capped += 1;
            }
            for value in map.values_mut() {
                walk(value, depth + 1, w);
            }
        }
        // Null / Bool / Number carry no free text and are enumerated, not defaulted: `Value` has no
        // other variants, so nothing reaches this arm by being unfamiliar.
        _ => {}
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
            assert!(
                r.enabled_for("anything"),
                "unset/blank must scrub: {unset:?}"
            );
            assert!(r.defaulted, "{unset:?}");
            assert_eq!(r.describe(), "all projects (default)");
            let mut ev = event("p1", json!("mail jane@example.com"), json!("clean"));
            assert!(r.redact_event(&mut ev, Redaction::None) > 0);
            assert!(!serde_json::to_string(&ev)
                .unwrap()
                .contains("jane@example.com"));
        }

        // The opt-out still exists and still means off — an operator who wants raw text keeps it.
        for off in ["off", "OFF", " Off ", "0"] {
            let r = Redactor::from_raw(Some(off));
            assert!(
                !r.enabled_for("anything"),
                "explicit off must not scrub: {off}"
            );
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
        assert!(
            Redactor::from_env().enabled_for("p1"),
            "unset env must scrub"
        );
        std::env::set_var(ENV_REDACT, "off");
        assert!(
            !Redactor::from_env().enabled_for("p1"),
            "env=off must not scrub"
        );
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
        assert_eq!(
            scrubbing.redact_event(&mut dropped, Redaction::Drop),
            0,
            "nothing left to scrub"
        );
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
        let mut ev = event(
            "p1",
            json!({ "q": "secret prompt" }),
            json!("secret answer"),
        );
        assert!(apply_policy(&mut ev, Redaction::Hash));
        let input = ev.input.clone().unwrap();
        let output = ev.output.clone().unwrap();
        let ih = input
            .get("sha256")
            .and_then(Value::as_str)
            .expect("input digest");
        let oh = output
            .get("sha256")
            .and_then(Value::as_str)
            .expect("output digest");
        assert_eq!(ih.len(), 64);
        assert_ne!(ih, oh, "different payloads hash differently");
        let blob = serde_json::to_string(&ev).unwrap();
        assert!(
            !blob.contains("secret"),
            "no plaintext survives hashing: {blob}"
        );
        // Same payload → same digest (presence/diff semantics).
        let mut ev2 = event("p1", json!({ "q": "secret prompt" }), json!("x"));
        apply_policy(&mut ev2, Redaction::Hash);
        assert_eq!(
            ev2.input
                .unwrap()
                .get("sha256")
                .and_then(Value::as_str)
                .unwrap(),
            ih
        );
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

    // ---- traversal caps: what the boundary does when it cannot look ----------------------------

    /// Build `depth` nested objects with the PII at the bottom.
    fn nest(depth: usize, leaf: Value) -> Value {
        (0..depth).fold(leaf, |acc, _| json!({ "n": acc }))
    }

    #[test]
    fn a_payload_deeper_than_the_cap_is_dropped_not_forwarded() {
        // The direction rule, and the whole reason this is not a formatter: a walker that hits its
        // depth limit and lets the subtree through has emitted the ONE region it never inspected —
        // and object graphs nest deepest exactly where they are richest.
        let r = Redactor::all();
        let mut ev = event(
            "p1",
            nest(MAX_DEPTH + 5, json!("mail jane@example.com")),
            json!("ok"),
        );
        r.redact_event(&mut ev, Redaction::None);
        let stored = serde_json::to_string(&ev).unwrap();
        assert!(
            !stored.contains("jane@example.com"),
            "an un-inspected deep subtree must never reach storage: {stored}"
        );
        assert!(
            stored.contains("<UNSCANNED: depth>"),
            "and the reader must be told there is a blind spot at that path: {stored}"
        );
    }

    #[test]
    fn a_cap_marker_is_not_spelled_like_a_redaction() {
        // "nothing sensitive here" and "I could not look" must not read alike, or an investigator
        // reading the record can conclude nothing from a marker.
        let r = Redactor::all();
        let mut ev = event(
            "p1",
            json!("mail jane@example.com"),
            nest(MAX_DEPTH + 2, json!("x")),
        );
        r.redact_event(&mut ev, Redaction::None);
        let input = serde_json::to_string(&ev.input).unwrap();
        let output = serde_json::to_string(&ev.output).unwrap();
        assert!(input.contains("<EMAIL>"), "{input}");
        assert!(!input.contains("UNSCANNED"), "{input}");
        assert!(output.contains("<UNSCANNED"), "{output}");
    }

    #[test]
    fn breadth_and_the_node_budget_shed_the_tail_and_say_how_much() {
        let r = Redactor::all();
        let wide: Vec<Value> = (0..MAX_BREADTH + 40)
            .map(|_| json!("mail jane@example.com"))
            .collect();
        let mut ev = event("p1", json!(wide), json!("ok"));
        r.redact_event(&mut ev, Redaction::None);
        let stored = serde_json::to_string(&ev.input).unwrap();
        assert!(!stored.contains("jane@example.com"), "{stored}");
        assert!(
            stored.contains("40 entries dropped"),
            "the count is the point: {stored}"
        );

        // Wide-and-shallow evades the depth cap; the node budget is the ceiling on the walk itself.
        let huge: Vec<Value> = (0..MAX_BREADTH)
            .map(|_| json!((0..MAX_BREADTH).map(|_| json!("x")).collect::<Vec<_>>()))
            .collect();
        let mut ev = event("p1", json!(huge), json!("ok"));
        r.redact_event(&mut ev, Redaction::None);
        assert!(
            serde_json::to_string(&ev.input)
                .unwrap()
                .contains("<UNSCANNED: node-budget>"),
            "a payload wide enough to exhaust the budget must be capped, not walked forever"
        );
    }

    #[test]
    fn an_oversized_string_keeps_its_inspected_prefix_and_drops_the_rest() {
        let r = Redactor::all();
        let mut long = "mail jane@example.com ".to_string();
        long.push_str(&"a".repeat(MAX_STRING_BYTES));
        long.push_str(" bob@example.com"); // past the cap: never inspected, must not survive
        let mut ev = event("p1", json!(long), json!("ok"));
        r.redact_event(&mut ev, Redaction::None);
        let stored = serde_json::to_string(&ev.input).unwrap();
        assert!(
            stored.contains("<EMAIL>"),
            "the inspected prefix is still scrubbed: {}",
            &stored[..80]
        );
        assert!(
            !stored.contains("bob@example.com"),
            "the un-inspected tail is dropped"
        );
        assert!(
            stored.contains("bytes dropped"),
            "and the cut is marked with its size"
        );
    }

    // ---- emit-site inventory: every field a caller can write ------------------------------------

    #[test]
    fn metadata_is_scrubbed_except_the_accounting_keys() {
        // `metadata` is arbitrary client JSON and was never scrubbed at all — the largest hole in
        // this module's promise, on the field applications actually use for per-call context.
        let r = Redactor::all();
        let mut ev = event("p1", json!("clean"), json!("clean"));
        ev.metadata = json!({
            "note": "escalated by jane@example.com",
            "nested": { "ticket": "card 4111 1111 1111 1111" },
            "customer_id": "jane@example.com",
            "api_key_id": "key-123",
            "product_id": "pro@example.com",
            "cost_source": "client",
        });
        assert!(r.redact_event(&mut ev, Redaction::None) > 0);
        let m = &ev.metadata;
        assert_eq!(m["note"], "escalated by <EMAIL>");
        assert!(
            !m["nested"]["ticket"].as_str().unwrap().contains("4111"),
            "the walk reaches nested metadata too: {m}"
        );
        // The passthrough list, and why it is a list rather than "scrub everything": these are join
        // keys. Rewriting one protects nobody and merges the buckets every cost and margin number is
        // grouped by — every customer whose id looks like an email would collapse into one.
        assert_eq!(m["customer_id"], "jane@example.com");
        assert_eq!(m["product_id"], "pro@example.com");
        assert_eq!(m["api_key_id"], "key-123");
        assert_eq!(m["cost_source"], "client");
    }

    #[test]
    fn name_and_source_are_dispositioned_rather_than_unmentioned() {
        let r = Redactor::all();
        let mut ev = event("p1", json!("clean"), json!("clean"));
        ev.name = Some("reply-to jane@example.com".into());
        ev.source = Some("agent-4111111111111111".into());
        assert!(r.redact_event(&mut ev, Redaction::None) > 0);
        assert_eq!(ev.name.as_deref(), Some("reply-to <EMAIL>"));
        assert!(!ev.source.as_deref().unwrap().contains("4111111111111111"));

        // A legitimate call-site label matches no rule and is left exactly alone — the property that
        // makes scrubbing these two cheap rather than a rollup hazard.
        let mut ok = event("p1", json!("clean"), json!("clean"));
        ok.name = Some("summarize-email".into());
        ok.source = Some("checkout-api".into());
        r.redact_event(&mut ok, Redaction::None);
        assert_eq!(ok.name.as_deref(), Some("summarize-email"));
        assert_eq!(ok.source.as_deref(), Some("checkout-api"));
    }

    #[test]
    fn the_caps_leave_an_ordinary_payload_completely_alone() {
        // False-positive economics: a boundary that mangles normal traffic gets turned off. Nothing
        // an ordinary LLM payload contains should ever meet one of these limits.
        let r = Redactor::all();
        let ordinary = json!({
            "messages": (0..40).map(|i| json!({ "role": "user", "content": format!("turn {i}") })).collect::<Vec<_>>(),
            "temperature": 0.0,
            "tools": [{ "name": "search", "parameters": { "type": "object", "properties": { "q": { "type": "string" } } } }],
        });
        let mut ev = event("p1", ordinary.clone(), json!("a normal answer"));
        r.redact_event(&mut ev, Redaction::None);
        assert_eq!(
            ev.input,
            Some(ordinary),
            "no cap fires on realistic traffic"
        );
    }
}
