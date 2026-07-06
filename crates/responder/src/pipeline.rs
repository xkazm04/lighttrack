//! The end-to-end reaction for one error-spike: route → classify → enrich → investigate → report.
//! Runs on a detached task (spawned by the webhook handler), so every step just logs and returns.

use std::sync::Arc;

use crate::classify::{classify, Class};
use crate::config::Config;
use crate::webhook::Spike;
use crate::{enrich, investigate, report};

pub(crate) async fn handle_spike(cfg: Arc<Config>, spike: Spike) {
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
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap_or_default();
    let context =
        enrich::recent_failures(&client, &cfg.lighttrack_url, &project, cfg.defaults.enrich_limit)
            .await;

    let diag = investigate::investigate(&cfg, entry, &spike, &context).await;

    match report::write_report(&cfg.report_dir, &spike, &diag) {
        Ok(path) => println!(
            "[responder] '{project}': diagnosis {} -> {}",
            if diag.ok { "written" } else { "FAILED, see" },
            path.display()
        ),
        Err(e) => eprintln!("[responder] '{project}': could not write report: {e}"),
    }
}
