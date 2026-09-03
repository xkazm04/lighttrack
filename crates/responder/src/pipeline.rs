//! The end-to-end reaction for one trigger. Error spikes: route → classify → enrich → investigate →
//! report, plus an optional gated auto-fix (ACT) for opt-in projects. Quality regressions: enrich →
//! investigate → report, diagnosis-only (fixing a quality drop is a human judgment call, not an
//! auto-edit). Runs on a detached task spawned by the webhook handler, so every step just logs.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::breaker::Breaker;
use crate::classify::{decide, Class, Source};
use crate::config::{Config, ProjectEntry};
use crate::webhook::{Drop, Spike, Trigger};
use crate::{act, email, enrich, investigate, ledger, report};

pub(crate) async fn handle_trigger(
    cfg: Arc<Config>,
    breaker: Arc<Breaker>,
    trigger: Trigger,
    alert_id: Option<String>,
) {
    let project = trigger.project_id().to_string();
    let Some(entry) = cfg.projects.get(&project) else {
        eprintln!("[responder] no repo mapped for project '{project}' — skipping");
        return;
    };
    match &trigger {
        Trigger::Error(spike) => run_error(&cfg, &breaker, entry, spike, alert_id).await,
        Trigger::Quality(drop) => run_quality(&cfg, &breaker, entry, drop, alert_id).await,
    }
}

/// Admission control for the (billable) INVESTIGATE stage: dedup + cooldown + hourly cap + a global
/// concurrency permit. Returns the RAII guard to hold for the run, or `None` if the run was shed
/// (already logged). The read stage runs first and always, so this — not the ACT breaker — is what
/// actually bounds a flapping project's spend.
async fn admit(
    breaker: &Breaker,
    cfg: &Config,
    project: &str,
) -> Option<crate::breaker::InvestigationGuard> {
    let cooldown = Duration::from_secs(cfg.defaults.investigate_cooldown_secs);

    // The durable half, consulted first. The in-process counters below are empty after a restart,
    // so a still-firing spike used to buy a fresh paid run the moment the responder came back up.
    // The ledger remembers: an alert this responder investigated carries a resolution. Best-effort
    // in the honest direction — an unreachable ledger only ever falls back to the memory below.
    if let Some(a) = ledger::admission(
        &cfg.lighttrack_url,
        cfg.api_key.as_deref(),
        project,
        cooldown,
    )
    .await
    {
        if a.project_recent {
            println!(
                "[responder] '{project}': investigation skipped (the alert ledger shows one \
                 already resolved within the cooldown, {}s)",
                cooldown.as_secs()
            );
            return None;
        }
        if a.hour_count >= cfg.defaults.max_investigations_per_hour {
            println!(
                "[responder] '{project}': investigation skipped (the alert ledger shows {} \
                 resolved in the last hour, cap {}/h)",
                a.hour_count, cfg.defaults.max_investigations_per_hour
            );
            return None;
        }
    }

    match breaker.try_admit_investigation(
        project,
        cooldown,
        cfg.defaults.max_investigations_per_hour,
    ) {
        Ok(guard) => Some(guard),
        Err(reason) => {
            println!("[responder] '{project}': investigation skipped ({reason})");
            None
        }
    }
}

