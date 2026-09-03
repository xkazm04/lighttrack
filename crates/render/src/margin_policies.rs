//! `list_margin_policies` — the standing guardrails that turn a loss-making or eroding customer
//! into a usage cap without a human in the loop.
//!
//! The column that matters is what the policy *does*, not that it exists: a policy whose action is
//! `alert` tells someone, one whose action creates a rule spends the tenant's ability to call the
//! provider. Reading a list where those look identical is how an operator is surprised by a cap.

use serde_json::Value;

use crate::md::{opt_f, pct, s, short_ts, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No margin policies. Nothing creates a cap automatically here._".to_string());
    }
    let mut t = Table::new(&[
        ("Trigger", Align::Left),
        ("Below", Align::Right),
        ("Action", Align::Left),
        ("On", Align::Left),
        ("Created", Align::Left),
        ("Policy id", Align::Left),
    ]);
    for r in rows {
        let trigger = r
            .get("trigger")
            .and_then(|t| t.get("kind").and_then(Value::as_str).or(t.as_str()))
            .unwrap_or("—");
        let action = r
            .get("action")
            .and_then(|a| a.get("kind").and_then(Value::as_str).or(a.as_str()))
            .unwrap_or("—");
        let enabled = r.get("enabled").and_then(Value::as_bool).unwrap_or(true);
        t.row(vec![
            trigger.to_string(),
            opt_f(r, "threshold_pct")
                .or_else(|| opt_f(r, "below"))
                .map(|x| pct(x / 100.0))
                .unwrap_or_else(|| "—".into()),
            action.to_string(),
            if enabled { "✅".into() } else { "—".into() },
            short_ts(s(r, "created_at")),
            s(r, "id").to_string(),
        ]);
    }
    Some(t.render())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_empty_list_says_nothing_creates_a_cap_here() {
        let md = list(&json!([])).expect("renders");
        assert!(md.contains("Nothing creates a cap automatically"));
    }

    /// A policy that only alerts and one that mints a rule must not read the same.
    #[test]
    fn the_action_is_a_column_of_its_own() {
        let md = list(&json!([
            { "id": "mp1", "trigger": {"kind": "margin_below"}, "threshold_pct": 12.5,
              "action": {"kind": "create_limit"}, "enabled": true, "created_at": "2026-09-01T00:00:00.000000000Z" },
            { "id": "mp2", "trigger": {"kind": "margin_eroding"}, "action": {"kind": "alert"}, "enabled": false }
        ]))
        .expect("renders");
        assert!(md.contains("create_limit"));
        assert!(md.contains("alert"));
        assert!(md.contains("mp1") && md.contains("mp2"));
    }

    #[test]
    fn a_non_array_falls_back_to_json() {
        assert!(list(&json!({})).is_none());
    }
}
