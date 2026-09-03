//! The acting half of the served-version quality loop: compare each prompt's canary label against
//! its production label on a timer, alert when it is measurably worse, and — only when asked —
//! move the label back.
//!
//! **Where it runs.** In the API process, on `forecast_sweep`'s shape and for its reasons: the
//! `Alerter` and its cooldown map, the alert channel config and the `Store` handle all live in
//! `AppState`, and a Cloud Run deployment ships the API alone. A sweep hosted in the optional
//! runner would silently not fire in exactly the deployment that most needs it.
//!
//! **Off by default.** `LIGHTTRACK_PROMPT_CANARY_SWEEP_SECS` unset (or `0`) = no sweep. A canary
//! that can move what production serves is not something a deployment acquires by upgrading, and
//! even with the sweep on, `auto_revert` is a second, per-prompt opt-in.
//!
//! **What counts as a regression.** Two conditions, both required, and both taken from the
//! benchmark verdict's semantics rather than invented here:
//!
//! 1. **Evidence** — at least `min_n` verdicts on *each* side. A version judged three times has
//!    said nothing, and acting on it would make the canary a random-number generator.
//! 2. **Separation** — the canary's whole ~95% interval sits below production's, *and* the relative
//!    drop exceeds `max_drop`. The CI test is what keeps a noisy sample from tripping a rollback;
//!    the `max_drop` band is what keeps a statistically-real-but-trivial 0.5% dip from doing it.
//!
//! The comparison is **paired on the rubric** where the prompt's benchmark names one: both sides are
//! then judged against the same criteria on the same scale, which is the only reading under which
//! "worse" means anything.
//!
//! **It cannot touch the ingest hot path**: detached task, every store read through `spawn_db`,
//! yields between projects, and a failure on one prompt is logged and skipped.

use std::time::Duration;

use chrono::Utc;

use lighttrack_core::{CanaryPolicy, Dimension, Prompt, REASON_CANARY_REGRESSED};
use lighttrack_store::ScoreSummaryRow;

use crate::alerts_canary::CanaryRegression;
use crate::error::ApiError;
use crate::state::{spawn_db, AppState};
use lighttrack_store::Scope as TenantScope;

/// Cadence in seconds; unset or `0` disables the sweep entirely.
const ENV_SECS: &str = "LIGHTTRACK_PROMPT_CANARY_SWEEP_SECS";

/// Floor on the interval. The comparison reads a multi-hour window and the alert cooldown (default
/// 1h) would suppress the output anyway, so sweeping faster than once a minute only burns reads.
const MIN_INTERVAL_SECS: u64 = 60;

#[derive(Clone, Copy)]
pub(crate) struct SweepConfig {
    pub(crate) interval: Duration,
}

impl SweepConfig {
    /// `None` when the sweep is off (the default).
    pub(crate) fn from_env() -> Option<Self> {
        let secs: u64 = std::env::var(ENV_SECS).ok()?.trim().parse().ok()?;
        if secs == 0 {
            return None;
        }
        Some(SweepConfig {
            interval: Duration::from_secs(secs.max(MIN_INTERVAL_SECS)),
        })
    }
}

/// One line for the startup banner, so an operator can see whether anything is watching.
pub(crate) fn describe(cfg: Option<SweepConfig>) -> String {
    match cfg {
        None => format!("off (set {ENV_SECS})"),
        Some(c) => format!(
            "every {}s (auto-revert is per-prompt)",
            c.interval.as_secs()
        ),
    }
}

/// Start the sweep loop as a detached task. No-op when the sweep is disabled.
pub(crate) fn spawn(st: AppState, cfg: Option<SweepConfig>) {
    let Some(cfg) = cfg else { return };
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(cfg.interval);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick fires immediately; spend it, so a restart loop cannot re-alert on every
        // boot and startup stays quiet.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let n = sweep_once(&st).await;
            if n > 0 {
                tracing::info!(
                    raised = n,
                    "prompt canary sweep raised regressions (cooldown decides delivery)"
                );
            }
        }
    });
}

