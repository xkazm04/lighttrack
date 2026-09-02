use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How prompt/output payloads are persisted for a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Redaction {
    /// Store payloads as sent.
    #[default]
    None,
    /// Store only a hash of payloads (presence/diff without content).
    Hash,
    /// Never persist payloads.
    Drop,
}

/// Reserved `metadata` key carrying the [`RedactionStamp`]. Server-owned: the ingest path strips
/// whatever a client sent under it before stamping its own, exactly as it does for `api_key_id`.
/// Defined here so the writer (the api crate's redactor) and the reader
/// ([`crate::LlmEvent::redaction`]) cannot drift onto two spellings.
pub const REDACTION_KEY: &str = "redaction";

/// What the ingest boundary did to one row.
///
/// D14 names the one class of defect this product cannot observe: a scrub that rewrote the evidence
/// a judge later read. It could not be observed because nothing was recorded — the redactor returned
/// a span count that ingest logged at debug and dropped, so a database was an indistinguishable mix
/// of raw and scrubbed rows. This is that count, plus the two things that make it mean something:
/// the persistence policy in force, and *which* rule set produced the spans (the rule list has
/// already changed shape once, so a bare count is not comparable across versions).
///
/// `dataset_items.anonymization` has stored `{method, redactions}` since datasets shipped — the
/// shape is accepted; only the fact table lacked it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RedactionStamp {
    /// The per-project persistence policy applied to the payloads (`none` | `hash` | `drop`).
    #[serde(default)]
    pub policy: Redaction,
    /// Whether the PII scrub ran at all for this project. `false` with `spans: 0` is a row stored
    /// verbatim; `true` with `spans: 0` is a row the scrubber read and found nothing in. Collapsing
    /// those two into "0" is exactly the ambiguity this stamp exists to end.
    #[serde(default)]
    pub scrub: bool,
    /// How many spans the scrub replaced across every client-supplied surface of the event.
    #[serde(default)]
    pub spans: u32,
    /// `lighttrack_anon::rules_fingerprint()` — which rule set did it. Empty when `scrub` is false.
    #[serde(default)]
    pub rules: String,
}

/// What a key is allowed to do. Three capabilities on a key, deliberately **not** RBAC: no roles,
/// no inheritance, no per-resource grants — just the three doors an API key can be handed
/// (`docs/ARCHITECTURE.md` §9).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scope {
    /// Write monitored traffic: the event / batch / OTLP doors and the relay settle report.
    Ingest,
    /// Read stored data (the `GET`s under the key's own project).
    Read,
    /// Configure the project: limits, prompts, benchmarks, datasets, rubrics.
    Manage,
}

impl Scope {
    /// The stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Ingest => "ingest",
            Scope::Read => "read",
            Scope::Manage => "manage",
        }
    }

    /// Parse a wire string, case-insensitively. `None` for anything unknown.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "ingest" => Some(Scope::Ingest),
            "read" => Some(Scope::Read),
            "manage" => Some(Scope::Manage),
            _ => None,
        }
    }

    /// Every scope, for exhaustive tests and `--help` text.
    pub const ALL: [Scope; 3] = [Scope::Ingest, Scope::Read, Scope::Manage];
}

/// What a key with no recorded scopes is granted. Permissive for **one release**, so that a key
/// minted before scopes existed keeps working exactly as it did — a silent downgrade to `[Ingest]`
/// would have broken every dashboard reading through a project key on upgrade day. Its use is
/// logged (see the api crate's guards); the documented next default is `[Ingest]`.
pub fn default_scopes() -> Vec<Scope> {
    vec![Scope::Ingest, Scope::Read]
}

