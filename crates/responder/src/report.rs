//! Persist a diagnosis (and any auto-fix outcome) as a Markdown file under the report directory.
//! Generic over the trigger (an error spike or a quality regression).

use std::path::{Path, PathBuf};

use crate::act::ActOutcome;
use crate::claude::ClaudeRun;

/// Build the Markdown report body (also used verbatim as the email body).
#[allow(clippy::too_many_arguments)]
pub(crate) fn render(
    project_id: &str,
    ts: &str,
    trigger_kind: &str,
    trigger_detail: &str,
    diag: &ClaudeRun,
    act: Option<&ActOutcome>,
) -> String {
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
    body
}

/// Write a rendered report to `dir/{project}-{ts}.md`, returning the path.
pub(crate) fn write(dir: &str, project_id: &str, ts: &str, markdown: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let safe: String = project_id
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    let path = Path::new(dir).join(format!("{safe}-{ts}.md"));
    std::fs::write(&path, markdown)?;
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

// ── HTML email body ───────────────────────────────────────────────────────────
// Email clients don't render Markdown, so the email carries an HTML template (the Markdown stays the
// text fallback + the local file). Styles are embedded (Gmail/Apple Mail honor a <style> block).

const EMAIL_CSS: &str = "body{margin:0;background:#f4f5f7;font-family:-apple-system,Segoe UI,Roboto,Helvetica,Arial,sans-serif;color:#1f2430}\
.wrap{padding:24px}\
.card{max-width:640px;margin:0 auto;background:#fff;border:1px solid #e5e7eb;border-radius:12px;overflow:hidden}\
.head{padding:18px 22px;background:#fafafa;border-bottom:1px solid #eee}\
.proj{font-size:18px;font-weight:700}\
.kind{margin-top:4px;font-size:12px;color:#6b7280;text-transform:uppercase;letter-spacing:.04em}\
.badge{float:right;color:#fff;font-size:12px;font-weight:700;padding:3px 10px;border-radius:999px}\
.meta{border-collapse:collapse;margin:14px 22px;font-size:13px;color:#4b5563}\
.meta td{padding:3px 0}\
.meta td:first-child{width:64px;color:#9ca3af;text-transform:uppercase;font-size:11px;letter-spacing:.04em}\
.sect{margin:18px 22px 6px;font-size:12px;font-weight:700;text-transform:uppercase;letter-spacing:.05em;color:#374151}\
.trigger{margin:0 22px;padding:12px 14px;background:#0f172a;color:#e2e8f0;border-radius:8px;font-family:ui-monospace,Consolas,monospace;font-size:12px;white-space:pre-wrap;word-break:break-word}\
.md{margin:0 22px;font-size:14px;line-height:1.55}\
.md h1,.md h2,.md h3{font-size:15px;margin:16px 0 6px}\
.md p{margin:8px 0}.md ul,.md ol{margin:8px 0;padding-left:20px}\
.md code{background:#f1f5f9;padding:1px 5px;border-radius:4px;font-family:ui-monospace,Consolas,monospace;font-size:12px}\
.md pre{background:#0f172a;color:#e2e8f0;padding:12px;border-radius:8px;overflow-x:auto}\
.md pre code{background:none;color:inherit;padding:0}\
.act{margin:8px 22px 0;padding:12px 14px;border:1px solid #e5e7eb;border-radius:8px;background:#f9fafb;font-size:13px}\
.foot{margin:20px 22px;padding-top:12px;border-top:1px solid #eee;font-size:11px;color:#9ca3af}";

pub(crate) fn render_html(
    project_id: &str,
    ts: &str,
    trigger_kind: &str,
    trigger_detail: &str,
    diag: &ClaudeRun,
    act: Option<&ActOutcome>,
) -> String {
    let status_color = if diag.ok { "#16a34a" } else { "#dc2626" };
    let status_label = if diag.ok { "ok" } else { "FAILED" };
    let cost = diag.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".to_string());
    let act_html = act.map(render_act_html).unwrap_or_default();
    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><style>{css}</style></head><body><div class="wrap"><div class="card"><div class="head"><span class="badge" style="background:{status_color}">{status_label}</span><div class="proj">{project}</div><div class="kind">{kind}</div></div><table class="meta"><tr><td>when</td><td>{ts}</td></tr><tr><td>model</td><td>{model}</td></tr><tr><td>cost</td><td>{cost}</td></tr></table><div class="sect">Trigger</div><pre class="trigger">{detail}</pre><div class="sect">Investigation</div><div class="md">{investigation}</div>{act_html}<div class="foot">Generated by the LightTrack responder</div></div></div></body></html>"#,
        css = EMAIL_CSS,
        project = html_escape(project_id),
        kind = html_escape(trigger_kind),
        ts = html_escape(ts),
        model = html_escape(&diag.model),
        detail = html_escape(trigger_detail),
        investigation = md_to_html(&diag.text),
    )
}

fn render_act_html(a: &ActOutcome) -> String {
    if let Some(reason) = &a.skipped_reason {
        return format!(
            r#"<div class="sect">Auto-fix</div><div class="act">skipped: {}</div>"#,
            html_escape(reason)
        );
    }
    let tests = match a.tests {
        Some(true) => "passed",
        Some(false) => "FAILED",
        None => "not run",
    };
    let cost = a.cost_usd.map(|c| format!("${c:.4}")).unwrap_or_else(|| "n/a".to_string());
    format!(
        r#"<div class="sect">Auto-fix</div><div class="act"><b>branch</b> <code>{branch}</code> · <b>applied</b> {applied} · <b>tests</b> {tests} · <b>cost</b> {cost}<div class="md">{notes}</div></div>"#,
        branch = html_escape(a.branch.as_deref().unwrap_or("-")),
        applied = a.applied,
        notes = md_to_html(&a.notes),
    )
}

fn md_to_html(md: &str) -> String {
    let mut opts = pulldown_cmark::Options::empty();
    opts.insert(pulldown_cmark::Options::ENABLE_TABLES);
    opts.insert(pulldown_cmark::Options::ENABLE_STRIKETHROUGH);
    let parser = pulldown_cmark::Parser::new_ext(md, opts);
    let mut out = String::new();
    pulldown_cmark::html::push_html(&mut out, parser);
    out
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude::ClaudeRun;

    #[test]
    fn html_email_renders() {
        let diag = ClaudeRun {
            text: "## Root cause\n\n`add()` in `add.js` returns `a - b`, which is incorrect.\n\n\
                   ## Evidence\n\n- `add.js:2` — `return a - b;`\n\n\
                   ## Proposed fix\n\nChange line 2 to `return a + b;`.\n\n**Confidence:** high."
                .into(),
            model: "claude-sonnet-5".into(),
            cost_usd: Some(0.3466),
            ok: true,
        };
        let html = render_html(
            "systedo-dev",
            "20260706T152027Z",
            "error",
            "error x3 (status error): Claude CLI nevrátil platný JSON. Reached max turns (1)",
            &diag,
            None,
        );
        assert!(html.contains("<html"));
        assert!(html.contains("systedo-dev"));
        assert!(html.contains("<h2>Root cause</h2>")); // markdown -> html
        assert!(html.contains("<code>")); // inline code rendered
        assert!(!html.contains("## Root cause")); // no raw markdown left
        if let Ok(p) = std::env::var("RESPONDER_HTML_DUMP") {
            std::fs::write(p, &html).unwrap();
        }
    }
}
