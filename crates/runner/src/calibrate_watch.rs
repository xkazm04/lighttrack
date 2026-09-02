//! `calibrate --watch`: the **judge-drift sentinel**. Re-judges a pinned golden set on a schedule,
//! persists each cycle's agreement (κ/MAE/bias/trusted) through the existing scores API under a
//! reserved rubric, and warns when the judge's trust degrades — so a silently-worsening judge (model
//! update, prompt change) is caught instead of surfacing later as weird benchmarks.
//!
//! Since M11 the durable fact is a `CalibrationRecord` in the `calibrations` table, and the
//! reserved-rubric `lt:calibration:<provider>/<model>` score is *derived* from it — kept because
//! every `POST /v1/scores` feeds the API's rolling `score_drop` detector, so a degrading κ still
//! rides the existing alert channel with no parallel alerting built. See [`crate::calibration_post`]
//! for why that order matters. The runner additionally does an immediate per-cycle drift check
//! (below-bar / drop vs the previous run) so cron gets a non-zero exit on the very next bad run,
//! without waiting for the API window.
//!
//! Like `schedule`, it runs as a daemon (`--interval` loop) or a single cycle (`--once`, for cron).

use std::time::Duration;

use anyhow::{bail, Result};

use lighttrack_core::{Agreement, ModelPriceRow, Rubric};
use lighttrack_engine::{parse_judge_spec, EngineConfig};

use crate::calibrate::{judge_set, resolve_rubric};
use crate::calibration_post;
use crate::cli::Cli;
use crate::http::get;
use crate::util::now_ts;

/// Exit code the runner returns when a `--once` cycle ends untrusted, so an external scheduler / CI
/// step can fail on a judge that fell below the trust bar. `0` otherwise (incl. all daemon runs).
pub(crate) const UNTRUSTED_EXIT: i32 = 5;

/// Parameters for a watch run (kept in a struct to avoid a long argument list).
pub(crate) struct WatchParams<'a> {
    pub(crate) set: &'a crate::calibrate::CalibrationSet,
    pub(crate) rubric_text: Option<&'a str>,
    pub(crate) rubric_id: Option<&'a str>,
    pub(crate) project: Option<&'a str>,
    pub(crate) threshold: f64,
    pub(crate) kappa_bar: f64,
    pub(crate) drift_threshold: f64,
    pub(crate) samples: u32,
    pub(crate) interval: u64,
    pub(crate) once: bool,
    pub(crate) jobs: usize,
}

/// Trust level of one calibration cycle, relative to the trust bar and the previous run's κ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DriftLevel {
    /// κ ≥ bar and no significant drop vs the previous run.
    Ok,
    /// κ still ≥ bar, but it fell by more than the drift threshold vs the previous run.
    Drift,
    /// κ dropped below the trust bar — the judge is no longer trusted.
    Untrusted,
}

/// The drift verdict for a cycle: this run's κ against the bar and (when known) the previous run.
/// `level` is the headline classification; `trusted`/`drifted` are the two independent signals it is
/// derived from (kept so callers/tests can inspect them separately — untrusted dominates drift).
#[derive(Debug, Clone)]
pub(crate) struct DriftDecision {
    pub(crate) trusted: bool,
    pub(crate) prev_kappa: Option<f64>,
    /// `prev − current` when a previous run exists; `> 0` means agreement degraded.
    pub(crate) delta: Option<f64>,
    pub(crate) drifted: bool,
    pub(crate) level: DriftLevel,
}

/// Pure drift decision (no I/O, no LLM): compare this cycle's κ to the trust bar and, if a previous
/// run's κ is known, to that. A drop past `drift_threshold` warns even while still above the bar.
pub(crate) fn assess_drift(
    kappa: f64,
    prev_kappa: Option<f64>,
    kappa_bar: f64,
    drift_threshold: f64,
) -> DriftDecision {
    let trusted = kappa >= kappa_bar;
    let delta = prev_kappa.map(|p| p - kappa);
    let drifted = delta.map(|d| d > drift_threshold).unwrap_or(false);
    let level = if !trusted {
        DriftLevel::Untrusted
    } else if drifted {
        DriftLevel::Drift
    } else {
        DriftLevel::Ok
    };
    DriftDecision {
        trusted,
        prev_kappa,
        delta,
        drifted,
        level,
    }
}

/// The reserved rubric name that calibration history is persisted under, per judge model.
pub(crate) fn reserved_rubric(jp: &str, jm: &str) -> String {
    format!("lt:calibration:{jp}/{jm}")
}