/// Encode scopes for a text column / Firestore field: a JSON array of wire strings.
pub fn encode_scopes(scopes: &[Scope]) -> String {
    let items: Vec<&str> = scopes.iter().map(|s| s.as_str()).collect();
    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// Decode a stored scopes column. `None`, empty, or anything unparseable reads as
/// [`default_scopes`] — the **backfill sentinel**: a row written before the column existed carries
/// no opinion, and the safe reading of "no opinion" during the back-compat release is the old
/// behaviour, not a lockout. Unknown scope strings (a future scope seen by an older binary) are
/// dropped rather than failing the key; a key left with nothing falls back to the default.
pub fn decode_scopes(raw: Option<&str>) -> Vec<Scope> {
    let Some(raw) = raw.map(str::trim).filter(|s| !s.is_empty()) else {
        return default_scopes();
    };
    let parsed: Vec<Scope> = serde_json::from_str::<Vec<String>>(raw)
        .unwrap_or_default()
        .iter()
        .filter_map(|s| Scope::parse(s))
        .collect();
    if parsed.is_empty() {
        default_scopes()
    } else {
        parsed
    }
}

/// A monitored application / tenant.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub name: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub redaction: Redaction,
    /// Consent to include this project's benchmark runs in a collective-network digest. Default
    /// **off**: contributing a project's eval results to a shared hub is an act, not an inheritance —
    /// `lt collective contribute` must never ship a project nobody opted in.
    #[serde(default)]
    pub collective_opt_in: bool,
    /// Refuse to promote — a prompt label, a benchmark gate — on a judge that is not `trusted` for
    /// the rubric being gated on (M11). Default **off**: turning it on retroactively would block
    /// every existing deployment's gates on the day it upgraded, because nothing has been
    /// calibrated yet. Once on, both `untrusted` and `unknown` block, because a gate that promotes
    /// on a judge nobody has measured is precisely the failure this flag exists to close.
    #[serde(default)]
    pub require_trusted_judge: bool,
    pub created_at: DateTime<Utc>,
    /// When the project was archived. `DELETE /v1/projects/:id` **archives** — it sets this and
    /// `enabled = false` — because a project's events, scores and benchmark runs are the record a
    /// cost report is built from; dropping the tenant row would orphan them silently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<DateTime<Utc>>,
}

fn default_true() -> bool {
    true
}

/// An ingest API key. Only `key_hash` is persisted; the raw secret is shown once at creation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKey {
    pub id: String,
    pub project_id: String,
    pub name: String,
    /// Non-secret, human-recognizable prefix, e.g. `lt_ab12cd`.
    pub prefix: String,
    /// Salted hash of the full secret (hashing lives in the `api` crate).
    pub key_hash: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub revoked: bool,
    /// What this key may do. Absent/empty reads as [`default_scopes`] — see [`decode_scopes`].
    #[serde(default = "default_scopes")]
    pub scopes: Vec<Scope>,
    /// Hard expiry. A key past it authenticates as nothing (401), which is what makes a rotation
    /// grace window self-closing: rotation stamps the *old* key's expiry instead of scheduling a
    /// background revoke that a restart would lose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

impl ApiKey {
    /// Does this key carry `want`?
    pub fn has_scope(&self, want: Scope) -> bool {
        self.scopes.contains(&want)
    }

    /// Is this key past its expiry at `now`? A key with no expiry never is.
    pub fn is_expired(&self, now: DateTime<Utc>) -> bool {
        self.expires_at.is_some_and(|e| e <= now)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_scope_round_trips_through_its_wire_string() {
        for s in Scope::ALL {
            assert_eq!(Scope::parse(s.as_str()), Some(s));
            assert_eq!(Scope::parse(&s.as_str().to_uppercase()), Some(s));
        }
        assert_eq!(Scope::parse("admin"), None);
    }

    /// The backfill sentinel: a key row written before the column existed must keep working.
    #[test]
    fn absent_or_unreadable_scopes_read_as_the_permissive_default() {
        for raw in [None, Some(""), Some("   "), Some("not json"), Some("[]")] {
            assert_eq!(decode_scopes(raw), default_scopes(), "{raw:?}");
        }
    }

    #[test]
    fn encoded_scopes_decode_back_to_themselves() {
        let scopes = vec![Scope::Ingest, Scope::Manage];
        assert_eq!(decode_scopes(Some(&encode_scopes(&scopes))), scopes);
        // A single narrow scope is the whole point — it must NOT widen back to the default.
        let only = vec![Scope::Ingest];
        assert_eq!(decode_scopes(Some(&encode_scopes(&only))), only);
    }

    /// An older binary reading a key a newer one granted a future scope keeps the scopes it
    /// understands rather than refusing the key outright.
    #[test]
    fn unknown_scope_strings_are_dropped_not_fatal() {
        assert_eq!(
            decode_scopes(Some(r#"["ingest","teleport"]"#)),
            vec![Scope::Ingest]
        );
        assert_eq!(decode_scopes(Some(r#"["teleport"]"#)), default_scopes());
    }

    /// Expiry is inclusive at the instant itself — a key whose window closed "now" is closed.
    #[test]
    fn expiry_is_only_in_effect_when_set() {
        let now = Utc::now();
        let mut k = ApiKey {
            id: "k".into(),
            project_id: "p".into(),
            name: "n".into(),
            prefix: "pre".into(),
            key_hash: "h".into(),
            created_at: now,
            last_used_at: None,
            revoked: false,
            scopes: default_scopes(),
            expires_at: None,
        };
        assert!(!k.is_expired(now));
        k.expires_at = Some(now);
        assert!(k.is_expired(now));
        k.expires_at = Some(now + chrono::Duration::seconds(1));
        assert!(!k.is_expired(now));
    }
}