/// One pass over every enabled project's prompts. Returns how many regressions were **found** — not
/// how many were delivered; the in-process cooldown and the store's admission gate decide that,
/// which is why a sustained regression logs a count every sweep while an operator hears about it
/// once. Never panics and never propagates — a broken prompt must not stop the others, or the loop.
pub(crate) async fn sweep_once(st: &AppState) -> usize {
    let store = st.store.clone();
    let projects = match spawn_db(move || store.list_projects()).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "prompt canary sweep could not list projects");
            return 0;
        }
    };
    let mut raised = 0;
    // A backend that serves neither the registry nor the quality join cannot canary at all — a
    // property of the DEPLOYMENT, not of a project. One line per sweep, not one per project per
    // tick, which is the shape of log noise that trains an operator to stop reading.
    let mut unsupported_warned = false;
    for p in projects.iter().filter(|p| p.enabled) {
        match project_pass(st, &p.id).await {
            Ok(found) => {
                raised += found.len();
                st.alerts.notify_prompt_canary(&found);
            }
            Err(e) if e.is_unsupported() => {
                if !unsupported_warned {
                    unsupported_warned = true;
                    tracing::warn!(
                        error = %e,
                        "prompt canary sweep is skipping every project: this backend does not serve \
                         the prompt registry or the per-version quality join, so a promoted version \
                         cannot be measured here (see docs/PARITY.md)"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(project_id = %p.id, error = %e, "prompt canary sweep failed for a project")
            }
        }
        tokio::task::yield_now().await;
    }
    raised
}

/// One project: every prompt carrying a canary policy, compared and (when asked) reverted.
pub(crate) async fn project_pass(
    st: &AppState,
    project: &str,
) -> Result<Vec<CanaryRegression>, ApiError> {
    let store = st.store.clone();
    let pid = project.to_string();
    let prompts = spawn_db(move || store.list_prompts(&pid)).await?;
    let mut out = Vec::new();
    for prompt in prompts.into_iter().filter(|p| p.canary.is_some()) {
        match one_prompt(st, project, prompt).await {
            Ok(Some(r)) => out.push(r),
            Ok(None) => {}
            // A single prompt's failure is logged, never propagated: the next prompt's canary is
            // still worth checking, and an unsupported backend has already been reported once.
            Err(e) if e.is_unsupported() => return Err(e),
            Err(e) => {
                tracing::warn!(project_id = %project, error = %e, "a prompt's canary check failed")
            }
        }
    }
    Ok(out)
}

/// Compare one prompt's canary against its production label, acting when the policy says to.
async fn one_prompt(
    st: &AppState,
    project: &str,
    prompt: Prompt,
) -> Result<Option<CanaryRegression>, ApiError> {
    let Some(policy) = prompt.canary.clone() else {
        return Ok(None);
    };
    // A policy that cannot decide anything is a configuration error, not a quiet no-op: say so once
    // per sweep rather than looking like a canary that never finds anything.
    if let Some(why) = policy.invalid() {
        tracing::warn!(project_id = %project, prompt = %prompt.name, reason = %why,
            "skipping an unusable canary policy");
        return Ok(None);
    }
    let (Some(&canary_v), Some(&prod_v)) = (
        prompt.labels.get(&policy.label),
        prompt.labels.get(&policy.production_label),
    ) else {
        // Nothing is being canaried, or there is no production version to compare against. Not a
        // problem — it is the resting state of a prompt whose canary has been reverted.
        return Ok(None);
    };
    if canary_v == prod_v {
        return Ok(None);
    }

    // Pair on the rubric the prompt's benchmark judges against, where there is one: two versions
    // scored against different criteria are not comparable, and the mean of a mixture is a number
    // with no meaning that would still trip a threshold.
    let rubric_id = benchmark_rubric(st, &prompt).await;
    let since = Utc::now() - chrono::Duration::seconds(policy.window_secs as i64);
    let store = st.store.clone();
    let pid = project.to_string();
    let rubric = rubric_id.clone();
    let rows = spawn_db(move || {
        store.score_summary_by_dimension(
            TenantScope::Project(&pid),
            Dimension::Prompt,
            since,
            None,
            rubric.as_deref(),
        )
    })
    .await?;

    let canary_tag = CanaryPolicy::tag(&prompt.name, canary_v);
    let prod_tag = CanaryPolicy::tag(&prompt.name, prod_v);
    let (Some(canary), Some(production)) = (find(&rows, &canary_tag), find(&rows, &prod_tag))
    else {
        return Ok(None);
    };
    let Some(drop) = regression(&policy, canary, production) else {
        return Ok(None);
    };

    // Act before alerting, so the alert can state what was already done rather than what is about
    // to be attempted — an alert that promises a revert which then fails is worse than no alert.
    let reverted_to = match policy.auto_revert {
        true => revert(st, prompt.clone(), &policy).await,
        false => None,
    };
    Ok(Some(CanaryRegression {
        project_id: project.to_string(),
        prompt: prompt.name,
        canary_label: policy.label,
        production_label: policy.production_label,
        canary_version: canary_v,
        production_version: prod_v,
        canary_mean: canary.mean,
        production_mean: production.mean,
        canary_ci95_high: canary.ci95_high,
        production_ci95_low: production.ci95_low,
        canary_n: canary.n,
        production_n: production.n,
        drop,
        max_drop: policy.max_drop,
        reverted_to,
    }))
}

fn find<'a>(rows: &'a [ScoreSummaryRow], tag: &str) -> Option<&'a ScoreSummaryRow> {
    rows.iter().find(|r| r.key.as_deref() == Some(tag))
}

