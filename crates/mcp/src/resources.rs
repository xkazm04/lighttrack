//! MCP resources — entity attachment via `lighttrack://` URIs.
//!
//! LightTrack has no fixed resource list, so `resources/list` is honestly empty; instead we advertise
//! resource *templates* (`lighttrack://trace/{id}`, `.../event/{id}`, `.../benchmark/{id}`) — the
//! idiomatic MCP shape for "attach this entity by id". `resources/read` resolves one through the same
//! thin HTTP client + Markdown renderers the tools use, returning the rendered Markdown (primary) plus
//! the raw JSON as a second content item, so a client can attach either view.

use serde_json::{json, Value};

use crate::client::Client;

/// Each template: the URI kind, the API path prefix it reads, and a human description.
const KINDS: &[(&str, &str, &str)] = &[
    ("trace", "/v1/traces/", "One agent trace: rolled-up totals, the span tree, and any scores within it."),
    ("event", "/v1/events/", "One LLM call event: provider, model, tokens, cost, latency, and status."),
    ("benchmark", "/v1/benchmarks/", "One benchmark definition: rubric, judge model, dataset, and baseline."),
];

const SCHEME: &str = "lighttrack://";

/// `resources/list` — no fixed resources exist (entities are addressed by id via templates), so this
/// is honestly empty. Clients enumerate the addressable shapes through `resources/templates/list`.
pub(crate) fn list() -> Value {
    json!({ "resources": [] })
}

/// `resources/templates/list` — the `lighttrack://{kind}/{id}` shapes a client can fill in and read.
pub(crate) fn templates_list() -> Value {
    let templates: Vec<Value> = KINDS
        .iter()
        .map(|(kind, _, desc)| {
            json!({
                "uriTemplate": format!("{SCHEME}{kind}/{{id}}"),
                "name": format!("lighttrack-{kind}"),
                "description": desc,
                "mimeType": "text/markdown"
            })
        })
        .collect();
    json!({ "resourceTemplates": templates })
}

/// `resources/read` — resolve a `lighttrack://{kind}/{id}` URI to its contents. Returns the rendered
/// Markdown first (primary), then the raw JSON, both tagged with the request URI. An unknown scheme,
/// kind, or missing id is a clear error; an HTTP failure (e.g. 404) flows through the caller's mapper.
pub(crate) fn read(c: &Client, params: &Value) -> Result<Value, String> {
    let uri = params
        .get("uri")
        .and_then(Value::as_str)
        .ok_or_else(|| "missing required argument: uri".to_string())?;
    let (kind, id) = parse_uri(uri)?;
    let (_, prefix, _) = KINDS
        .iter()
        .find(|(k, _, _)| *k == kind)
        .ok_or_else(|| format!("unknown resource kind '{kind}' — expected one of trace, event, benchmark"))?;

    let body = c.get(&format!("{prefix}{id}"))?;
    let markdown = lighttrack_render::render(render_kind(kind), &body)
        .unwrap_or_else(|| serde_json::to_string_pretty(&body).unwrap_or_default());
    let raw_json = serde_json::to_string_pretty(&body).unwrap_or_default();

    Ok(json!({
        "contents": [
            { "uri": uri, "mimeType": "text/markdown", "text": markdown },
            { "uri": uri, "mimeType": "application/json", "text": raw_json }
        ]
    }))
}

/// Split a `lighttrack://{kind}/{id}` URI into `(kind, id)`. Errors on a wrong scheme, a missing id,
/// or an empty segment.
fn parse_uri(uri: &str) -> Result<(&str, &str), String> {
    let rest = uri
        .strip_prefix(SCHEME)
        .ok_or_else(|| format!("resource uri must start with `{SCHEME}` (got `{uri}`)"))?;
    let (kind, id) = rest
        .split_once('/')
        .ok_or_else(|| format!("resource uri must be `{SCHEME}{{kind}}/{{id}}` (got `{uri}`)"))?;
    if kind.is_empty() || id.is_empty() {
        return Err(format!("resource uri is missing a kind or id (got `{uri}`)"));
    }
    Ok((kind, id))
}

/// The render `kind` (an MCP tool name the render layer keys on) for a resource kind.
fn render_kind(kind: &str) -> &'static str {
    match kind {
        "trace" => "get_trace",
        "event" => "get_event",
        "benchmark" => "get_benchmark",
        _ => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_is_honestly_empty() {
        assert_eq!(list()["resources"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn templates_cover_all_three_kinds() {
        let v = templates_list();
        let tpls = v["resourceTemplates"].as_array().unwrap();
        assert_eq!(tpls.len(), 3);
        for t in tpls {
            let uri = t["uriTemplate"].as_str().unwrap();
            assert!(uri.starts_with("lighttrack://"), "{uri}");
            assert!(uri.ends_with("/{id}"), "{uri}");
            assert_eq!(t["mimeType"], "text/markdown");
            assert!(t["description"].as_str().is_some());
        }
        let uris: Vec<&str> = tpls.iter().map(|t| t["uriTemplate"].as_str().unwrap()).collect();
        assert!(uris.contains(&"lighttrack://trace/{id}"));
        assert!(uris.contains(&"lighttrack://event/{id}"));
        assert!(uris.contains(&"lighttrack://benchmark/{id}"));
    }

    #[test]
    fn parse_uri_accepts_each_kind() {
        for (uri, kind, id) in [
            ("lighttrack://trace/tr-1", "trace", "tr-1"),
            ("lighttrack://event/ev-9", "event", "ev-9"),
            ("lighttrack://benchmark/bm-abc", "benchmark", "bm-abc"),
            // ids may contain slashes after the first segment split — keep the remainder intact.
            ("lighttrack://trace/a/b", "trace", "a/b"),
        ] {
            assert_eq!(parse_uri(uri).unwrap(), (kind, id));
        }
    }

    #[test]
    fn parse_uri_rejects_bad_shapes() {
        for bad in [
            "http://trace/1",              // wrong scheme
            "lighttrack://trace",          // no id segment
            "lighttrack:///id",            // empty kind
            "lighttrack://event/",         // empty id
        ] {
            assert!(parse_uri(bad).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn render_kind_maps_to_renderers() {
        assert_eq!(render_kind("trace"), "get_trace");
        assert_eq!(render_kind("event"), "get_event");
        assert_eq!(render_kind("benchmark"), "get_benchmark");
    }

    #[test]
    fn read_rejects_missing_uri_and_unknown_kind() {
        let c = Client::from_env();
        assert!(read(&c, &json!({})).unwrap_err().contains("uri"));
        // Unknown kind fails before any HTTP request.
        let err = read(&c, &json!({ "uri": "lighttrack://widget/1" })).unwrap_err();
        assert!(err.contains("unknown resource kind"), "{err}");
    }
}
