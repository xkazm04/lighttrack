//! **Endpoint identity**: which program answered, established *before* a benchmark row is
//! attributed to a provider.
//!
//! [`provider`](crate::provider) and [`model_id`](crate::model_id) own identity for providers that
//! declare themselves — a name the operator gave, folded through a declared synonym table. This is
//! the layer beneath them, for the case where the declaration was never reliable: an operator
//! benchmarking a self-hosted, OpenAI-compatible endpoint. Half a dozen local runtimes answer that
//! one protocol, and the protocol deliberately does not identify the implementation — being
//! interchangeable is what it is *for*. So the provider string on such a run is a free-text label
//! somebody chose, and two contributors who each benchmarked "their local setup" merge into one
//! leaderboard row having measured different programs at different quantizations.
//!
//! What this is **not**. Not health: whether the endpoint answers is a different question with
//! different evidence. Not model identity: [`model_id`](crate::model_id) owns that and never
//! matches on family, and nothing here licenses loosening it. Not routing: this project does not
//! proxy inference. This answers exactly one question — *what is answering* — and files the answer
//! with the evidence class that produced it and the date the observation was taken.
//!
//! **Every discriminator below is a dated observation about somebody else's program, not an
//! invariant.** They decay on release schedules we do not control, so each entry says what it was
//! measured against; a stale one is a re-verification task, not a magic string.

use serde::{Deserialize, Serialize};

/// Prefix for the provider id a *probed* endpoint contributes under. `.` is inside
/// [`ProviderId`](crate::provider::ProviderId)'s charset, so these survive canonicalization, and
/// they are absent from every price book — so the alias table passes them through unchanged rather
/// than folding them into a named vendor's row.
pub const SELF_HOSTED_PREFIX: &str = "self-hosted.";

/// What the probe concluded the endpoint *is*.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum Endpoint {
    /// One implementation, identified.
    Runtime { name: String },
    /// A multiplexer answered — it fronts several upstreams behind one compatible surface, so the
    /// program that actually generated the tokens is not resolvable from here. Naming the front as
    /// the runtime would manufacture confidence, which is worse than no probe at all.
    Multiplexed { name: String },
    /// Nothing we can recognize answered. A first-class state, never the most likely guess.
    Unrecognized,
}

/// Which rung of the evidence ladder produced the identity. Ordered strongest first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Evidence {
    /// A route only this implementation serves, carrying a field the shared schema has no word for.
    NativeRoute,
    /// A namespace the implementation controls, read out of the shared protocol's own response.
    OwnedBy,
    /// The root path's plain-text banner. Crude, and the right fallback for an empty inventory.
    RootBanner,
    /// The endpoint was probed and answered nothing we recognize.
    NoEvidence,
    /// No probe ran: the identity is whatever the operator typed. This is also what the *absence*
    /// of an [`EndpointIdentity`] on a run means.
    OperatorAsserted,
}

/// The record that rides with a run: what answered, how we know, and when we looked.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EndpointIdentity {
    pub endpoint: Endpoint,
    pub evidence: Evidence,
    /// `YYYY-MM-DD`, the day the probe ran — so a reader can tell a fresh observation from one
    /// taken against a runtime release that has since moved.
    pub probed_on: String,
}

impl EndpointIdentity {
    /// The unprobed state. Equivalent to carrying no record at all; spelled out for callers that
    /// would rather say so than omit the field.
    pub fn operator_asserted() -> Self {
        Self {
            endpoint: Endpoint::Unrecognized,
            evidence: Evidence::OperatorAsserted,
            probed_on: String::new(),
        }
    }

    /// Whether the identity was *established* rather than asserted — the numerator of the measure
    /// this module exists to make computable.
    pub fn established(&self) -> bool {
        matches!(
            self.evidence,
            Evidence::NativeRoute | Evidence::OwnedBy | Evidence::RootBanner
        ) && matches!(self.endpoint, Endpoint::Runtime { .. })
    }

