//! `list_traces` (rollup table) and `get_trace` (totals + span tree + scores within the trace).

use serde_json::Value;

use crate::md::{
    commafy, money, opt_f, opt_u, pass_glyph, s, short_ts, status_glyph, trunc, u, Align, Table,
};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No traces._".to_string());
    }
    let mut t = Table::new(&[
        ("When", Align::Left),
        ("Spans", Align::Right),
        ("Tok", Align::Right),
        ("Cost", Align::Right),
        ("Dur", Align::Right),
        ("Models", Align::Left),
        ("Trace id", Align::Left),
    ]);
    for r in rows {
        let status = s(r, "status");
        let when = short_ts(s(r, "ended_at"));
        let when_cell = if status == "error" {
            format!("{} {when}", status_glyph(status))
        } else {
            when
        };
        let models = r
            .get("models")
            .and_then(Value::as_array)
            .map(|m| m.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))
            .unwrap_or_default();
        t.row(vec![
            when_cell,
            u(r, "spans").to_string(),
            commafy(u(r, "total_tokens")),
            opt_f(r, "cost_usd").map(money).unwrap_or_else(|| "—".into()),
            dur(opt_u(r, "duration_ms").unwrap_or(0)),
            trunc(&models, 28),
            trunc(s(r, "trace_id"), 28),
        ]);
    }
    Some(format!("**{} trace(s)** (newest first)\n\n{}", rows.len(), t.render()))
}

pub(crate) fn tree(v: &Value) -> Option<String> {
    let trace_id = s(v, "trace_id");
    if !v.is_object() || trace_id.is_empty() {
        return None;
    }
    let status = s(v, "status");
    let glyph = if status == "error" { "❌" } else { "✅" };
    let totals = v.get("totals").unwrap_or(&Value::Null);

    let mut out = format!("### Trace `{trace_id}` {glyph}\n\n");
    let when = short_ts(s(v, "started_at"));
    // Wall duration, plus total compute time when present (they differ when spans overlap or idle).
    let wall = dur(opt_u(v, "duration_ms").unwrap_or(0));
    let timing = match opt_u(totals, "total_latency_ms") {
        Some(c) => format!("{wall} wall · {} compute", dur(c)),
        None => wall,
    };
    out.push_str(&format!("- **When:** {when} · {timing}\n"));
    out.push_str(&format!("- **Spans:** {}\n", u(totals, "spans")));
    let (in_t, out_t) = (u(totals, "input_tokens"), u(totals, "output_tokens"));
    out.push_str(&format!(
        "- **Tokens:** {} in / {} out\n",
        commafy(in_t),
        commafy(out_t)
    ));
    if let Some(c) = opt_f(totals, "cost_usd") {
        out.push_str(&format!("- **Cost:** {}\n", money(c)));
    }
    if let Some(models) = v.get("models").and_then(Value::as_array).filter(|m| !m.is_empty()) {
        let joined: Vec<&str> = models.iter().filter_map(Value::as_str).collect();
        out.push_str(&format!("- **Models:** {}\n", joined.join(", ")));
    }
    let errors = u(totals, "errors");
    if errors > 0 {
        out.push_str(&format!("- **Errors:** {errors}\n"));
    }

    if let Some(spans) = v.get("spans").and_then(Value::as_array).filter(|s| !s.is_empty()) {
        out.push_str("\n**Spans:**\n");
        for node in spans {
            render_node(node, 0, &mut out);
        }
    }

    if let Some(scores) = v.get("scores").and_then(Value::as_array).filter(|s| !s.is_empty()) {
        out.push_str("\n**Scores:**\n");
        for sc in scores {
            let value = opt_f(sc, "value").unwrap_or(0.0);
            let max = opt_f(sc, "max").unwrap_or(1.0);
            let score_cell = if (max - 1.0).abs() < 1e-9 {
                format!("{value:.2}")
            } else {
                format!("{value:.2}/{max:.0}")
            };
            out.push_str(&format!(
                "- {} {}: {score_cell} by {}\n",
                pass_glyph(sc.get("pass").and_then(Value::as_bool)),
                trunc(s(sc, "rubric"), 36),
                trunc(s(sc, "scored_by"), 22),
            ));
        }
    }
    Some(out)
}

