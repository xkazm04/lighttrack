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
    (
        "trace",
        "/v1/traces/",
        "One agent trace: rolled-up totals, the span tree, and any scores within it.",
    ),
    (
        "event",
        "/v1/events/",
        "One LLM call event: provider, model, tokens, cost, latency, and status.",
    ),
    (
        "benchmark",
        "/v1/benchmarks/",
        "One benchmark definition: rubric, judge model, dataset, and baseline.",
    ),
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
    let (_, prefix, _) = KINDS.iter().find(|(k, _, _)| *k == kind).ok_or_else(|| {
        format!("unknown resource kind '{kind}' — expected one of trace, event, benchmark")
    })?;

    let body = c.get(&format!("{prefix}{id}"))?;
    let markdown = lighttrack_render::render(render_kind(kind), &body)
        .unwrap_or_else(|| serde_json::to_string_pretty(&body).unwrap_or_default());
    let raw_json = render_raw_json(&body);

    Ok(json!({
        "contents": [
            { "uri": uri, "mimeType": "text/markdown", "text": markdown },
            { "uri": uri, "mimeType": "application/json", "text": raw_json }
        ]
    }))
}

/// The size at which the raw-JSON content item switches to its elided form.
///
/// This is a **trigger threshold, not an output ceiling**, and the distinction is measured
/// rather than assumed: a 200-span trace carrying 400-byte payloads serializes to 206,644
/// bytes whole and 43,628 bytes elided-and-compact — a 4.7x reduction that still exceeds this
/// number. Elision bounds the payload per span; it cannot bound a span count that is itself
/// unbounded. Capping the number of spans (with a `...{n} spans elided...` marker, the way the
/// span-scoring harness does) is the separate change that would make this a true ceiling.
const MAX_RESOURCE_JSON: usize = 24 * 1024;

/// Serialize the body for the raw-JSON content item, eliding payload-bearing fields when the
/// compact form is over budget.
///
/// The structural view — ids, models, tokens, cost, status, the span tree's shape — is what
/// makes this item useful, and it is small. What blows the budget is `input`/`output` on each
/// span's event: whole prompts and whole completions, repeated per span.
///
/// Those are **recoverable**: every span carries the event id that `get_event` reads, so the
/// bytes are one tool call away. So they are replaced with a pointer that says how to get them
/// back, rather than summarized (which would lose them) or truncated (which would lie about
/// having them). The marker carries the original byte count, because "how much is missing" is
/// the fact a caller needs to decide whether to fetch.
fn render_raw_json(body: &Value) -> String {
    let compact = serde_json::to_string(body).unwrap_or_default();
    if compact.len() <= MAX_RESOURCE_JSON {
        return serde_json::to_string_pretty(body).unwrap_or_default();
    }
    // Over budget, two things change together. Payload-bearing fields are elided, and the
    // output is serialized COMPACT: pretty-printing a few hundred spans costs tens of
    // kilobytes in indentation alone, so eliding while still pretty-printing measurably
    // fails to reach the budget. Readability is what the Markdown content item is for.
    let mut elided = body.clone();
    elide_payloads(&mut elided);
    serde_json::to_string(&elided).unwrap_or_default()
}

