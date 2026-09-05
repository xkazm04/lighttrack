//! Asking an OpenAI-compatible endpoint **what it is**, before a benchmark row is attributed to it.
//!
//! `LIGHTTRACK_OPENAI_BASE` re-points the origin the `openai` provider path generates against
//! (`providers`). That one env var is the whole seam this module exists for: an
//! operator who points it at a local runtime benchmarks Ollama, llama.cpp or vLLM and contributes a
//! row keyed `provider: "openai"` — a number nobody measured at OpenAI. The reasoning and the
//! evidence ladder live in [`lighttrack_core::endpoint_identity`]; this is only the gathering.
//!
//! **Cost and blast radius.** The probe runs once per benchmark run, never per case, and only when
//! the base is re-pointed — an unset `LIGHTTRACK_OPENAI_BASE` means generation goes to
//! `api.openai.com`, whose identity its documented address already establishes, so nothing is
//! fetched at all. Every request is capped at [`PROBE_TIMEOUT`] on its own client, because the
//! generation client's 120s timeout would let a black-holed host stall a run.
//!
//! The rungs are fetched in order and the walk stops at the first that resolves, so the common case
//! (a native route answers) is one or two requests. The native rung is fetched *whole* before any
//! decision, because a multiplexer must be able to outrank a runtime route it forwarded — deciding
//! on the first hit would name one upstream as the thing that was measured.

use std::io::Read;
use std::sync::OnceLock;
use std::time::Duration;

use serde_json::Value;

use lighttrack_core::{native_routes, resolve_endpoint, Endpoint, EndpointIdentity, Observations};

/// The env var that re-points the OpenAI-compatible origin. Spelled here as well as in
/// `providers` because this module's whole reason to exist is that it is set.
pub const OPENAI_BASE_ENV: &str = "LIGHTTRACK_OPENAI_BASE";

/// Per-request cap. A probe is a courtesy question; it may never hold a run open.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Bodies are read for a marker substring, never parsed whole beyond the model listing.
const MAX_PROBE_BYTES: u64 = 64 * 1024;

fn probe_client() -> Option<&'static reqwest::blocking::Client> {
    static CLIENT: OnceLock<Option<reqwest::blocking::Client>> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::blocking::Client::builder()
                .connect_timeout(PROBE_TIMEOUT)
                .timeout(PROBE_TIMEOUT)
                .build()
                .ok()
        })
        .as_ref()
}

/// Probe the endpoint the `openai` provider path will actually generate against, or `None` when the
/// base is not re-pointed. `probed_on` is `YYYY-MM-DD` — the caller owns the clock.
pub fn probe_openai_base(probed_on: &str) -> Option<EndpointIdentity> {
    let base = std::env::var(OPENAI_BASE_ENV)
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    Some(probe(base.trim(), probed_on))
}

/// The origin a base URL sits on, with a trailing `/` or `/v1` removed. Operators write both
/// spellings; the native rung lives at the origin while the shared protocol lives under `/v1`.
pub fn origin_of(base: &str) -> String {
    let b = base.trim().trim_end_matches('/');
    b.strip_suffix("/v1").unwrap_or(b).to_string()
}

/// Gather the evidence and hand it to the one resolver. Total: an endpoint that answers nothing
/// resolves to `Unrecognized`, which is a state, not a failure.
pub fn probe(base: &str, probed_on: &str) -> EndpointIdentity {
    let origin = origin_of(base);
    let mut obs = Observations::default();

    // Rung 1, fetched whole: a route only one implementation serves.
    for path in native_routes() {
        match get(&format!("{origin}{path}")) {
            Fetched::Body(body) => obs.routes.push((path.to_string(), body)),
            Fetched::NotServed => {}
            Fetched::Unreachable => return resolve_endpoint(&obs, probed_on),
        }
    }
    let id = resolve_endpoint(&obs, probed_on);
    if id.endpoint != Endpoint::Unrecognized {
        return id;
    }

    // Rung 2: a namespace the implementation controls, read out of the shared protocol's response.
    if let Fetched::Body(body) = get(&format!("{origin}/v1/models")) {
        obs.owned_by = owned_by_values(&body);
    }
    let id = resolve_endpoint(&obs, probed_on);
    if id.endpoint != Endpoint::Unrecognized {
        return id;
    }

    // Rung 3: the root banner — crude, and the only rung that survives an empty model list.
    if let Fetched::Body(banner) = get(&format!("{origin}/")) {
        obs.banner = Some(banner);
    }
    resolve_endpoint(&obs, probed_on)
}

/// Distinct `owned_by` values from an OpenAI-shaped model listing, in the order seen. Read from the
/// *response*, never from configuration.
fn owned_by_values(body: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    let items = v
        .get("data")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    for item in items {
        if let Some(owner) = item.get("owned_by").and_then(Value::as_str) {
            let owner = owner.trim().to_string();
            if !owner.is_empty() && !out.contains(&owner) {
                out.push(owner);
            }
        }
    }
    out
}

/// What one request learned. `NotServed` and `Unreachable` are deliberately different answers: a
/// 404 on a native route is the *expected* reply from every implementation that is not the one
/// being asked about, while a transport failure says nothing is answering at this origin at all —
/// and asking the remaining rungs would only spend another timeout to learn the same thing.
enum Fetched {
    Body(String),
    NotServed,
    Unreachable,
}

/// A bounded GET, capped at [`PROBE_TIMEOUT`].
fn get(url: &str) -> Fetched {
    let Some(client) = probe_client() else {
        return Fetched::Unreachable;
    };
    let Ok(resp) = client.get(url).send() else {
        return Fetched::Unreachable;
    };
    if !resp.status().is_success() {
        return Fetched::NotServed;
    }
    let mut buf = Vec::new();
    match resp.take(MAX_PROBE_BYTES).read_to_end(&mut buf) {
        Ok(_) => Fetched::Body(String::from_utf8_lossy(&buf).into_owned()),
        Err(_) => Fetched::NotServed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_origin_survives_both_spellings_operators_write() {
        assert_eq!(
            origin_of("http://localhost:11434"),
            "http://localhost:11434"
        );
        assert_eq!(
            origin_of("http://localhost:11434/v1"),
            "http://localhost:11434"
        );
        assert_eq!(
            origin_of("  http://localhost:8000/v1/  "),
            "http://localhost:8000"
        );
        assert_eq!(
            origin_of("https://api.openai.com"),
            "https://api.openai.com"
        );
    }

    #[test]
    fn owned_by_is_deduped_and_survives_a_listing_it_cannot_parse() {
        assert_eq!(
            owned_by_values(
                r#"{"object":"list","data":[{"owned_by":"vllm"},{"owned_by":"vllm"}]}"#
            ),
            vec!["vllm".to_string()]
        );
        assert_eq!(
            owned_by_values(r#"{"data":[{"owned_by":"library"},{"owned_by":"vllm"}]}"#),
            vec!["library".to_string(), "vllm".to_string()]
        );
        // The empty-inventory shape: well-formed, zero records, zero evidence.
        assert!(owned_by_values(r#"{"object":"list","data":[]}"#).is_empty());
        assert!(owned_by_values("<html>nope</html>").is_empty());
    }
}
