//! `schedule`: periodically sample live events into frozen datasets (online sampling).
//!
//! Runs as a daemon (loop on `--interval`) or a single cycle (`--once`, for OS cron / Cloud
//! Scheduler / a systemd timer). Each cycle names the dataset after the newest sampled event, so it
//! is **idempotent**: if that window was already captured, the cycle is skipped — which means idle
//! periods (no new traffic) cost nothing, even across separate `--once` processes. Only a frozen
//! dataset counts as captured: `build_from_events` creates the dataset before its items and freezes
//! it last, so a failed cycle can leave an unfrozen, partial or empty one behind under the name.

use std::time::Duration;

use anyhow::Result;

use lighttrack_core::{Dataset, ImportSpec, LlmEvent};
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
        // `--interval 0` on a daemon is a hot loop against the API, not "as fast as possible" —
        // the one-shot spelling is `--once`. Floored to a second, the same as `serve`.
        std::thread::sleep(Duration::from_secs(interval.max(1)));
    }
    Ok(())
}

/// One sampling cycle. Returns the new dataset name, or `None` if skipped (nothing new to sample, or
/// this window was already captured).
///
/// `pub(crate)` because it is also what a `dataset_sample` job runs: the queue executes one cycle
/// of the same thing this daemon loops over, rather than a second implementation of it.
pub(crate) fn run_cycle(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    project: &str,
    n: usize,
    name_prefix: &str,
    llm_scrub: bool,
) -> Result<Option<String>> {
    let events: Vec<LlmEvent> = get(
        cli,
        http,
        &format!("/v1/events?project={project}&limit={n}"),
    )?;
    // Watermark = newest event that carries an input (events come back newest-first).
    let name = match cycle_name(name_prefix, &events) {
        Some(n) => n,
        None => return Ok(None),
    };

    // Idempotent: if a FROZEN dataset for this watermark exists, this window is captured — skip.
    let existing: Vec<Dataset> = get(cli, http, &format!("/v1/projects/{project}/datasets"))?;
    if window_captured(&existing, &name) {
        return Ok(None);
    }

    let built = build_from_events(cli, http, engine, project, &name, &events, llm_scrub)?;
    Ok((built > 0).then_some(name))
}

/// One **versioned** sampling cycle (M24): open (or fork) the newest version of `name` and mine
/// `spec` into it, then freeze it.
///
/// This is what the watermark naming was a workaround for. Naming each cycle's dataset after its
/// newest event made a cycle idempotent, at the cost of leaving a year of online sampling as ~300
/// unrelated corpora that no `dataset_version` could relate — so the paired-test guard `dataset_pin`
/// feeds had nothing to compare. One name accumulating versions gives it something: v1 frozen after
/// its window, v2 forked from it for the next.
///
/// Idempotence survives the change and gets stronger: `spec.dedupe` collapses a case whose
/// normalised input is already in the set, so re-running a cycle over an overlapping window adds the
/// genuinely new traffic and nothing else — rather than skipping the window wholesale.
pub(crate) fn run_versioned_cycle(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    project: &str,
    name: &str,
    spec: &ImportSpec,
) -> Result<Option<String>> {
    let built = crate::dataset_import::run_import(cli, http, project, name, spec, true)?;
    Ok((built > 0).then(|| name.to_string()))
}

/// Whether this cycle's window is already captured, so the cycle may skip.
///
/// The name alone is not the completion. `build_from_events` creates the dataset before it posts a
/// single item and freezes it as its last act, so a cycle that failed part-way (an item post, or
/// the LLM scrub on the first event) leaves a dataset carrying this window's name that is unfrozen
/// and partial or empty. Skipping on the name would record that failure as "already captured" on
/// every later cycle over the same window. Only a frozen dataset certifies the window; a leftover
/// is rebuilt beside, and the error that left it already named it for the operator.
fn window_captured(existing: &[Dataset], name: &str) -> bool {
    existing.iter().any(|d| d.name == name && d.frozen)
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
    use super::{cycle_name, window_captured};
    use lighttrack_core::{Dataset, LlmEvent};
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
        assert_eq!(
            cycle_name("online", &events).as_deref(),
            Some("online-abcdef01")
        );
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
        assert_eq!(
            cycle_name("nightly", &events).as_deref(),
            Some("nightly-deadbeef")
        );
    }
    fn dataset(name: &str, frozen: bool) -> Dataset {
        serde_json::from_value(json!({ "name": name, "frozen": frozen })).unwrap()
    }

    #[test]
    fn frozen_dataset_captures_the_window() {
        let existing = vec![dataset("online-abcdef01", true)];
        assert!(window_captured(&existing, "online-abcdef01"));
    }

    #[test]
    fn no_dataset_for_the_watermark_does_not_capture() {
        let existing = vec![dataset("online-other000", true)];
        assert!(!window_captured(&existing, "online-abcdef01"));
        assert!(!window_captured(&[], "online-abcdef01"));
    }

    #[test]
    fn unfrozen_leftover_does_not_capture_the_window() {
        // `build_from_events` creates the dataset before posting items and freezes it last, so a
        // failed cycle (an item post, or the LLM scrub on the first event) leaves this row behind:
        // named after the window, unfrozen, partial or empty. Its name is not the completion.
        let existing = vec![dataset("online-abcdef01", false)];
        assert!(!window_captured(&existing, "online-abcdef01"));
    }

    #[test]
    fn a_frozen_twin_beside_a_leftover_still_captures() {
        let existing = vec![
            dataset("online-abcdef01", false),
            dataset("online-abcdef01", true),
        ];
        assert!(window_captured(&existing, "online-abcdef01"));
    }
}
