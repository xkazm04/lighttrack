//! The end-to-end reaction for one trigger. Error spikes: route → classify → enrich → investigate →
//! report, plus an optional gated auto-fix (ACT) for opt-in projects. Quality regressions: enrich →
//! investigate → report, diagnosis-only (fixing a quality drop is a human judgment call, not an
//! auto-edit). Runs on a detached task spawned by the webhook handler, so every step just logs.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::breaker::Breaker;
use crate::classify::{classify, Class};
use crate::config::{Config, ProjectEntry};
use crate::webhook::{Drop, Spike, Trigger};
use crate::{act, email, enrich, investigate, report};

pub(crate) async fn handle_trigger(cfg: Arc<Config>, breaker: Arc<Breaker>, trigger: Trigger) {
    let project = trigger.project_id().to_string();
    let Some(entry) = cfg.projects.get(&project) else {
        eprintln!("[responder] no repo mapped for project '{project}' — skipping");
        return;
    };
    match &trigger {
        Trigger::Error(spike) => run_error(&cfg, &breaker, entry, spike).await,
        Trigger::Quality(drop) => run_quality(&cfg, entry, drop).await,
    }
}

async fn run_error(cfg: &Config, breaker: &Breaker, entry: &ProjectEntry, spike: &Spike) {
    let project = &spike.project_id;
    match classify(spike.status.as_deref(), spike.error.as_deref()) {
        Class::Transient => {
            println!(
                "[responder] '{project}': transient/provider error — no code investigation (error: {})",
                spike.error.as_deref().unwrap_or("")
            );
            return;
        }
        Class::Code => {}
    }

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
    deliver(cfg, &ts, project, "error", &detail, &diag, act_outcome.as_ref()).await;
}

async fn run_quality(cfg: &Config, entry: &ProjectEntry, drop: &Drop) {
    let project = &drop.project_id;
    let rubric = drop.rubric.as_deref().unwrap_or("?");
    println!(
        "[responder] '{project}': quality regression on rubric '{rubric}' — investigating in {}",
        entry.repo
    );
    let context =
        enrich::recent_scores(&http_client(), &cfg.lighttrack_url, project, drop.rubric.as_deref(), 30)
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
    deliver(cfg, &ts, project, "quality regression", &detail, &diag, None).await;
}

/// Render the report once, persist it, and (if email is configured) send the same body.
async fn deliver(
    cfg: &Config,
    ts: &str,
    project: &str,
    kind: &str,
    detail: &str,
    diag: &crate::claude::ClaudeRun,
    act_outcome: Option<&act::ActOutcome>,
) {
    let md = report::render(project, ts, kind, detail, diag, act_outcome);
    match report::write(&cfg.report_dir, project, ts, &md) {
        Ok(path) => println!("[responder] '{project}': report -> {}", path.display()),
        Err(e) => eprintln!("[responder] '{project}': could not write report: {e}"),
    }
    if let Some(cfg_email) = &cfg.email {
        let subject = format!("LightTrack diagnosis: {project} ({kind})");
        email::send(cfg_email, &subject, &md).await;
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