async fn run_error(
    cfg: &Config,
    breaker: &Breaker,
    entry: &ProjectEntry,
    spike: &Spike,
    alert_id: Option<String>,
) {
    let project = &spike.project_id;
    // Read the class the PRODUCER minted first; fall back to reading the message only when it said
    // nothing. Which branch ran is counted, because "how often are we still guessing" is the number
    // that says whether carrying the class was worth doing.
    let (class, source) = decide(
        spike.failure_class.as_deref(),
        spike.status.as_deref(),
        spike.error.as_deref(),
    );
    CLASSIFIED.record(class, source);
    match class {
        Class::Transient => {
            println!(
                "[responder] '{project}': transient/provider error — no code investigation                  (class={}, error: {})",
                label(source),
                spike.error.as_deref().unwrap_or("")
            );
            return;
        }
        Class::Code => {}
    }
    if source == Source::Fallback {
        // Not a failure — it is the honest handling for a producer that said nothing — but it is a
        // paid decision made by reading prose, so it is visible rather than silent.
        println!(
            "[responder] '{project}': no carried failure class — spending an investigation on a              verdict read from the message"
        );
    }

    // Gate the paid investigation after classification, so transient errors (which never spawn)
    // don't consume a permit or the per-project cooldown. Held across investigate + act + deliver.
    let Some(_guard) = admit(breaker, cfg, project).await else {
        return;
    };

    println!(
        "[responder] '{project}': error — investigating in {} (branch={}, model={}, mode={})",
        entry.repo,
        entry.branch.as_deref().unwrap_or("-"),
        cfg.defaults.model,
        cfg.defaults.permission_mode
    );
    let context = enrich::recent_failures(
        &http_client(),
        &cfg.lighttrack_url,
        project,
        cfg.defaults.enrich_limit,
        cfg.api_key.as_deref(),
    )
    .await;
    let prompt = investigate::error_prompt(entry, spike, &context);
    let diag = investigate::investigate(cfg, entry, &prompt).await;
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let act_outcome = if entry.auto_fix && diag.ok {
        println!("[responder] '{project}': diagnosis ok — attempting gated auto-fix");
        let outcome = act::run_act(cfg, breaker, entry, spike, &diag.text, &ts).await;
        log_act(project, &outcome);
        Some(outcome)
    } else {
        if entry.auto_fix {
            println!("[responder] '{project}': diagnosis failed — skipping auto-fix");
        }
        None
    };

    let detail = format!(
        "error x{} (status {}): {}",
        spike.count.unwrap_or(0),
        spike.status.as_deref().unwrap_or("error"),
        spike.error.as_deref().unwrap_or("(no message)")
    );
    deliver(
        cfg,
        &ts,
        project,
        "error",
        &detail,
        &diag,
        act_outcome.as_ref(),
        alert_id.as_deref(),
    )
    .await;
}

async fn run_quality(
    cfg: &Config,
    breaker: &Breaker,
    entry: &ProjectEntry,
    drop: &Drop,
    alert_id: Option<String>,
) {
    let project = &drop.project_id;
    let rubric = drop.rubric.as_deref().unwrap_or("?");

    // Same admission gate as the error path — a quality-drop investigation is an equally billable run.
    let Some(_guard) = admit(breaker, cfg, project).await else {
        return;
    };

    println!(
        "[responder] '{project}': quality regression on rubric '{rubric}' — investigating in {}",
        entry.repo
    );
    let context = enrich::recent_scores(
        &http_client(),
        &cfg.lighttrack_url,
        project,
        drop.rubric.as_deref(),
        30,
        cfg.api_key.as_deref(),
    )
    .await;
    let prompt = investigate::quality_prompt(entry, drop, &context);
    let diag = investigate::investigate(cfg, entry, &prompt).await;
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let detail = format!(
        "rubric '{rubric}' down {:.0}% — recent mean {:.2} vs baseline {:.2}",
        drop.drop_pct.unwrap_or(0.0),
        drop.recent_avg.unwrap_or(0.0),
        drop.baseline_avg.unwrap_or(0.0),
    );
    // Diagnosis-only: no ACT for quality regressions.
    deliver(
        cfg,
        &ts,
        project,
        "quality regression",
        &detail,
        &diag,
        None,
        alert_id.as_deref(),
    )
    .await;
}

