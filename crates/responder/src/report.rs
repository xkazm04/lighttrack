//! Persist a diagnosis as a Markdown file under the report directory.

use std::path::{Path, PathBuf};

use chrono::Utc;

use crate::investigate::Diagnosis;
use crate::webhook::Spike;

pub(crate) fn write_report(
    dir: &str,
    spike: &Spike,
    diag: &Diagnosis,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let ts = Utc::now().format("%Y%m%dT%H%M%SZ").to_string();
    let safe: String = spike
        .project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = Path::new(dir).join(format!("{safe}-{ts}.md"));

    let cost = diag.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".to_string());
    let body = format!(
        "# Diagnosis — {project}\n\n\
         - when: {ts}\n\
         - model: {model}\n\
         - cost: {cost}\n\
         - status: {ok}\n\n\
         ## Triggering error\n\n```\n{error}\n```\n\n\
         ## Investigation\n\n{text}\n",
        project = spike.project_id,
        model = diag.model,
        ok = if diag.ok { "ok" } else { "FAILED" },
        error = spike.error.as_deref().unwrap_or("(no message)"),
        text = diag.text,
    );
    std::fs::write(&path, body)?;
    Ok(path)
}
