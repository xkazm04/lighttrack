//! `list_jobs` (queue table) and `get_job` (single job + result summary).

use serde_json::Value;

use crate::md::{fenced, opt_s, s, short_ts, status_glyph, trunc, u, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No jobs._".to_string());
    }
    let mut t = Table::new(&[
        ("Updated", Align::Left),
        ("Type", Align::Left),
        ("Status", Align::Left),
        ("Try", Align::Right),
        ("Progress", Align::Left),
        ("Job id", Align::Left),
    ]);
    for r in rows {
        let status = s(r, "status");
        let progress = match opt_s(r, "progress").filter(|p| !p.is_empty()) {
            Some(p) => trunc(p, 28),
            None => "—".into(),
        };
        t.row(vec![
            short_ts(s(r, "updated_at")),
            s(r, "type").to_string(),
            format!("{} {status}", status_glyph(status)),
            format!("{}/{}", u(r, "attempts"), u(r, "max_attempts")),
            progress,
            s(r, "id").to_string(),
        ]);
    }
    Some(t.render())
}

pub(crate) fn detail(v: &Value) -> Option<String> {
    let id = s(v, "id");
    if !v.is_object() || id.is_empty() {
        return None;
    }
    let status = s(v, "status");
    let mut out = format!("### Job `{id}` {} {status}\n\n", status_glyph(status));
    out.push_str(&format!("- **Type:** {}\n", s(v, "type")));
    out.push_str(&format!(
        "- **Attempts:** {}/{}\n",
        u(v, "attempts"),
        u(v, "max_attempts")
    ));
    if let Some(p) = opt_s(v, "progress").filter(|p| !p.is_empty()) {
        out.push_str(&format!("- **Progress:** {p}\n"));
    }
    if let Some(e) = opt_s(v, "error").filter(|e| !e.is_empty()) {
        out.push_str(&format!("- **Error:** {e}\n"));
    }
    out.push_str(&format!(
        "- **Updated:** {}\n",
        short_ts(s(v, "updated_at"))
    ));
    if let Some(res) = v.get("result").filter(|r| !r.is_null()) {
        let pretty = serde_json::to_string_pretty(res).unwrap_or_default();
        out.push_str(&format!(
            "\n**Result:**\n{}",
            fenced("json", &trunc(&pretty, 1500))
        ));
    }
    Some(out)
}
