//! Enrolled relay devices: who may lease, and what each one can actually run (M18).
//!
//! Before this the relay had exactly one device, and it was anonymous: a single shared
//! `LIGHTTRACK_RELAY_DEVICE_KEY` authorized every lease, and the `device` name written onto a task
//! was whatever the client asserted. Two things fell out of that. Identity was un-revocable — one
//! leaked key meant rotating the secret on every device at once — and routing was blind: a task was
//! handed to whoever asked first, including a device whose action library has no such action, which
//! then burns a real attempt reporting "no action" and waits out a five-hour retry interval to do
//! it again.
//!
//! A [`Device`] fixes both. It carries a hashed per-device key (the `api_keys` scheme, so a key is
//! shown once and stored as a salted digest), and it **advertises capabilities**: the action types
//! it can run, exactly (`xprice/reprice-summary`) or by namespace (`xprice/*`). The lease filters on
//! them, so an action can only reach a device that has it — and the enqueue door can answer the
//! question that used to take hours to discover, [`RelayAdmission`]: is there anyone out there who
//! could run this at all?

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One enrolled device that may lease relay tasks.
///
/// `key_hash` follows the API-key scheme verbatim (`"<salt>:<sha256hex>"`), and the raw key —
/// `ltd_<prefix>_<secret>` — is returned once at creation and never stored. `key_prefix` is the
/// non-secret lookup handle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Which project this device belongs to, or `None` for an **operator-wide** device that may
    /// serve every project's tasks. Operator-wide is the shape the shipped single-device relay
    /// already had, so it stays expressible rather than forcing an artificial project on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
    pub name: String,
    pub key_prefix: String,
    /// Salted SHA-256 of the full key. **Never** serialized outward — the API strips it before it
    /// reaches a response, and this type is what the store round-trips.
    #[serde(default)]
    pub key_hash: String,
    /// What this device can run: exact action types, or `"<ns>/*"` namespace prefixes. An **empty**
    /// list means "everything", which is what an agent predating M18 (and the legacy shared key)
    /// advertises — a device that suddenly leased nothing after an upgrade would be a worse failure
    /// than an unfiltered one.
    #[serde(default)]
    pub capabilities: Vec<String>,
    /// When this device last leased, renewed, or reported. The liveness signal
    /// `GET /v1/relay/devices` shows: there is no inbound path to a device, so "is it alive" can
    /// only ever be "when did it last talk to us".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<DateTime<Utc>>,
    /// The `lt-agent` version this device last reported, so an operator can tell a fleet that has
    /// not been upgraded from one that has.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
    /// A revoked device authenticates nothing and is eligible for nothing. Revocation is a flag,
    /// not a delete, so the tasks it already ran keep naming a device that still exists.
    #[serde(default)]
    pub revoked: bool,
}

impl Device {
    /// Whether this device could run `action_type` — revocation included, because an eligibility
    /// count that ignored it would promise a fleet that cannot answer.
    pub fn serves(&self, action_type: &str) -> bool {
        !self.revoked && capability_matches(&self.capabilities, action_type)
    }

    /// Strip the stored secret digest. The API calls this on every device it is about to serialize:
    /// a hash is not a secret, but it is offline-attackable material with no reason to leave the
    /// database.
    pub fn redacted(&self) -> Device {
        let mut d = self.clone();
        d.key_hash = String::new();
        d
    }
}

/// Whether an advertised capability set covers `action_type`.
///
/// Three shapes, in order of how specific they are: `"*"` (everything), `"<ns>/*"` (a namespace),
/// and an exact action type. An **empty** set is "everything" for the back-compat reason on
/// [`Device::capabilities`] — absence of an advertisement is not an advertisement of absence.
pub fn capability_matches(capabilities: &[String], action_type: &str) -> bool {
    if capabilities.is_empty() {
        return true;
    }
    capabilities.iter().any(|c| {
        let c = c.trim();
        if c == "*" {
            return true;
        }
        match c.strip_suffix("/*") {
            Some(ns) => action_type
                .strip_prefix(ns)
                .is_some_and(|rest| rest.starts_with('/')),
            None => c == action_type,
        }
    })
}

