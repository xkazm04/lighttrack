//! Runnable example for the LightTrack Rust client.
//!
//! Start the API first, then from `clients/rust/`:
//!     cargo run --example quickstart                         # dev mode: project "demo"
//!     LIGHTTRACK_KEY=lt_... cargo run --example quickstart   # enforced: project from the key

use lighttrack_client::{Client, Provider};
use serde_json::json;

fn main() {
    // from_env reads LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT (empty values are ignored).
    // In dev mode set LIGHTTRACK_PROJECT (e.g. "demo"); a project key derives the project server-side.
    let lt = Client::from_env().source("rust-example");

    // Fluent builder.
    lt.event(Provider::OpenAi, "gpt-4o-mini")
        .input_tokens(120)
        .output_tokens(45)
        .cached_input(64)
        .latency_ms(210)
        .trace_id("t-1")
        .tag("demo")
        .send();

    lt.event(Provider::Anthropic, "claude-haiku-4-5")
        .input_tokens(200)
        .output_tokens(80)
        .latency_ms(540)
        .send();

    // From a provider response JSON value.
    let openai_resp = json!({"model": "gpt-4o", "usage": {"prompt_tokens": 10, "completion_tokens": 5}});
    lt.track_openai_json(&openai_resp, None);

    lt.flush(); // drain + join the background worker
    println!("sent 3 events — check: GET /v1/events and /v1/costs");
}
