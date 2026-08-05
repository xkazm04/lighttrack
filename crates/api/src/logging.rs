//! Structured diagnostics for the API server.
//!
//! **Why JSON on stdout.** Every host this service is meant to run on — Cloud Run, Docker, k8s, Fly,
//! systemd — already collects a process's stdout and, when a line parses as JSON, indexes its keys as
//! queryable fields. A human-formatted `eprintln!` becomes one opaque string per line: you can grep it
//! and nothing else. Emitting `{"level":"WARN","project_id":"acme",…}` means "show me every breach on
//! project acme in the last hour" is a filter, not a regex. No sidecar, no agent, no collector.
//!
//! This is the opposite of the `lt-mcp` invariant (all diagnostics to **stderr**, because *its* stdout
//! carries JSON-RPC frames) and does not conflict with it: nothing in this crate is linked into that
//! binary, and the one shared surface — the alert channels — writes through `tracing`, whose sink is
//! whatever subscriber the host binary installed.
//!
//! Env:
//!   `LIGHTTRACK_LOG`         level or full `tracing-subscriber` filter directive (default `info`).
//!                            Falls back to `RUST_LOG` when unset, so the usual Rust reflex works.
//!   `LIGHTTRACK_LOG_FORMAT`  `json` (default) | `text` — `text` is the readable local-dev form.

use tracing_subscriber::{fmt, EnvFilter};

const ENV_LEVEL: &str = "LIGHTTRACK_LOG";
const ENV_FORMAT: &str = "LIGHTTRACK_LOG_FORMAT";
const DEFAULT_LEVEL: &str = "info";

/// Install the process-wide subscriber. Called once, from `main`'s wiring.
pub(crate) fn init() {
    let filter = EnvFilter::try_new(directive(
        std::env::var(ENV_LEVEL).ok().as_deref(),
        std::env::var("RUST_LOG").ok().as_deref(),
    ))
    // A typo'd directive must not silence the server — an operator who mistypes a filter should get
    // the default, not a mute process they then have to debug without logs.
    .unwrap_or_else(|_| EnvFilter::new(DEFAULT_LEVEL));

    let builder = fmt::Subscriber::builder()
        .with_env_filter(filter)
        .with_writer(std::io::stdout);

    // `try_init` rather than `init`: a second call (a test harness, an embedder) must not abort a
    // running server over its logging setup.
    if wants_json(std::env::var(ENV_FORMAT).ok().as_deref()) {
        let _ = builder
            .json()
            // Fields at the top level rather than nested under "fields": that is the shape Cloud
            // Logging / Loki / Datadog auto-index without a parsing rule.
            .flatten_event(true)
            .with_current_span(false)
            .with_span_list(false)
            .try_init();
    } else {
        let _ = builder.try_init();
    }
}

/// Which filter directive wins: explicit `LIGHTTRACK_LOG`, else `RUST_LOG`, else the default. Blank
/// values are treated as unset (an exported-but-empty var is a deployment accident, not "log nothing").
fn directive(explicit: Option<&str>, rust_log: Option<&str>) -> String {
    [explicit, rust_log]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(DEFAULT_LEVEL)
        .to_string()
}

/// JSON unless the operator explicitly asked for the human form. Unrecognized values keep JSON: the
/// machine-readable shape is the one a host depends on, so a typo must not silently break ingestion.
fn wants_json(format: Option<&str>) -> bool {
    !matches!(
        format.map(str::trim).unwrap_or_default().to_ascii_lowercase().as_str(),
        "text" | "plain" | "human" | "pretty" | "compact"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_beats_rust_log_and_blanks_fall_through() {
        assert_eq!(directive(Some("debug"), Some("trace")), "debug");
        assert_eq!(directive(None, Some("trace")), "trace");
        assert_eq!(directive(None, None), DEFAULT_LEVEL);
        // Exported-but-empty is an accident (`ENV=${MISSING}` in a compose file), not an instruction.
        assert_eq!(directive(Some("  "), Some("warn")), "warn");
        assert_eq!(directive(Some(""), None), DEFAULT_LEVEL);
        // Full directives pass through untouched.
        assert_eq!(directive(Some("info,lighttrack_api::events=debug"), None), "info,lighttrack_api::events=debug");
    }

    #[test]
    fn json_is_the_default_and_only_an_explicit_human_format_opts_out() {
        assert!(wants_json(None));
        assert!(wants_json(Some("json")));
        assert!(wants_json(Some("")));
        assert!(wants_json(Some("jsno")), "a typo must not silently break log ingestion");
        assert!(!wants_json(Some("text")));
        assert!(!wants_json(Some("TEXT")));
        assert!(!wants_json(Some(" pretty ")));
    }

    #[test]
    fn the_default_directive_parses() {
        // Guards the fallback path in `init`: if this ever stopped parsing, a mistyped filter would
        // land on an `EnvFilter::new` that panics instead of a working default.
        assert!(EnvFilter::try_new(DEFAULT_LEVEL).is_ok());
    }
}
