//! The endpoint probe against a stub that speaks real HTTP.
//!
//! The resolution rules are unit-tested beside them in `lighttrack_core::endpoint_identity`. What
//! only a wire test can show is the joint this module owns: that the routes are built on the right
//! origin, that a 404 on a native route is read as "not this implementation" rather than as an
//! error, and — the one that matters — that a multiplexer forwarding a runtime's own route is
//! reported as unresolvable rather than as that runtime.
//!
//! Same hand-rolled loopback stub as `provider_boundary.rs`, and for the same reason: the engine's
//! client is blocking `reqwest`, so every mock crate would drag a tokio runtime into a crate that
//! has none. This one routes by request path.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;

use lighttrack_core::{Endpoint, Evidence};
use lighttrack_engine::endpoint_probe::probe;

/// A loopback endpoint answering a fixed `path -> body` table; everything else is a 404.
fn stub(routes: &'static [(&'static str, &'static str)]) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { continue };
            let mut reader = BufReader::new(sock.try_clone().expect("clone socket"));
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
                continue;
            }
            // Drain the headers so the client sees our status, not a reset.
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or("/");
            let hit = routes.iter().find(|(p, _)| *p == path);
            let (status, body) = match hit {
                Some((_, body)) => (200, *body),
                None => (404, "not found"),
            };
            let resp = format!(
                "HTTP/1.1 {status} STUB\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
            let _ = sock.flush();
        }
    });
    base
}

const OLLAMA_TAGS: &str = r#"{"models":[{"name":"qwen3:8b","digest":"sha256:ab12"}]}"#;

#[test]
fn a_native_route_identifies_the_runtime_over_the_wire() {
    let base = stub(&[("/api/tags", OLLAMA_TAGS)]);
    let id = probe(&base, "2026-09-03");
    assert_eq!(
        id.endpoint,
        Endpoint::Runtime {
            name: "ollama".into()
        }
    );
    assert_eq!(id.evidence, Evidence::NativeRoute);
    assert!(id.established());
    // …and the trailing-`/v1` spelling reaches the same origin.
    assert_eq!(probe(&format!("{base}/v1"), "2026-09-03"), id);
}

#[test]
fn an_empty_inventory_falls_through_to_the_banner() {
    // The fresh-install shape: the protocol answers, correctly, with nothing in it.
    let base = stub(&[
        ("/v1/models", r#"{"object":"list","data":[]}"#),
        ("/", "Ollama is running"),
    ]);
    let id = probe(&base, "2026-09-03");
    assert_eq!(
        id.endpoint,
        Endpoint::Runtime {
            name: "ollama".into()
        }
    );
    assert_eq!(id.evidence, Evidence::RootBanner);
}

#[test]
fn the_model_listing_is_read_when_no_native_route_answers() {
    let base = stub(&[(
        "/v1/models",
        r#"{"object":"list","data":[{"id":"m","owned_by":"vllm"}]}"#,
    )]);
    let id = probe(&base, "2026-09-03");
    assert_eq!(
        id.endpoint,
        Endpoint::Runtime {
            name: "vllm".into()
        }
    );
    assert_eq!(id.evidence, Evidence::OwnedBy);
}

#[test]
fn a_multiplexer_forwarding_a_runtimes_route_is_not_that_runtime() {
    // The falsifier the design had to survive: a proxy fronting several runtimes, one of whose
    // native routes it happens to forward. Answering `ollama` here would publish a row claiming a
    // program we did not establish was the one measured.
    let base = stub(&[
        ("/api/tags", OLLAMA_TAGS),
        ("/health/liveliness", r#"{"status":"I'm alive!"}"#),
        ("/", "Ollama is running"),
    ]);
    let id = probe(&base, "2026-09-03");
    assert_eq!(
        id.endpoint,
        Endpoint::Multiplexed {
            name: "litellm".into()
        }
    );
    assert!(!id.established());
    assert_eq!(
        id.collective_provider().as_deref(),
        Some("self-hosted.unresolved")
    );
}

#[test]
fn an_endpoint_that_says_nothing_is_unrecognized_not_guessed() {
    let base = stub(&[("/v1/chat/completions", "{}")]);
    let id = probe(&base, "2026-09-03");
    assert_eq!(id.endpoint, Endpoint::Unrecognized);
    assert_eq!(id.evidence, Evidence::NoEvidence);
    assert_eq!(id.probed_on, "2026-09-03");
}

#[test]
fn an_unreachable_endpoint_resolves_rather_than_erroring() {
    // A closed port: every rung fails fast, and the answer is a state, not an error.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let dead = format!("http://{}", listener.local_addr().unwrap());
    drop(listener);
    assert_eq!(probe(&dead, "2026-09-03").endpoint, Endpoint::Unrecognized);
}