/// Render the report once, persist it, post it back as the alert's resolution, and (if email is
/// configured) send the same body.
#[allow(clippy::too_many_arguments)]
async fn deliver(
    cfg: &Config,
    ts: &str,
    project: &str,
    kind: &str,
    detail: &str,
    diag: &crate::invoke::ClaudeRun,
    act_outcome: Option<&act::ActOutcome>,
    alert_id: Option<&str>,
) {
    let md = report::render(project, ts, kind, detail, diag, act_outcome);
    let report_path = match report::write(&cfg.report_dir, project, ts, &md) {
        Ok(path) => {
            println!("[responder] '{project}': report -> {}", path.display());
            Some(path.display().to_string())
        }
        Err(e) => {
            eprintln!("[responder] '{project}': could not write report: {e}");
            None
        }
    };
    // Close the loop. Until this existed, the diagnosis lived only on this machine's disk: the
    // alert that caused a paid investigation carried no trace that anyone — human or model — had
    // ever looked at it.
    if let Some(id) = alert_id {
        ledger::post_resolution(
            &cfg.lighttrack_url,
            cfg.api_key.as_deref(),
            id,
            report_path.as_deref(),
            diag.cost_usd,
            diag.ok,
            act_outcome.map(act_summary).as_deref(),
        )
        .await;
    }
    if let Some(cfg_email) = &cfg.email {
        let subject = format!("LightTrack diagnosis: {project} ({kind})");
        // HTML for rendering, Markdown as the text fallback.
        let html = report::render_html(project, ts, kind, detail, diag, act_outcome);
        email::send(cfg_email, &subject, &html, &md).await;
        println!("[responder] '{project}': diagnosis emailed");
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
}

fn log_act(project: &str, o: &act::ActOutcome) {
    if let Some(reason) = &o.skipped_reason {
        println!("[responder] '{project}': auto-fix skipped ({reason})");
    } else if o.applied {
        let tests = match o.tests {
            Some(true) => "tests passed",
            Some(false) => "tests FAILED",
            None => "no test run",
        };
        println!(
            "[responder] '{project}': auto-fix applied on {} — {tests}",
            o.branch.as_deref().unwrap_or("-")
        );
    } else {
        println!("[responder] '{project}': auto-fix made no changes (no confident fix)");
    }
}

/// How each verdict was reached, over the life of the process.
///
/// Before this, `pipeline.rs`'s transient branch only printed, so nothing anywhere recorded how
/// often the classifier was right, wrong, or guessing — the measurable the direction asks for did
/// not exist to be read. Two numbers matter and both are here: investigations SKIPPED as transient
/// (money not spent), and investigations SPENT on a verdict that came from reading a message
/// (`code_fallback` — the population every false `Class::Code` is drawn from, each one a real
/// Claude Code run against a codebase with no bug in it).
#[derive(Default)]
pub(crate) struct Classified {
    pub transient_carried: AtomicU64,
    pub transient_fallback: AtomicU64,
    pub code_carried: AtomicU64,
    pub code_fallback: AtomicU64,
}

impl Classified {
    pub(crate) fn record(&self, class: Class, source: Source) {
        let counter = match (class, source) {
            (Class::Transient, Source::Carried) => &self.transient_carried,
            (Class::Transient, Source::Fallback) => &self.transient_fallback,
            (Class::Code, Source::Carried) => &self.code_carried,
            (Class::Code, Source::Fallback) => &self.code_fallback,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn get(&self, class: Class, source: Source) -> u64 {
        let counter = match (class, source) {
            (Class::Transient, Source::Carried) => &self.transient_carried,
            (Class::Transient, Source::Fallback) => &self.transient_fallback,
            (Class::Code, Source::Carried) => &self.code_carried,
            (Class::Code, Source::Fallback) => &self.code_fallback,
        };
        counter.load(Ordering::Relaxed)
    }

    /// Decisions still being made by reading prose, as a fraction of all of them. The number this
    /// direction moves; 1.0 is where it started, because there was no other path.
    pub(crate) fn fallback_rate(&self) -> f64 {
        let fallback =
            self.get(Class::Transient, Source::Fallback) + self.get(Class::Code, Source::Fallback);
        let total = fallback
            + self.get(Class::Transient, Source::Carried)
            + self.get(Class::Code, Source::Carried);
        if total == 0 {
            0.0
        } else {
            fallback as f64 / total as f64
        }
    }
}

pub(crate) static CLASSIFIED: Classified = Classified {
    transient_carried: AtomicU64::new(0),
    transient_fallback: AtomicU64::new(0),
    code_carried: AtomicU64::new(0),
    code_fallback: AtomicU64::new(0),
};

fn label(source: Source) -> &'static str {
    match source {
        Source::Carried => "carried from the producer",
        Source::Fallback => "read from the message",
    }
}

/// One line describing what the auto-fix stage did, for the alert's resolution.
fn act_summary(o: &act::ActOutcome) -> String {
    if let Some(reason) = &o.skipped_reason {
        return format!("skipped: {reason}");
    }
    if !o.applied {
        return "no confident fix".to_string();
    }
    let tests = match o.tests {
        Some(true) => "tests passed",
        Some(false) => "tests FAILED",
        None => "no test run",
    };
    format!(
        "applied on {} — {tests}",
        o.branch.as_deref().unwrap_or("-")
    )
}