/// How much of the fleet could take one action type, as two numbers rather than one.
///
/// One count could not distinguish the two situations the enqueue door has to treat oppositely:
/// **no devices are enrolled at all** (the legacy shared-key deployment — accept, there is nothing
/// to route against) and **devices are enrolled and none advertises this** (refuse — it will never
/// run). `enrolled` counts unrevoked devices; `eligible` counts the subset that serves the action.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceEligibility {
    pub enrolled: u32,
    pub eligible: u32,
}

impl DeviceEligibility {
    /// Count both figures over a device list — the shared rule, so every backend and the API's own
    /// admission check agree on what "eligible" means instead of each re-deriving it.
    pub fn count<'a>(devices: impl IntoIterator<Item = &'a Device>, action_type: &str) -> Self {
        let mut e = DeviceEligibility {
            enrolled: 0,
            eligible: 0,
        };
        for d in devices {
            if d.revoked {
                continue;
            }
            e.enrolled += 1;
            e.eligible += u32::from(d.serves(action_type));
        }
        e
    }

    /// The verdict these counts imply. An **empty fleet** admits: a deployment still on the legacy
    /// shared device key has no rows here, and refusing its traffic would be this feature breaking
    /// the relay it was meant to make reliable.
    pub fn admit(&self, action_type: &str) -> RelayAdmission {
        if self.eligible > 0 || self.enrolled == 0 {
            RelayAdmission::Queued {
                eligible_devices: self.eligible,
            }
        } else {
            RelayAdmission::unroutable(action_type, self.enrolled)
        }
    }
}

/// What the enqueue door decided about a relay task — the answer to "will anything ever run this?".
///
/// A value, not an error, and a **closed vocabulary**: the failure it replaces was silence. Enqueue
/// validated only that `action_type` was non-empty, so a typo'd action type was indistinguishable
/// from a healthy backlog until the task dead-lettered hours later, having burned four attempts on
/// devices that never had the action. `Queued` carries the eligible count so a caller can see the
/// difference between "one device has this" and "the whole fleet does".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "verdict", rename_all = "snake_case")]
pub enum RelayAdmission {
    /// Accepted. `eligible_devices` is how many enrolled, unrevoked devices advertise this action
    /// type **right now** — zero here means the deployment has no device table entries at all
    /// (the legacy shared-key fleet), never that an enrolled fleet declined it.
    Queued { eligible_devices: u32 },
    /// Refused: devices are enrolled, and none of them advertises this action type. The task is not
    /// stored — a queue entry nothing can lease is a slow-motion dead letter.
    Refused { reason: String },
}

impl RelayAdmission {
    /// The refusal an unroutable action type earns, phrased so the operator can act on it: the fix
    /// is either the action's spelling or a device's advertised capabilities.
    pub fn unroutable(action_type: &str, enrolled: u32) -> RelayAdmission {
        RelayAdmission::Refused {
            reason: format!(
                "no enrolled device advertises '{action_type}' ({enrolled} device(s) enrolled) — \
                 check the action type's spelling, or add it to a device's capabilities"
            ),
        }
    }

