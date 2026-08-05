# lighttrack-client (Rust)

Fire-and-forget client for ingesting LLM call events into [LightTrack](https://github.com/xkazm04/lighttrack).
Reuses `lighttrack-core`'s `LlmEvent` as the wire type, so the payload can never drift from the API.
Sends go to a background worker thread and never block or panic the caller.

This crate is **detached from the main workspace** (its own `[workspace]`), so it builds and versions
independently and is not pulled into the server build.

## Add the dependency

```toml
[dependencies]
lighttrack-client = { git = "https://github.com/xkazm04/lighttrack", subdir = "clients/rust" }
# or a path dep when vendored:
# lighttrack-client = { path = "../lighttrack/clients/rust" }
```

## Configure

**Where do my events land?** Every event is attributed to a project. A **project key** pins it
server-side; otherwise the event has to name one. With neither, a **dev-mode** server files events
under a `default` project — fine for a first run — while a server with **authentication enabled**
rejects them. Set one of these as soon as you want events somewhere specific:

```bash
export LIGHTTRACK_URL=http://127.0.0.1:8787   # default; override for a remote server
export LIGHTTRACK_PROJECT=demo                # choose the project (also needed with an admin key)
# ...or instead: export LIGHTTRACK_KEY=lt_...  # a project key pins the project server-side
```

## Use

```rust
use lighttrack_client::{Client, Provider};

let lt = Client::from_env().source("my-app");   // LIGHTTRACK_URL / LIGHTTRACK_KEY / LIGHTTRACK_PROJECT
// ...or pass it explicitly, no env needed:
// let lt = Client::new("http://127.0.0.1:8787", None, Some("demo".into()));

lt.event(Provider::OpenAi, "gpt-4o")
    .input_tokens(120).output_tokens(45).latency_ms(120)
    .send();

// or from a serde_json::Value provider response:
lt.track_openai_json(&resp_json, None);   // also: track_anthropic_json, track_gemini_json

lt.flush();   // drain + join the background worker before exit (Drop does this too)
```

Run the demo from this directory: `cargo run --example quickstart` (start the API first). See the
repo's `clients/README.md` for the full field-mapping table and design notes.

## Why don't I see my events?

Sends never panic and never block — but they are not silent. A failed send writes one actionable line
to **stderr** (never stdout, which your app may be using as a protocol channel):

```
[lighttrack] no project is configured, so these events are not attributed: a dev-mode server files
them under the 'default' project, and a server with authentication enabled rejects them. To choose
where they land, set LIGHTTRACK_PROJECT=<your-project-id> ...
```

That case is reported *before* the request, so it appears on your very first `send()`. Warnings are
rate-limited to one line per error kind per 60 s, so a hot loop costs one line, not thousands.

Silence them with `LIGHTTRACK_QUIET=1` or `Client::from_env().quiet(true)`.

## Test

```bash
cd clients/rust && cargo test   # this crate is its own workspace: build it from here, not the repo root
```