/// One span as an indented bullet, then its children one level deeper.
fn render_node(node: &Value, depth: usize, out: &mut String) {
    let ev = node.get("event").unwrap_or(&Value::Null);
    let status = s(ev, "status");
    let glyph = if status.is_empty() || status == "success" {
        "✅"
    } else {
        status_glyph(status)
    };
    let (in_t, out_t) = ev
        .get("usage")
        .map(|x| (u(x, "input"), u(x, "output")))
        .unwrap_or((0, 0));
    let cost = opt_f(ev, "cost_usd").map(money).unwrap_or_else(|| "—".into());
    // Waterfall placement: `@<offset>ms +<latency>ms`. Offset/latency live on the span node (with
    // latency mirrored from the event); fall back to the event for older payloads.
    let offset = opt_u(node, "offset_ms").unwrap_or(0);
    let lat = opt_u(node, "latency_ms")
        .or_else(|| opt_u(ev, "latency_ms"))
        .map(|m| format!("+{m}ms"))
        .unwrap_or_else(|| "+—".into());
    let model = {
        let provider = s(ev, "provider");
        let m = s(ev, "model");
        if provider.is_empty() { m.to_string() } else { format!("{provider}/{m}") }
    };
    out.push_str(&format!(
        "{}- {glyph} `{}` · @{offset}ms {lat} · {}/{} tok · {cost}\n",
        "  ".repeat(depth),
        trunc(&model, 30),
        commafy(in_t),
        commafy(out_t),
    ));
    if let Some(children) = node.get("children").and_then(Value::as_array) {
        for child in children {
            render_node(child, depth + 1, out);
        }
    }
}

/// Compact duration: sub-second in ms, else seconds with two decimals.
fn dur(ms: u64) -> String {
    if ms >= 1000 {
        format!("{:.2}s", ms as f64 / 1000.0)
    } else {
        format!("{ms} ms")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn list_renders_rows_or_empty() {
        assert!(list(&json!([])).unwrap().contains("No traces"));
        let md = list(&json!([{
            "trace_id": "tr-1", "status": "success", "ended_at": "2026-06-21T12:34:56.000000000Z",
            "spans": 3, "total_tokens": 1234, "cost_usd": 0.007, "duration_ms": 1500,
            "models": ["a", "b"]
        }]))
        .unwrap();
        assert!(md.contains("1 trace(s)"));
        assert!(md.contains("tr-1"));
        assert!(md.contains("1.50s"));
        assert!(md.contains("1,234"));
    }

    #[test]
    fn tree_renders_nested_spans_and_scores() {
        let v = json!({
            "trace_id": "tr-1", "status": "success",
            "started_at": "2026-06-21T12:34:56.000000000Z", "duration_ms": 500,
            "models": ["m1"],
            "totals": { "spans": 2, "input_tokens": 200, "output_tokens": 100, "cost_usd": 0.003,
                        "errors": 0, "total_latency_ms": 200 },
            "spans": [{
                "offset_ms": 0, "latency_ms": 120,
                "event": { "provider": "anthropic", "model": "m1", "status": "success",
                           "usage": { "input": 100, "output": 50 }, "cost_usd": 0.001, "latency_ms": 120 },
                "children": [{
                    "offset_ms": 120, "latency_ms": 80,
                    "event": { "provider": "anthropic", "model": "m1", "status": "success",
                               "usage": { "input": 100, "output": 50 }, "cost_usd": 0.002, "latency_ms": 80 },
                    "children": []
                }]
            }],
            "scores": [{ "rubric": "coherence", "value": 0.9, "max": 1.0, "pass": true, "scored_by": "judge" }]
        });
        let md = tree(&v).unwrap();
        assert!(md.contains("### Trace `tr-1`"));
        assert!(md.contains("**Spans:**"));
        assert!(md.contains("  - ✅"), "child span is indented one level: {md}");
        // Waterfall placement per node + total compute time in the header.
        assert!(md.contains("@0ms +120ms"), "root waterfall: {md}");
        assert!(md.contains("@120ms +80ms"), "child waterfall: {md}");
        assert!(md.contains("compute"), "header shows total compute time: {md}");
        assert!(md.contains("**Scores:**"));
        assert!(md.contains("coherence"));
        // Not an object / no id -> no render.
        assert!(tree(&json!([])).is_none());
    }
}