/// Run the sentinel: daemon loop (`--interval`) or one cycle (`--once`). Returns the suggested
/// process exit code — [`UNTRUSTED_EXIT`] when a `--once` cycle ended untrusted, else `0`.
pub(crate) fn watch(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: &WatchParams,
) -> Result<i32> {
    let items = p.set.items.clone();
    if items.is_empty() {
        bail!("no calibration items in {}", p.set.source);
    }
    let rubric = resolve_rubric(cli, http, p.rubric_id)?;
    if rubric.is_none() && p.rubric_text.is_none() {
        bail!("provide --rubric \"<criteria>\" or --rubric-id <id>");
    }
    let (jp, jm) = parse_judge_spec(&engine.model);
    let reserved = reserved_rubric(&jp, &jm);
    let prices: Vec<ModelPriceRow> = get(cli, http, "/v1/prices").unwrap_or_default();

    println!(
        "calibrate --watch: {} item(s), judge={jp}/{jm}, rubric={reserved}, every {}s (once={}), \
         \u{3ba}-bar={:.2}, drift>{:.2}",
        items.len(),
        p.interval,
        p.once,
        p.kappa_bar,
        p.drift_threshold,
    );

    let mut last = DriftLevel::Ok;
    loop {
        match run_cycle(
            cli, http, engine, p, &items, &rubric, &jp, &jm, &reserved, &prices,
        ) {
            Ok(level) => last = level,
            // A failed cycle (API briefly down, transient judge error) must not kill the daemon.
            Err(e) => eprintln!("calibrate cycle error (continuing): {e}"),
        }
        if p.once {
            break;
        }
        std::thread::sleep(Duration::from_secs(p.interval));
    }
    Ok(if p.once && last == DriftLevel::Untrusted {
        UNTRUSTED_EXIT
    } else {
        0
    })
}

/// One sentinel cycle: read the previous κ from scores history, re-judge the golden set, persist the
/// new agreement (which feeds the API's `score_drop` alerting), and report the drift verdict.
#[allow(clippy::too_many_arguments)]
fn run_cycle(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    p: &WatchParams,
    items: &[lighttrack_core::CalibrationItem],
    rubric: &Option<Rubric>,
    jp: &str,
    jm: &str,
    reserved: &str,
    prices: &[ModelPriceRow],
) -> Result<DriftLevel> {
    // Keyed on the (rubric, judge) pair rather than on a reserved rubric NAME in a 500-row scan —
    // which returned "no baseline" the moment a busy project pushed the last calibration off that
    // page, quietly disabling drift detection exactly where it was needed most.
    let prev =
        calibration_post::previous_kappa(cli, http, p.project, p.rubric_id, &judge_id(jp, jm));
    let c = judge_set(
        engine,
        jp,
        jm,
        rubric,
        p.rubric_text,
        items,
        p.threshold,
        p.kappa_bar,
        p.samples,
        p.jobs,
        prices,
        false,
    );
    let decision = assess_drift(
        c.agreement.cohen_kappa,
        prev,
        p.kappa_bar,
        p.drift_threshold,
    );
    calibration_post::persist(
        cli,
        http,
        &calibration_post::Measured {
            project: p.project,
            rubric_id: p.rubric_id,
            judge: &judge_id(jp, jm),
            dataset_id: p.set.dataset_id.as_deref(),
            cost: c.cost,
        },
        &c.agreement,
        reserved,
    )?;
    report(&decision, &c.agreement, c.cost, c.skipped);
    Ok(decision.level)
}

/// The canonical `[provider/]model` string a calibration record is keyed by.
fn judge_id(jp: &str, jm: &str) -> String {
    format!("{jp}/{jm}")
}