    pub fn is_refused(&self) -> bool {
        matches!(self, RelayAdmission::Refused { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_match_exactly_by_namespace_and_by_wildcard() {
        let caps = vec!["xprice/*".to_string(), "ops/nightly-report".to_string()];
        assert!(capability_matches(&caps, "xprice/reprice-summary"));
        assert!(capability_matches(&caps, "xprice/deep/nested"));
        assert!(capability_matches(&caps, "ops/nightly-report"));
        // A namespace prefix must not match a *longer namespace name* that merely starts the same
        // way — this is the bug a naive `starts_with` has, and it would route xpricey/* work to a
        // device that cannot run it.
        assert!(!capability_matches(&caps, "xpricey/thing"));
        // Nor may the bare namespace itself match: `xprice/*` names actions inside it.
        assert!(!capability_matches(&caps, "xprice"));
        assert!(!capability_matches(&caps, "ops/other"));
        assert!(capability_matches(&["*".to_string()], "anything/at-all"));
    }

    #[test]
    fn an_empty_advertisement_is_not_an_advertisement_of_absence() {
        // The back-compat rule that keeps a pre-M18 agent (and the legacy shared key) working: it
        // sends no capabilities, and must keep leasing everything rather than silently nothing.
        assert!(capability_matches(&[], "xprice/anything"));
    }

    #[test]
    fn a_revoked_device_is_eligible_for_nothing_whatever_it_advertises() {
        let mut d = Device {
            id: "d1".into(),
            project_id: None,
            name: "laptop".into(),
            key_prefix: "abcd1234".into(),
            key_hash: "salt:digest".into(),
            capabilities: vec!["*".into()],
            last_seen_at: None,
            agent_version: None,
            created_at: Utc::now(),
            revoked: false,
        };
        assert!(d.serves("xprice/x"));
        d.revoked = true;
        assert!(!d.serves("xprice/x"));
        // …and the stored digest never leaves the database on the way to a response.
        assert!(d.redacted().key_hash.is_empty());
    }

    fn dev(name: &str, caps: &[&str]) -> Device {
        Device {
            id: name.into(),
            project_id: None,
            name: name.into(),
            key_prefix: "abcd1234".into(),
            key_hash: "salt:digest".into(),
            capabilities: caps.iter().map(|s| (*s).to_string()).collect(),
            last_seen_at: None,
            agent_version: None,
            created_at: Utc::now(),
            revoked: false,
        }
    }

    #[test]
    fn an_empty_fleet_admits_but_a_fleet_that_cannot_run_it_refuses() {
        // The distinction one count could not make. No rows at all is the legacy shared-key
        // deployment — refusing its traffic would be this feature breaking the relay.
        let empty = DeviceEligibility::count(&[], "xprice/foo");
        assert_eq!(empty.enrolled, 0);
        assert_eq!(
            empty.admit("xprice/foo"),
            RelayAdmission::Queued {
                eligible_devices: 0
            }
        );

        // Enrolled, and none of them advertises it: this task would never run, so it is refused at
        // the door instead of dead-lettering four attempts later.
        let fleet = vec![dev("a", &["ops/*"]), dev("b", &["other/thing"])];
        let e = DeviceEligibility::count(&fleet, "xprice/foo");
        assert_eq!(
            e,
            DeviceEligibility {
                enrolled: 2,
                eligible: 0
            }
        );
        assert!(e.admit("xprice/foo").is_refused());

        // One device that serves it is enough, and the count says how much of the fleet does.
        let fleet = vec![dev("a", &["xprice/*"]), dev("b", &["ops/*"])];
        assert_eq!(
            DeviceEligibility::count(&fleet, "xprice/foo").admit("xprice/foo"),
            RelayAdmission::Queued {
                eligible_devices: 1
            }
        );

        // A revoked device counts for neither number — an eligibility figure that included it
        // would promise a fleet that cannot answer.
        let mut revoked = dev("a", &["xprice/*"]);
        revoked.revoked = true;
        let e = DeviceEligibility::count(&[revoked], "xprice/foo");
        assert_eq!(
            e,
            DeviceEligibility {
                enrolled: 0,
                eligible: 0
            }
        );
    }

    #[test]
    fn the_admission_verdict_is_a_closed_two_valued_vocabulary_on_the_wire() {
        let q = serde_json::to_value(RelayAdmission::Queued {
            eligible_devices: 2,
        })
        .expect("serialize");
        assert_eq!(q["verdict"], "queued");
        assert_eq!(q["eligible_devices"], 2);
        let r = RelayAdmission::unroutable("xprice/foo", 3);
        assert!(r.is_refused());
        let v = serde_json::to_value(&r).expect("serialize");
        assert_eq!(v["verdict"], "refused");
        assert!(
            v["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("xprice/foo"),
            "the refusal must name the action type that could not be routed: {v}"
        );
    }
}
