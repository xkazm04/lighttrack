//! `schedule`: periodically sample live events into frozen datasets (online sampling).
//!
//! Runs as a daemon (loop on `--interval`) or a single cycle (`--once`, for OS cron / Cloud
//! Scheduler / a systemd timer). Each cycle names the dataset after the newest sampled event, so it
//! is **idempotent**: if that window was already captured, the cycle is skipped — which means idle
//! periods (no new traffic) cost nothing, even across separate `--once` processes. And
//! `build_from_events` never creates an empty dataset.

use std::time::Duration;

use anyhow::Result;

use lighttrack_core::{Dataset, LlmEvent};
use lighttrack_engine::EngineConfig;

use crate::cli::Cli;
use crate::dataset::build_from_events;
use crate::http::get;
use crate::util::short;

#[allow(clippy::too_many_arguments)]
pub(crate) fn schedule(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    project: &str,
    interval: u64,
    once: bool,
    n: usize,
    name_prefix: &str,
    llm_scrub: bool,
) -> Result<()> {
    println!(
        "lt-runner schedule: sampling '{project}' every {interval}s (once={once}, n={n}, prefix={name_prefix})"
    );
    loop {
        match run_cycle(cli, http, engine, project, n, name_prefix, llm_scrub) {
            Ok(Some(name)) => println!("cycle: built dataset {name}"),
            Ok(None) => println!("cycle: no new events to sample; skipped"),
            // A failed cycle (e.g. API briefly down) must not kill the daemon.
            Err(e) => eprintln!("cycle error (continuing): {e}"),
        }
        if once {
            break;
        }
        std::thread::sleep(Duration::from_secs(interval));
    }
    Ok(())
}

/// One sampling cycle. Returns the new dataset name, or `None` if skipped (nothing new to sample, or
/// this window was already captured).
fn run_cycle(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    project: &str,
    n: usize,
    name_prefix: &str,
    llm_scrub: bool,
) -> Result<Option<String>> {
    let events: Vec<LlmEvent> = get(cli, http, &format!("/v1/events?project={project}&limit={n}"))?;
    // Watermark = newest event that carries an input (events come back newest-first).
    let name = match cycle_name(name_prefix, &events) {
        Some(n) => n,
        None => return Ok(None),
    };

    // Idempotent: if a dataset for this watermark already exists, this window is captured — skip.
    let existing: Vec<Dataset> = get(cli, http, &format!("/v1/projects/{project}/datasets"))?;
    if existing.iter().any(|d| d.name == name) {
        return Ok(None);
    }

    let built = build_from_events(cli, http, engine, project, &name, &events, llm_scrub)?;
    Ok((built > 0).then_some(name))
}

/// The watermark dataset name for a cycle: `<prefix>-<short id>` of the newest sampled event that
/// carries an input (events arrive newest-first), or `None` when nothing is samplable. Naming after
/// the watermark is what makes a cycle idempotent — re-sampling the same window yields the same name.
fn cycle_name(name_prefix: &str, events: &[LlmEvent]) -> Option<String> {
    events
        .iter()
        .find(|e| e.input.is_some())
        .map(|e| format!("{name_prefix}-{}", short(&e.id)))
}

#[cfg(test)]
mod tests {
    use super::cycle_name;
    use lighttrack_core::LlmEvent;
    use serde_json::json;

    fn event(id: &str, input: Option<&str>) -> LlmEvent {
        let mut v = json!({ "id": id, "provider": "anthropic", "model": "m" });
        if let Some(i) = input {
            v["input"] = json!(i);
        }
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn names_after_first_event_with_input() {
        let events = vec![
            event("noinput0", None),
            event("abcdef0123456789", Some("hello")),
            event("xyz", Some("later")),
        ];
        // Newest-first: the first event carrying an input wins; id is shortened to 8 chars.
        assert_eq!(cycle_name("online", &events).as_deref(), Some("online-abcdef01"));
    }

    #[test]
    fn none_when_no_event_has_input() {
        let events = vec![event("a", None), event("b", None)];
        assert_eq!(cycle_name("online", &events), None);
        assert_eq!(cycle_name("online", &[]), None);
    }

    #[test]
    fn honors_custom_prefix() {
        let events = vec![event("deadbeefcafe", Some("x"))];
        assert_eq!(cycle_name("nightly", &events).as_deref(), Some("nightly-deadbeef"));
    }
}