/// Replace `input`/`output` values under any `event` object with a re-fetch marker, in place.
///
/// Walks the whole tree rather than a fixed path: the span tree nests, and a shape-specific
/// walk would silently stop eliding the day the response gains a level.
fn elide_payloads(node: &mut Value) {
    match node {
        Value::Object(map) => {
            if let Some(Value::Object(event)) = map.get_mut("event") {
                for field in ["input", "output"] {
                    if let Some(v) = event.get_mut(field) {
                        if !v.is_null() {
                            let bytes = serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
                            *v = Value::String(format!(
                                "<elided: {bytes} bytes — fetch via get_event>"
                            ));
                        }
                    }
                }
            }
            for (_, v) in map.iter_mut() {
                elide_payloads(v);
            }
        }
        Value::Array(items) => {
            for v in items.iter_mut() {
                elide_payloads(v);
            }
        }
        _ => {}
    }
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
        return Err(format!(
            "resource uri is missing a kind or id (got `{uri}`)"
        ));
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

    /// A trace body with `n` spans, each carrying a `bytes`-sized input and output.
    fn trace_with_spans(n: usize, bytes: usize) -> Value {
        let blob = "x".repeat(bytes);
        let spans: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "span_id": format!("s{i}"),
                    "name": "llm.call",
                    "event": {
                        "id": format!("e{i}"),
                        "model": "some-model",
                        "tokens": 1234,
                        "cost_usd": 0.01,
                        "input": blob,
                        "output": blob,
                    }
                })
            })
            .collect();
        json!({ "trace_id": "t1", "total_cost_usd": 1.0, "spans": spans })
    }

    #[test]
    fn small_body_is_emitted_whole() {
        let body = trace_with_spans(2, 16);
        let out = render_raw_json(&body);
        assert!(!out.contains("elided"), "small body must not be elided");
        assert!(out.contains("\"input\""));
        let v: Value = serde_json::from_str(&out).expect("still valid json");
        assert_eq!(v["spans"][0]["event"]["input"], "x".repeat(16));
    }

    #[test]
    fn over_budget_body_elides_payloads_to_a_refetch_pointer() {
        let body = trace_with_spans(200, 400);
        let out = render_raw_json(&body);
        let v: Value = serde_json::from_str(&out).expect("elided output is still valid json");

        // The structural view survives whole - that is the point of eliding rather than truncating.
        assert_eq!(v["trace_id"], "t1");
        assert_eq!(v["spans"].as_array().unwrap().len(), 200);
        assert_eq!(v["spans"][0]["event"]["model"], "some-model");
        assert_eq!(v["spans"][0]["event"]["tokens"], 1234);

        // The payload is replaced by a pointer naming the way back, carrying its own size.
        let input = v["spans"][0]["event"]["input"].as_str().unwrap();
        assert!(input.starts_with("<elided: "), "{input}");
        assert!(input.contains("fetch via get_event"), "{input}");
        assert!(
            input.contains("402"),
            "marker carries the original byte count: {input}"
        );
        assert!(v["spans"][199]["event"]["output"]
            .as_str()
            .unwrap()
            .contains("get_event"));
    }

    /// The paired measurement this change was made for: same input, both arms, one instrument.
    #[test]
    fn elision_brings_a_large_trace_under_budget() {
        let body = trace_with_spans(200, 400);

        let arm_a = serde_json::to_string_pretty(&body).unwrap(); // previous behaviour
        let arm_b = render_raw_json(&body); // current behaviour

        assert!(
            arm_a.len() > 5 * MAX_RESOURCE_JSON,
            "arm A is {} bytes - the case that motivated the budget",
            arm_a.len()
        );
        eprintln!(
            "paired measurement: arm A {} bytes -> arm B {} bytes ({:.1}% of A)",
            arm_a.len(),
            arm_b.len(),
            100.0 * arm_b.len() as f64 / arm_a.len() as f64
        );
        // Measured 4.7x on this fixture; the floor is set at 4x so normal drift in the
        // structural fields does not turn a real regression into a judgement call.
        assert!(
            arm_b.len() * 4 < arm_a.len(),
            "arm B {} must be at least 4x smaller than arm A {}",
            arm_b.len(),
            arm_a.len()
        );
        // Pins the honest residual: elision is a large, real reduction that does NOT reach the
        // trigger threshold for a trace this wide, because the per-span structure is unbounded
        // in span count. If a later change caps spans, this assertion is the one to tighten.
        assert!(
            arm_b.len() > MAX_RESOURCE_JSON,
            "arm B {} unexpectedly fits the threshold - if spans are now capped, tighten this              test to assert the ceiling instead",
            arm_b.len()
        );
    }

    #[test]
    fn elision_reaches_nested_spans() {
        let body = json!({
            "trace_id": "t1",
            "spans": [{
                "span_id": "s0",
                "children": [{
                    "span_id": "s1",
                    "event": { "id": "e1", "input": "y".repeat(64), "output": null }
                }]
            }]
        });
        let mut v = body.clone();
        elide_payloads(&mut v);
        assert!(v["spans"][0]["children"][0]["event"]["input"]
            .as_str()
            .unwrap()
            .contains("get_event"));
        // A null payload is absent, not elided - nothing to fetch back.
        assert!(v["spans"][0]["children"][0]["event"]["output"].is_null());
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
        let uris: Vec<&str> = tpls
            .iter()
            .map(|t| t["uriTemplate"].as_str().unwrap())
            .collect();
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
            "http://trace/1",      // wrong scheme
            "lighttrack://trace",  // no id segment
            "lighttrack:///id",    // empty kind
            "lighttrack://event/", // empty id
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
