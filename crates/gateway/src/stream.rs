//! `stream: true` — a finished answer delivered as server-sent `chat.completion.chunk` events.
//!
//! Honest about what it is: the CLIs behind this gateway answer whole, so nothing arrives before
//! the model is done. What streaming buys an app is protocol compatibility — an SDK call written
//! against a streaming endpoint keeps working — and, on a long answer, a body that starts
//! rendering as it is read. The chunks are the same completion, split on whitespace.

use axum::body::Body;
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use serde_json::{json, Value};

/// Roughly how many characters go in one content chunk.
const CHUNK_CHARS: usize = 48;

/// Split `text` into chunks on whitespace boundaries, each around [`CHUNK_CHARS`] long.
pub fn chunks(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split_inclusive(char::is_whitespace) {
        cur.push_str(word);
        if cur.len() >= CHUNK_CHARS {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// Turn a finished `chat.completion` body into the SSE stream an OpenAI client expects.
pub fn response(completion: &Value) -> Response {
    let id = completion["id"].clone();
    let created = completion["created"].clone();
    let model = completion["model"].clone();
    let message = &completion["choices"][0]["message"];
    let finish_reason = completion["choices"][0]["finish_reason"].clone();

    let chunk = |delta: Value, finish: Value| {
        json!({
            "id": id, "object": "chat.completion.chunk", "created": created, "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }]
        })
    };

    let mut events: Vec<Value> = vec![chunk(
        json!({ "role": "assistant", "content": "" }),
        Value::Null,
    )];
    if let Some(text) = message["content"].as_str() {
        for piece in chunks(text) {
            events.push(chunk(json!({ "content": piece }), Value::Null));
        }
    }
    if let Some(calls) = message.get("tool_calls").filter(|v| !v.is_null()) {
        // Tool calls go whole, indexed as the streaming shape wants them.
        let indexed: Vec<Value> = calls
            .as_array()
            .map(|a| {
                a.iter()
                    .enumerate()
                    .map(|(i, c)| {
                        let mut c = c.clone();
                        c["index"] = json!(i);
                        c
                    })
                    .collect()
            })
            .unwrap_or_default();
        events.push(chunk(json!({ "tool_calls": indexed }), Value::Null));
    }
    events.push(chunk(json!({}), finish_reason));
    // The last chunk carries usage and the gateway's own block, as `stream_options:
    // {include_usage}` clients expect a trailing usage-only chunk.
    events.push(json!({
        "id": completion["id"], "object": "chat.completion.chunk", "created": completion["created"],
        "model": completion["model"], "choices": [],
        "usage": completion["usage"], "lighttrack": completion["lighttrack"],
    }));

    let mut body = String::new();
    for ev in events {
        body.push_str("data: ");
        body.push_str(&ev.to_string());
        body.push_str("\n\n");
    }
    body.push_str("data: [DONE]\n\n");

    let mut resp = (StatusCode::OK, Body::from(body)).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        header::HeaderValue::from_static("text/event-stream"),
    );
    resp.headers_mut().insert(
        header::CACHE_CONTROL,
        header::HeaderValue::from_static("no-cache"),
    );
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunks_split_on_whitespace_and_reassemble_exactly() {
        let text = "the quick brown fox jumps over the lazy dog ".repeat(4);
        let cs = chunks(&text);
        assert!(cs.len() > 1);
        assert!(cs.iter().all(|c| c.ends_with(' ')));
        assert_eq!(cs.concat(), text);
        assert_eq!(chunks(""), Vec::<String>::new());
        assert_eq!(chunks("one"), vec!["one".to_string()]);
    }
}
