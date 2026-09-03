//! `query_events` (summary table) and `get_event` (single-call detail with payloads).

use serde_json::Value;

use crate::md::{
    commafy, fenced, money, opt_f, opt_s, opt_u, s, short_ts, status_glyph, trunc, u, Align, Table,
};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No events._".to_string());
    }
    let mut t = Table::new(&[
        ("When", Align::Left),
        ("Model", Align::Left),
        ("Tok", Align::Right),
        ("Cost", Align::Right),
        ("ms", Align::Right),
        ("Event id", Align::Left),
    ]);
    for r in rows {
        let provider = s(r, "provider");
        let model = s(r, "model");
        let model_cell = if provider.is_empty() {
            model.to_string()
        } else {
            format!("{provider}/{model}")
        };
        let tok = r
            .get("usage")
            .map(|x| u(x, "input") + u(x, "output"))
            .unwrap_or(0);
        let status = s(r, "status");
        let when = short_ts(s(r, "ts"));
        let when_cell = if status.is_empty() || status == "success" {
            when
        } else {
            format!("{} {when}", status_glyph(status))
        };
        t.row(vec![
            when_cell,
            trunc(&model_cell, 28),
            commafy(tok),
            opt_f(r, "cost_usd")
                .map(money)
                .unwrap_or_else(|| "—".into()),
            opt_u(r, "latency_ms")
                .map(|m| m.to_string())
                .unwrap_or_else(|| "—".into()),
            s(r, "id").to_string(),
        ]);
    }
    Some(format!(
        "**{} event(s)** (newest first)\n\n{}",
        rows.len(),
        t.render()
    ))
}

pub(crate) fn detail(v: &Value) -> Option<String> {
    let id = s(v, "id");
    if !v.is_object() || id.is_empty() {
        return None;
    }
    let status = s(v, "status");
    let glyph = if status.is_empty() || status == "success" {
        "✅"
    } else {
        status_glyph(status)
    };
    let (in_t, out_t) = v
        .get("usage")
        .map(|x| (u(x, "input"), u(x, "output")))
        .unwrap_or((0, 0));

    let mut out = format!("### Event `{id}` {glyph}\n\n");
    out.push_str(&format!("- **When:** {}\n", short_ts(s(v, "ts"))));
    out.push_str(&format!(
        "- **Model:** {}/{}",
        s(v, "provider"),
        s(v, "model")
    ));
    let op = s(v, "operation");
    if !op.is_empty() {
        out.push_str(&format!(" ({op})"));
    }
    out.push('\n');
    out.push_str(&format!(
        "- **Tokens:** {} in / {} out\n",
        commafy(in_t),
        commafy(out_t)
    ));
    if let Some(c) = opt_f(v, "cost_usd") {
        out.push_str(&format!("- **Cost:** {}\n", money(c)));
    }
    if let Some(ms) = opt_u(v, "latency_ms") {
        out.push_str(&format!("- **Latency:** {ms} ms\n"));
    }
    if let Some(src) = opt_s(v, "source").filter(|x| !x.is_empty()) {
        out.push_str(&format!("- **Source:** {src}\n"));
    }
    if let Some(tags) = v
        .get("tags")
        .and_then(Value::as_array)
        .filter(|t| !t.is_empty())
    {
        let joined: Vec<&str> = tags.iter().filter_map(Value::as_str).collect();
        out.push_str(&format!("- **Tags:** {}\n", joined.join(", ")));
    }
    if let Some(err) = opt_s(v, "error").filter(|x| !x.is_empty()) {
        out.push_str(&format!("- **Error:** {err}\n"));
    }
    if let Some(p) = v.get("input") {
        out.push_str(&payload_block("Input", p));
    }
    if let Some(p) = v.get("output") {
        out.push_str(&payload_block("Output", p));
    }
    Some(out)
}

/// Render a payload (string or JSON) in a fenced block. Single-event detail keeps a generous budget
/// since inspecting the payload is often the point of fetching one event.
fn payload_block(label: &str, v: &Value) -> String {
    let raw = match v.as_str() {
        Some(s) => s.to_string(),
        None => serde_json::to_string_pretty(v).unwrap_or_default(),
    };
    format!("\n**{label}:**\n{}", fenced("", &trunc(&raw, 4000)))
}

#[cfg(test)]
mod tests {
    use super::detail;
    use serde_json::json;

    /// An LLM output that itself contains a code fence used to close the payload block early and
    /// render the rest of the answer as page Markdown.
    #[test]
    fn a_payload_containing_a_fence_stays_inside_its_block() {
        let md = detail(&json!({
            "id": "ev-1", "ts": "2026-06-17T12:34:56Z", "provider": "p", "model": "m",
            "output": "Here:\n```python\nprint(1)\n```\nDone."
        }))
        .unwrap();
        assert!(md.contains("\n````\nHere:"), "{md}");
        assert!(md.trim_end().ends_with("Done.\n````"), "{md}");
    }
}
