//! Prompt registry — `list_prompts` (registry entries with their label pointers) and `get_prompt`
//! (one resolved version's text). The registry is versioned + benchmark-gated, so the list surfaces
//! each prompt's live labels and linked benchmark, and the resolved view shows the concrete content.

use serde_json::Value;

use crate::md::{s, short_ts, trunc, Align, Table};

/// `list_prompts` → a table of registry entries: name, current label→version pointers, linked
/// benchmark (the promotion gate), and last-updated.
pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No prompts registered._".to_string());
    }
    let mut t = Table::new(&[
        ("Name", Align::Left),
        ("Labels", Align::Left),
        ("Benchmark", Align::Left),
        ("Updated", Align::Left),
        ("Prompt id", Align::Left),
    ]);
    for r in rows {
        t.row(vec![
            trunc(s(r, "name"), 28),
            labels(r),
            r.get("benchmark_id").and_then(Value::as_str).unwrap_or("—").to_string(),
            short_ts(s(r, "updated_at")),
            s(r, "id").to_string(),
        ]);
    }
    Some(t.render())
}

/// `get_prompt` → one resolved version: which version/label was resolved, then the content verbatim.
pub(crate) fn resolved(v: &Value) -> Option<String> {
    let name = s(v, "name");
    let content = v.get("content").and_then(Value::as_str)?;
    if name.is_empty() {
        return None;
    }
    let version = v.get("version").and_then(Value::as_u64).unwrap_or(0);
    let mut out = format!("### Prompt `{name}` — v{version}");
    if let Some(lbl) = v.get("label").and_then(Value::as_str) {
        out.push_str(&format!(" (`{lbl}`)"));
    }
    out.push_str("\n\n");
    if let Some(note) = v.get("note").and_then(Value::as_str) {
        out.push_str(&format!("- **Note:** {note}\n"));
    }
    out.push_str("\n```\n");
    out.push_str(content);
    if !content.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("```\n");
    Some(out)
}

/// Render a prompt's `labels` map (`{"production": 3}`) as `production→3, staging→5`, or `—` when
/// nothing is promoted yet.
fn labels(r: &Value) -> String {
    match r.get("labels").and_then(Value::as_object) {
        Some(m) if !m.is_empty() => {
            let mut parts: Vec<String> = m
                .iter()
                .map(|(k, ver)| format!("{k}→{}", ver.as_u64().unwrap_or(0)))
                .collect();
            parts.sort();
            parts.join(", ")
        }
        _ => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_shows_labels_and_benchmark() {
        let v = json!([{
            "id": "pr-1", "name": "support-reply",
            "labels": { "production": 3, "staging": 5 },
            "benchmark_id": "bm-9", "updated_at": "2026-07-01T10:20:30Z"
        }]);
        let md = list(&v).unwrap();
        assert!(md.contains("support-reply"));
        assert!(md.contains("production→3"));
        assert!(md.contains("staging→5"));
        assert!(md.contains("bm-9"));
    }

    #[test]
    fn list_empty_is_friendly() {
        assert_eq!(list(&json!([])).unwrap(), "_No prompts registered._");
    }

    #[test]
    fn resolved_renders_version_label_and_content() {
        let v = json!({
            "name": "support-reply", "version": 3, "label": "production",
            "content": "You are a helpful support agent.", "note": "tightened tone"
        });
        let md = resolved(&v).unwrap();
        assert!(md.contains("`support-reply` — v3"));
        assert!(md.contains("`production`"));
        assert!(md.contains("tightened tone"));
        assert!(md.contains("You are a helpful support agent."));
    }

    #[test]
    fn resolved_requires_content() {
        assert!(resolved(&json!({ "name": "x" })).is_none());
    }
}