/// Print the compact per-cycle line and any drift warning. Warnings go to **stderr** so a daemon's
/// stdout stays a clean κ time-series while cron/log scrapers can key on the warning stream.
fn report(d: &DriftDecision, a: &Agreement, cost: f64, skipped: u32) {
    let delta = match d.delta {
        Some(dl) => format!("  \u{394}\u{3ba}={:+.3}", -dl),
        None => String::new(),
    };
    println!(
        "  [{}] \u{3ba}={:.3} (n={})  MAE={:.3}  bias={:+.3}  cost=${cost:.5}{delta}",
        now_ts(),
        a.cohen_kappa,
        a.n,
        a.mae,
        a.bias,
    );
    match d.level {
        DriftLevel::Ok => {
            debug_assert!(d.trusted && !d.drifted);
            println!(
                "  verdict: OK (\u{3ba} {:.3} >= bar {:.2})",
                a.cohen_kappa, a.kappa_bar
            )
        }
        DriftLevel::Drift => {
            eprintln!(
            "  WARN drift: \u{3ba} fell {:.3} vs previous run ({:.3} -> {:.3}), still >= bar {:.2}",
            d.delta.unwrap_or(0.0), d.prev_kappa.unwrap_or(0.0), a.cohen_kappa, a.kappa_bar,
        )
        }
        DriftLevel::Untrusted => eprintln!(
            "  ALERT untrusted: \u{3ba} {:.3} < bar {:.2} — judge no longer trusted{} \
             (persisted; degradation feeds the API score_drop alert)",
            a.cohen_kappa,
            a.kappa_bar,
            if d.drifted {
                " and dropped sharply vs the previous run"
            } else {
                ""
            },
        ),
    }
    if skipped > 0 {
        println!("  note: {skipped} item(s) skipped (unparseable judge output).");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_when_above_bar_and_stable() {
        // No previous run, κ clears the bar → OK, no drift.
        let d = assess_drift(0.82, None, 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Ok);
        assert!(d.trusted && !d.drifted);
        assert!(d.delta.is_none());

        // Above bar and improved vs previous → still OK (a rise is never a drift).
        let d = assess_drift(0.9, Some(0.7), 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Ok);
        assert!((d.delta.unwrap() - (0.7 - 0.9)).abs() < 1e-9); // negative delta = improvement
        assert!(!d.drifted);
    }

    #[test]
    fn drift_when_big_drop_but_still_trusted() {
        // 0.90 → 0.70: drop 0.20 > 0.15 threshold, but 0.70 ≥ 0.6 bar → Drift (warn, still trusted).
        let d = assess_drift(0.70, Some(0.90), 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Drift);
        assert!(d.trusted && d.drifted);
        assert!((d.delta.unwrap() - 0.20).abs() < 1e-9);
    }

    #[test]
    fn small_drop_above_bar_is_ok() {
        // 0.80 → 0.72: drop 0.08 ≤ 0.15 threshold → no drift, OK.
        let d = assess_drift(0.72, Some(0.80), 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Ok);
        assert!(!d.drifted);
    }

    #[test]
    fn untrusted_dominates_even_on_small_drop() {
        // κ below the bar is Untrusted regardless of how small the step down was.
        let d = assess_drift(0.55, Some(0.58), 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Untrusted);
        assert!(!d.trusted);
        // Untrusted takes precedence over the Drift classification even on a large drop.
        let d = assess_drift(0.40, Some(0.90), 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Untrusted);
        assert!(d.drifted); // the drop is still recorded…
        assert!(!d.trusted); // …but "untrusted" is the headline.
    }

    #[test]
    fn untrusted_on_first_run_below_bar() {
        let d = assess_drift(0.3, None, 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Untrusted);
        assert!(d.delta.is_none() && !d.drifted);
    }

    #[test]
    fn exactly_at_bar_is_trusted() {
        // κ == bar is trusted (inclusive), matching Agreement::trusted (κ ≥ kappa_bar).
        let d = assess_drift(0.6, None, 0.6, 0.15);
        assert_eq!(d.level, DriftLevel::Ok);
        assert!(d.trusted);
    }

    #[test]
    fn drift_boundary_is_strictly_greater_than_threshold() {
        // A drop exactly equal to the threshold does NOT drift (strictly-greater rule). Values chosen
        // as exact binary fractions so 0.75 − 0.5 == 0.25 holds without float slop.
        let d = assess_drift(0.5, Some(0.75), 0.4, 0.25);
        assert!((d.delta.unwrap() - 0.25).abs() < 1e-12);
        assert_eq!(d.level, DriftLevel::Ok);
        assert!(!d.drifted);
        // Just past the threshold does drift.
        let d = assess_drift(0.5, Some(0.75), 0.4, 0.2);
        assert_eq!(d.level, DriftLevel::Drift);
    }

    #[test]
    fn reserved_rubric_name_is_stable() {
        assert_eq!(
            reserved_rubric("anthropic", "haiku"),
            "lt:calibration:anthropic/haiku"
        );
        assert_eq!(
            reserved_rubric("openai", "gpt-4o"),
            "lt:calibration:openai/gpt-4o"
        );
    }
}