    /// The provider id a contributed row must be keyed on, or `None` when nothing was probed — in
    /// which case the operator's own string stands unchanged, which is the existing behaviour for
    /// every commercial provider reached at its documented address.
    ///
    /// An unresolvable endpoint still gets a key, because the alternative is letting it keep a name
    /// like `openai` that a re-pointed base made a fiction. `self-hosted.unresolved` and
    /// `self-hosted.unrecognized` cannot be confused with a named provider and cannot merge with
    /// each other.
    pub fn collective_provider(&self) -> Option<String> {
        if self.evidence == Evidence::OperatorAsserted {
            return None;
        }
        Some(match &self.endpoint {
            Endpoint::Runtime { name } => format!("{SELF_HOSTED_PREFIX}{name}"),
            Endpoint::Multiplexed { .. } => format!("{SELF_HOSTED_PREFIX}unresolved"),
            Endpoint::Unrecognized => format!("{SELF_HOSTED_PREFIX}unrecognized"),
        })
    }
}

/// What a probe saw. Pure input to [`resolve`]; gathering it is the engine's job.
#[derive(Debug, Clone, Default)]
pub struct Observations {
    /// `(path, body)` for each native route that answered 200.
    pub routes: Vec<(String, String)>,
    /// Distinct `owned_by` values read from the shared protocol's own model listing. Empty on a
    /// runtime with nothing loaded — the hole rung 3 exists for.
    pub owned_by: Vec<String>,
    /// The root path's body, truncated by the probe.
    pub banner: Option<String>,
}

/// One implementation's discriminators. Dated at its declaration site in [`TABLE`].
struct Discriminator {
    /// The id this endpoint is filed under.
    runtime: &'static str,
    /// True when this program fronts *other* upstreams. See [`Endpoint::Multiplexed`].
    multiplexes: bool,
    /// `(path, marker)`: a route only this implementation serves, and a token its 200 response
    /// carries that the shared schema has no word for.
    native_route: (&'static str, &'static str),
    /// `owned_by` values this implementation stamps on its own model records. Empty where the
    /// implementation emits the OpenAI filler value, which identifies nothing.
    owned_by: &'static [&'static str],
    /// A substring of the root path's plain-text banner, lowercased.
    banner: Option<&'static str>,
}

/// The discriminator table. **Multiplexers come first and win outright** — a runtime discriminator
/// seen behind a multiplexer describes one of its upstreams, not the endpoint that answered.
const TABLE: &[Discriminator] = &[
    // LiteLLM proxy 1.7x, recorded 2026-09-03. MULTIPLEXES. `/health/liveliness` is its own route
    // and answers the fixed string `I'm alive!`; nothing in the OpenAI protocol serves it.
    Discriminator {
        runtime: "litellm",
        multiplexes: true,
        native_route: ("/health/liveliness", "alive"),
        owned_by: &[],
        banner: None,
    },
    // Ollama 0.6.x, recorded 2026-09-03. `/api/tags` is its own management API and returns a
    // per-model `digest` the OpenAI schema has no word for. Its `/v1/models` records are owned by
    // `library`, and the root path answers the plain-text banner `Ollama is running` — the rung
    // that still works with nothing pulled.
    Discriminator {
        runtime: "ollama",
        multiplexes: false,
        native_route: ("/api/tags", "digest"),
        owned_by: &["library"],
        banner: Some("ollama is running"),
    },
    // llama.cpp `llama-server`, recorded 2026-09-03. `/props` is llama.cpp's own settings route and
    // carries `total_slots`, a concept the shared protocol does not have.
    Discriminator {
        runtime: "llama-cpp",
        multiplexes: false,
        native_route: ("/props", "total_slots"),
        owned_by: &["llamacpp", "llama.cpp"],
        banner: None,
    },
    // vLLM 0.8.x, recorded 2026-09-03. `/version` returns `{"version": "..."}`; a generic
    // OpenAI-compatible server 404s it. Every record in its model listing is owned by `vllm`.
    Discriminator {
        runtime: "vllm",
        multiplexes: false,
        native_route: ("/version", "version"),
        owned_by: &["vllm"],
        banner: None,
    },
    // LM Studio 0.3.x, recorded 2026-09-03. Its own REST surface `/api/v0/models` returns a
    // per-model `quantization` the OpenAI schema has no word for. Its `owned_by` is the OpenAI
    // filler `organization_owner`, which identifies nothing — deliberately not listed below.
    Discriminator {
        runtime: "lm-studio",
        multiplexes: false,
        native_route: ("/api/v0/models", "quantization"),
        owned_by: &[],
        banner: None,
    },
    // text-generation-webui (oobabooga) 2.x, recorded 2026-09-03. `/v1/internal/model/info` is its
    // own extension to the compatible surface and returns the loaded `model_name`.
    Discriminator {
        runtime: "text-generation-webui",
        multiplexes: false,
        native_route: ("/v1/internal/model/info", "model_name"),
        owned_by: &[],
        banner: None,
    },
];