/// The relative drop when this comparison is a regression, or `None`.
///
/// Pure, and the whole gate: evidence on both sides, non-overlapping intervals (the canary's upper
/// bound below production's lower bound), and a drop past the policy's band.
pub(crate) fn regression(
    policy: &CanaryPolicy,
    canary: &ScoreSummaryRow,
    production: &ScoreSummaryRow,
) -> Option<f64> {
    let min_n = policy.min_n as u64;
    if canary.n < min_n || production.n < min_n {
        return None;
    }
    // A production mean of zero has no relative band to measure against — dividing by it would make
    // every canary infinitely worse, which is exactly the false rollback this gate exists to avoid.
    if production.mean <= 0.0 {
        return None;
    }
    if canary.ci95_high >= production.ci95_low {
        return None;
    }
    let drop = (production.mean - canary.mean) / production.mean;
    (drop > policy.max_drop).then_some(drop)
}

/// Move the canary label back to the version it replaced. `None` when the ledger names no
/// predecessor — the revert then does nothing rather than guessing at a version, and the alert says
/// so.
async fn revert(st: &AppState, mut prompt: Prompt, policy: &CanaryPolicy) -> Option<u32> {
    let previous = prompt.previous_version(&policy.label)?;
    prompt.set_label(&policy.label, previous, REASON_CANARY_REGRESSED);
    prompt.updated_at = Utc::now();
    let store = st.store.clone();
    let p2 = prompt.clone();
    match spawn_db(move || store.update_prompt(&p2)).await {
        Ok(()) => Some(previous),
        Err(e) => {
            tracing::warn!(prompt = %prompt.name, error = %e, "canary auto-revert could not be written");
            None
        }
    }
}

/// The rubric the prompt's linked benchmark judges against, when it has one. Best-effort: a
/// benchmark that cannot be read leaves the comparison unpaired rather than failing the sweep.
async fn benchmark_rubric(st: &AppState, prompt: &Prompt) -> Option<String> {
    let bid = prompt.benchmark_id.clone()?;
    let store = st.store.clone();
    let owner = prompt.project_id.clone();
    spawn_db(move || store.get_benchmark(TenantScope::Project(&owner), &bid))
        .await
        .ok()
        .flatten()
        .and_then(|b| b.rubric_id)
}

#[cfg(test)]
pub(crate) mod tests;
