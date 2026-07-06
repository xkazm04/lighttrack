//! Persist a diagnosis (and any auto-fix outcome) as a Markdown file under the report directory.
//! Generic over the trigger (an error spike or a quality regression).

use std::path::{Path, PathBuf};

use crate::act::ActOutcome;
use crate::claude::ClaudeRun;

#[allow(clippy::too_many_arguments)]
pub(crate) fn write_report(
    dir: &str,
    ts: &str,
    project_id: &str,
    trigger_kind: &str,
    trigger_detail: &str,
    diag: &ClaudeRun,
    act: Option<&ActOutcome>,
) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let safe: String = project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = Path::new(dir).join(format!("{safe}-{ts}.md"));

    let cost = diag.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".to_string());
    let mut body = format!(
        "# Diagnosis — {project_id}\n\n\
         - when: {ts}\n\
         - trigger: {trigger_kind}\n\
         - model: {model}\n\
         - cost: {cost}\n\
         - status: {ok}\n\n\
         ## Trigger\n\n```\n{trigger_detail}\n```\n\n\
         ## Investigation\n\n{text}\n",
        model = diag.model,
        ok = if diag.ok { "ok" } else { "FAILED" },
        text = diag.text,
    );
    if let Some(a) = act {
        body.push_str(&render_act(a));
    }
    std::fs::write(&path, body)?;
    Ok(path)
}

fn render_act(a: &ActOutcome) -> String {
    if let Some(reason) = &a.skipped_reason {
        return format!("\n## Auto-fix\n\n_skipped: {reason}_\n");
    }
    let branch = a.branch.as_deref().unwrap_or("-");
    let tests = match a.tests {
        Some(true) => "passed",
        Some(false) => "FAILED",
        None => "not run",
    };
    let cost = a.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".to_string());
    format!(
        "\n## Auto-fix\n\n\
         - branch: `{branch}`\n\
         - applied: {applied}\n\
         - tests: {tests}\n\
         - cost: {cost}\n\n\
         {notes}\n",
        applied = a.applied,
        notes = a.notes,
    )
}