/// Every native route the probe should try, in the order [`resolve`] consults them. Exposed so the
/// probe fetches exactly what the table knows how to read, and stops at the first match.
pub fn native_routes() -> impl Iterator<Item = &'static str> {
    TABLE.iter().map(|d| d.native_route.0)
}

impl Discriminator {
    fn route_fired(&self, obs: &Observations) -> bool {
        let (path, marker) = self.native_route;
        obs.routes.iter().any(|(p, body)| {
            p.eq_ignore_ascii_case(path) && body.to_ascii_lowercase().contains(marker)
        })
    }
}

/// Resolve observations into an identity. Pure and total; `probed_on` is `YYYY-MM-DD`.
///
/// The ordering is the technique: identity is decided here, once, and read everywhere. A second
/// inference somewhere else is how two subsystems come to disagree about what an endpoint is.
pub fn resolve(obs: &Observations, probed_on: &str) -> EndpointIdentity {
    let found = |endpoint, evidence| EndpointIdentity {
        endpoint,
        evidence,
        probed_on: probed_on.to_string(),
    };
    for d in TABLE {
        if d.route_fired(obs) {
            let name = d.runtime.to_string();
            let endpoint = if d.multiplexes {
                Endpoint::Multiplexed { name }
            } else {
                Endpoint::Runtime { name }
            };
            return found(endpoint, Evidence::NativeRoute);
        }
    }
    if let Some(name) = owned_by_runtime(obs) {
        return found(Endpoint::Runtime { name }, Evidence::OwnedBy);
    }
    if let Some(name) = banner_runtime(obs) {
        return found(Endpoint::Runtime { name }, Evidence::RootBanner);
    }
    found(Endpoint::Unrecognized, Evidence::NoEvidence)
}

/// The one runtime every observed `owned_by` value belongs to. `None` when the inventory is empty,
/// carries a value no entry claims, or points at more than one runtime — a listing that disagrees
/// with itself is evidence *against* a single implementation, not for the first match.
fn owned_by_runtime(obs: &Observations) -> Option<String> {
    let mut resolved: Option<&'static str> = None;
    for value in &obs.owned_by {
        let v = value.trim().to_ascii_lowercase();
        let hit = TABLE
            .iter()
            .find(|d| d.owned_by.iter().any(|o| *o == v))?
            .runtime;
        match resolved {
            Some(prev) if prev != hit => return None,
            _ => resolved = Some(hit),
        }
    }
    resolved.map(str::to_string)
}

