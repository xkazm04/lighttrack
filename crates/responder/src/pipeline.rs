//! The end-to-end reaction for one error-spike: route → classify → enrich → investigate → report,
//! plus an optional gated auto-fix (ACT) for opt-in projects. Runs on a detached task (spawned by the
//! webhook handler), so every step just logs and returns.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;

use crate::breaker::Breaker;
use crate::classify::{classify, Class};
use crate::config::Config;
use crate::webhook::Spike;
use crate::{act, enrich, investigate, report};

pub(crate) async fn handle_spike(cfg: Arc<Config>, breaker: Arc<Breaker>, spike: Spike) {
    let project = spike.project_id.clone();

    let Some(entry) = cfg.projects.get(&project) else {
        eprintln!("[responder] no repo mapped for project '{project}' — skipping");
        return;
    };

    match classify(spike.status.as_deref(), spike.error.as_deref()) {
        Class::Transient => {
            println!(
                "[responder] '{project}': transient/provider error — no code investigation \
                 (error: {})",
                spike.error.as_deref().unwrap_or("")
            );
            return;
        }
        Class::Code => {}
    }

    println!(
        "[responder] '{project}': investigating in {} (branch={}, model={}, mode={})",
        entry.repo,
        entry.branch.as_deref().unwrap_or("-"),
        cfg.defaults.model,
        cfg.defaults.permission_mode
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let context =
        enrich::recent_failures(&client, &cfg.lighttrack_url, &project, cfg.defaults.enrich_limit)
            .await;

    let diag = investigate::investigate(&cfg, entry, &spike, &context).await;
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();

    let act_outcome = if entry.auto_fix && diag.ok {
        println!("[responder] '{project}': diagnosis ok — attempting gated auto-fix");
        let outcome = act::run_act(&cfg, &breaker, entry, &spike, &diag.text, &ts).await;
        log_act(&project, &outcome);
        Some(outcome)
    } else {
        if entry.auto_fix {
            println!("[responder] '{project}': diagnosis failed — skipping auto-fix");
        }
        None
    };

    match report::write_report(&cfg.report_dir, &ts, &spike, &diag, act_outcome.as_ref()) {
        Ok(path) => println!("[responder] '{project}': report -> {}", path.display()),
        Err(e) => eprintln!("[responder] '{project}': could not write report: {e}"),
    }
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
