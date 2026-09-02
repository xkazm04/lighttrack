//! Round-trip against a **local axum stub**: an HTTP benchmark target really is called, really is
//! signed, and its answer really comes back as a `GenOutcome`.
//!
//! No live service and no model call — the whole point of the `Http` target kind is that the thing
//! under test is the operator's own endpoint, so the test owns one.

use std::net::SocketAddr;
use std::sync::mpsc;

use axum::{extract::Json, http::HeaderMap, routing::post, Router};
use lighttrack_engine::http_target::{
    generate_http, sign, HttpTargetRequest, SECRET_ENV, SIGNATURE_HEADER,
};
use serde_json::{json, Value};

/// What the stub saw, so the test can assert on the request as well as the response.
#[derive(Debug)]
struct Seen {
    body: Value,
    signature: Option<String>,
}

/// Start the stub on an ephemeral port; returns its base URL and a receiver of what it saw.
fn spawn_stub() -> (String, mpsc::Receiver<Seen>) {
    let (tx, rx) = mpsc::channel::<Seen>();
    let (addr_tx, addr_rx) = mpsc::channel::<SocketAddr>();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("stub runtime");
        rt.block_on(async move {
            let app = Router::new().route(
                "/answer",
                post(move |headers: HeaderMap, Json(body): Json<Value>| {
                    let tx = tx.clone();
                    async move {
                        let signature = headers
                            .get(SIGNATURE_HEADER)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_string);
                        let input = body
                            .get("input")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let _ = tx.send(Seen { body, signature });
                        Json(json!({
                            "output": format!("pipeline answer to: {input}"),
                            "usage": { "prompt_tokens": 11, "completion_tokens": 4 },
                            "latency_ms": 42
                        }))
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind stub");
            addr_tx
                .send(listener.local_addr().expect("stub addr"))
                .expect("hand back addr");
            axum::serve(listener, app).await.expect("serve stub");
        });
    });
    let addr = addr_rx.recv().expect("stub started");
    (format!("http://{addr}"), rx)
}

#[test]
fn an_http_target_is_called_signed_and_its_answer_comes_back() {
    // Both halves of this test share one process-wide env var, so they share one test.
    std::env::set_var(SECRET_ENV, "s3cret");
    let (base, seen) = spawn_stub();
    let url = format!("{base}/answer");

    let out = generate_http(
        &url,
        Some("you are terse"),
        "what is our refund window?",
        Some("30 days"),
    )
    .expect("the stub answers");

    assert_eq!(out.output, "pipeline answer to: what is our refund window?");
    assert_eq!(out.input_tokens, Some(11));
    assert_eq!(out.output_tokens, Some(4));
    assert_eq!(out.latency_ms, Some(42), "the endpoint's own figure wins");
    assert!(
        out.cost_usd.is_none(),
        "an endpoint that reports no cost stays honestly unpriced — the caller prices it from the \
         book by tokens, and never invents a number"
    );
    assert_eq!(out.model, url, "the endpoint identifies itself");

    let got = seen.recv().expect("the stub was called");
    assert_eq!(
        got.body,
        json!({
            "input": "what is our refund window?",
            "expected": "30 days",
            "system_prompt": "you are terse"
        }),
        "the case, its reference answer and the resolved prompt all reach the pipeline"
    );
    // The signature is over the EXACT bytes we sent (re-serializing the parsed value would reorder
    // the keys and no longer verify), so the endpoint can attribute our traffic.
    let body = serde_json::to_vec(&HttpTargetRequest {
        input: "what is our refund window?",
        expected: Some("30 days"),
        system_prompt: Some("you are terse"),
    })
    .expect("the bytes we sent");
    assert_eq!(
        got.signature.as_deref(),
        Some(sign("s3cret", &body).expect("sign").as_str()),
        "signed with LIGHTTRACK_HTTP_TARGET_SECRET over the request body"
    );

    // Without a secret the header is simply absent — never present-but-meaningless.
    std::env::remove_var(SECRET_ENV);
    generate_http(&url, None, "again", None).expect("still answers");
    let got = seen.recv().expect("second call");
    assert!(got.signature.is_none(), "no secret, no signature header");
    assert_eq!(got.body, json!({ "input": "again" }));
}