fn banner_runtime(obs: &Observations) -> Option<String> {
    let banner = obs.banner.as_ref()?.to_ascii_lowercase();
    TABLE
        .iter()
        .find(|d| d.banner.is_some_and(|b| banner.contains(b)))
        .map(|d| d.runtime.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obs(routes: &[(&str, &str)], owned_by: &[&str], banner: Option<&str>) -> Observations {
        Observations {
            routes: routes
                .iter()
                .map(|(p, b)| (p.to_string(), b.to_string()))
                .collect(),
            owned_by: owned_by.iter().map(|s| s.to_string()).collect(),
            banner: banner.map(str::to_string),
        }
    }

    #[test]
    fn a_native_route_is_the_strongest_rung() {
        let id = resolve(
            &obs(
                &[("/api/tags", r#"{"models":[{"digest":"sha256:ab"}]}"#)],
                &[],
                None,
            ),
            "2026-09-03",
        );
        assert_eq!(
            id.endpoint,
            Endpoint::Runtime {
                name: "ollama".into()
            }
        );
        assert_eq!(id.evidence, Evidence::NativeRoute);
        assert!(id.established());
        assert_eq!(
            id.collective_provider().as_deref(),
            Some("self-hosted.ollama")
        );
        // A 200 on the right path whose body lacks the marker is not the discriminator.
        let miss = resolve(
            &obs(&[("/api/tags", "<html>404</html>")], &[], None),
            "2026-09-03",
        );
        assert_eq!(miss.endpoint, Endpoint::Unrecognized);
    }

    #[test]
    fn owned_by_is_read_from_the_response_and_must_agree_with_itself() {
        let id = resolve(&obs(&[], &["vllm", "vllm"], None), "2026-09-03");
        assert_eq!(
            id.endpoint,
            Endpoint::Runtime {
                name: "vllm".into()
            }
        );
        assert_eq!(id.evidence, Evidence::OwnedBy);
        // A listing that names two runtimes is evidence against one implementation, not for the
        // first match.
        let mixed = resolve(&obs(&[], &["vllm", "library"], None), "2026-09-03");
        assert_eq!(mixed.endpoint, Endpoint::Unrecognized);
        // A value no entry claims (the OpenAI filler) identifies nothing.
        let filler = resolve(&obs(&[], &["organization_owner"], None), "2026-09-03");
        assert_eq!(filler.endpoint, Endpoint::Unrecognized);
    }

    #[test]
    fn an_empty_inventory_falls_through_to_the_banner_not_to_a_guess() {
        // The fresh-install shape: reachable, protocol answered, zero records, so every per-record
        // discriminator is structurally unavailable.
        let fresh = resolve(&obs(&[], &[], Some("Ollama is running")), "2026-09-03");
        assert_eq!(
            fresh.endpoint,
            Endpoint::Runtime {
                name: "ollama".into()
            }
        );
        assert_eq!(fresh.evidence, Evidence::RootBanner);
        // With no banner either, the answer is unknown — never the most likely runtime.
        let blind = resolve(&obs(&[], &[], Some("<!doctype html>")), "2026-09-03");
        assert_eq!(blind.endpoint, Endpoint::Unrecognized);
        assert_eq!(blind.evidence, Evidence::NoEvidence);
        assert!(!blind.established());
        assert_eq!(
            blind.collective_provider().as_deref(),
            Some("self-hosted.unrecognized")
        );
    }

    #[test]
    fn a_multiplexer_wins_outright_and_is_never_reported_as_a_runtime() {
        // Falsifier 3, made a test: a proxy fronting Ollama answers BOTH its own route and (if it
        // forwarded one) a runtime's. Reporting `ollama` here would name one upstream as the thing
        // that was measured — worse than no probe, because it manufactures confidence.
        let both = obs(
            &[
                ("/api/tags", r#"{"models":[{"digest":"x"}]}"#),
                ("/health/liveliness", "I'm alive!"),
            ],
            &["library"],
            Some("Ollama is running"),
        );
        let id = resolve(&both, "2026-09-03");
        assert_eq!(
            id.endpoint,
            Endpoint::Multiplexed {
                name: "litellm".into()
            }
        );
        assert!(!id.established(), "a front is not an established runtime");
        assert_eq!(
            id.collective_provider().as_deref(),
            Some("self-hosted.unresolved")
        );
    }

    #[test]
    fn an_unprobed_run_keeps_the_operators_string() {
        let asserted = EndpointIdentity::operator_asserted();
        assert!(!asserted.established());
        assert_eq!(asserted.collective_provider(), None);
    }

    #[test]
    fn the_record_round_trips_through_a_run_report() {
        let id = resolve(&obs(&[], &["llamacpp"], None), "2026-09-03");
        let v = serde_json::to_value(&id).unwrap();
        assert_eq!(v["endpoint"]["kind"], "runtime");
        assert_eq!(v["evidence"], "owned-by");
        assert_eq!(v["probed_on"], "2026-09-03");
        assert_eq!(serde_json::from_value::<EndpointIdentity>(v).unwrap(), id);
    }

    #[test]
    fn every_self_hosted_key_survives_provider_canonicalization() {
        // The keys must reach the leaderboard unchanged, or the layer beneath the alias table has
        // been overwritten by it.
        for name in TABLE
            .iter()
            .map(|d| d.runtime)
            .chain(["unresolved", "unrecognized"])
        {
            let key = format!("{SELF_HOSTED_PREFIX}{name}");
            assert_eq!(crate::provider::ProviderId::new(&key).as_str(), key);
        }
    }
}
