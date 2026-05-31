# lighttrack-client (Rust)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/tracklight).
Reuses `lighttrack-core`'s `LlmEvent` as the wire type, so the payload can never drift from the API.
Sends go to a background worker thread and never block or panic the caller.

This crate is **detached from the main workspace** (its own `[workspace]`), so it builds and versions
independently and is not pulled into the server build.

## Add the dependency

```toml
[dependencies]
lighttrack-client = { git = "https://github.com/xkazm04/tracklight", subdir = "clients/rust" }
# or a path dep when vendored:
# lighttrack-client = { path = "../tracklight/clients/rust" }
```

## Use

```rust
use lighttrack_client::{Client, Provider};

let lt = Client::from_env().source("my-app");   // LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT

lt.event(Provider::OpenAi, "gpt-4o")
    .input_tokens(120).output_tokens(45).latency_ms(120)
    .send();

// or from a serde_json::Value provider response:
lt.track_openai_json(&resp_json, None);   // also: track_anthropic_json, track_gemini_json

lt.flush();   // drain + join the background worker before exit (Drop does this too)
```

Run the demo from this directory: `cargo run --example quickstart` (start the API first). See the
repo's `clients/README.md` for the full field-mapping table and design notes.
