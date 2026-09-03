//! `list_projects` — monitored applications/tenants. Full project ids are shown because the other
//! tools key off them, so the agent/operator can copy one straight into a follow-up call.

use serde_json::Value;

use crate::md::{opt_s, s, short_ts, trunc, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No projects._".to_string());
    }
    let mut t = Table::new(&[
        ("Name", Align::Left),
        ("On", Align::Left),
        ("Redaction", Align::Left),
        ("Created", Align::Left),
        ("Project id", Align::Left),
    ]);
    for r in rows {
        let enabled = r.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        // The lifecycle has three words (live | disabled | archived) and the column shows all three:
        // an archived tenant used to read exactly like a switched-off one.
        let on = if enabled {
            "✅"
        } else if opt_s(r, "archived_at").is_some_and(|a| !a.is_empty()) {
            "🗄 archived"
        } else {
            "— disabled"
        };
        t.row(vec![
            trunc(s(r, "name"), 28),
            on.to_string(),
            s(r, "redaction").to_string(),
            short_ts(s(r, "created_at")),
            s(r, "id").to_string(),
        ]);
    }
    Some(t.render())
}

#[cfg(test)]
mod tests {
    use super::list;
    use serde_json::json;

    #[test]
    fn archived_and_disabled_read_differently() {
        let md = list(&json!([
            { "id": "a", "name": "live", "enabled": true, "redaction": "none" },
            { "id": "b", "name": "paused", "enabled": false, "redaction": "none" },
            { "id": "c", "name": "gone", "enabled": false, "redaction": "none",
              "archived_at": "2026-09-01T00:00:00Z" }
        ]))
        .unwrap();
        assert!(md.contains("archived"), "{md}");
        assert!(md.contains("disabled"), "{md}");
        assert_eq!(md.matches("✅").count(), 1, "{md}");
    }
}
