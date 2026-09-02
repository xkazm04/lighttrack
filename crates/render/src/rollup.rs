//! `query_rollup` — the generic grouped table.
//!
//! The columns are not fixed: the response echoes its own `group_by`, so one renderer covers every
//! grouping the primitive can answer. The unpriced count gets a column of its own **only when some
//! bucket has one** — a total that silently excludes unpriced calls is a floor presented as a fact,
//! and the reader has to be able to see that from the table.

use serde_json::Value;

use crate::md::{commafy, f, money, u, Align, Table};

pub(crate) fn table(v: &Value) -> Option<String> {
    let dims: Vec<String> = v
        .get("group_by")?
        .as_array()?
        .iter()
        .map(|d| d.as_str().unwrap_or("?").to_string())
        .collect();
    let rows = v.get("rows")?.as_array()?;
    if rows.is_empty() {
        return Some("_No usage in this window._".to_string());
    }

    let any_unpriced = rows.iter().any(|r| u(r, "unpriced_calls") > 0);
    let any_errors = rows.iter().any(|r| u(r, "errors") > 0);

    let mut cols: Vec<(&str, Align)> = dims.iter().map(|d| (d.as_str(), Align::Left)).collect();
    cols.push(("Calls", Align::Right));
    cols.push(("In tok", Align::Right));
    cols.push(("Out tok", Align::Right));
    cols.push(("Cost", Align::Right));
    if any_unpriced {
        cols.push(("Unpriced", Align::Right));
    }
    if any_errors {
        cols.push(("Errors", Align::Right));
    }
    let mut t = Table::new(&cols);

    let mut sorted: Vec<&Value> = rows.iter().collect();
    sorted.sort_by(|a, b| f(b, "cost_usd").total_cmp(&f(a, "cost_usd")));

    let (mut calls, mut cost, mut unpriced) = (0u64, 0.0f64, 0u64);
    for r in &sorted {
        let keys = r.get("keys").and_then(Value::as_array);
        let mut cells: Vec<String> = (0..dims.len())
            .map(|i| {
                keys.and_then(|k| k.get(i))
                    .and_then(Value::as_str)
                    // A `null` key is real data — traffic that carries no value on this dimension —
                    // so it is labelled, never blanked into looking like a rendering gap.
                    .unwrap_or("(none)")
                    .to_string()
            })
            .collect();
        let c = u(r, "calls");
        let cu = f(r, "cost_usd");
        let up = u(r, "unpriced_calls");
        calls += c;
        cost += cu;
        unpriced += up;
        cells.push(commafy(c));
        cells.push(commafy(u(r, "input_tokens")));
        cells.push(commafy(u(r, "output_tokens")));
        cells.push(money(cu));
        if any_unpriced {
            cells.push(commafy(up));
        }
        if any_errors {
            cells.push(commafy(u(r, "errors")));
        }
        t.row(cells);
    }

    let time_key = v.get("time_key").and_then(Value::as_str).unwrap_or("ts");
    let caveat = if unpriced > 0 {
        format!(
            "\n> {} of {} calls carried no price, so the total is a **floor** — \
             the true spend is higher by whatever those cost.\n",
            commafy(unpriced),
            commafy(calls)
        )
    } else {
        String::new()
    };
    Some(format!(
        "### Rollup by {} (windowed on `{}`) — {} bucket(s)\n\n{}\n**Total: {} across {} calls**\n{}",
        dims.join(" × "),
        time_key,
        sorted.len(),
        t.render(),
        money(cost),
        commafy(calls),
        caveat,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn resp(rows: Value) -> Value {
        json!({ "group_by": ["customer", "day"], "time_key": "received_at", "rows": rows })
    }

    #[test]
    fn columns_follow_the_grouping_the_response_declares() {
        let md = table(&resp(json!([{
            "keys": ["acme", "2026-06-10"], "calls": 3, "input_tokens": 10,
            "output_tokens": 5, "cost_usd": 1.5, "unpriced_calls": 0,
            "client_reported_cost_usd": 0.0, "errors": 0
        }])))
        .expect("renders");
        assert!(md.contains("customer"), "{md}");
        assert!(md.contains("day"), "{md}");
        assert!(md.contains("acme") && md.contains("2026-06-10"), "{md}");
        assert!(md.contains("received_at"), "the window key is stated: {md}");
        assert!(
            !md.contains("Unpriced"),
            "no unpriced column when none: {md}"
        );
        assert!(
            !md.contains("floor"),
            "no caveat when nothing is unpriced: {md}"
        );
    }

    /// The disclosure the whole primitive exists for: a total computed with unpriced calls in it is
    /// a floor, and the reader must be told so on the same screen.
    #[test]
    fn an_unpriced_bucket_gets_a_column_and_a_caveat() {
        let md = table(&resp(json!([{
            "keys": ["heavy", "2026-06-11"], "calls": 4, "input_tokens": 0,
            "output_tokens": 0, "cost_usd": 2.0, "unpriced_calls": 2,
            "client_reported_cost_usd": 0.0, "errors": 1
        }])))
        .expect("renders");
        assert!(md.contains("Unpriced"), "{md}");
        assert!(md.contains("Errors"), "{md}");
        assert!(md.contains("floor"), "{md}");
    }

    /// A `null` key is traffic that carries no value on that dimension, not a missing cell.
    #[test]
    fn a_null_key_is_labelled_rather_than_blank() {
        let md = table(&resp(json!([{
            "keys": [null, "2026-06-11"], "calls": 1, "input_tokens": 0,
            "output_tokens": 0, "cost_usd": 0.5, "unpriced_calls": 0,
            "client_reported_cost_usd": 0.0, "errors": 0
        }])))
        .expect("renders");
        assert!(md.contains("(none)"), "{md}");
    }

    #[test]
    fn an_empty_rollup_says_so_instead_of_rendering_an_empty_table() {
        let md = table(&resp(json!([]))).expect("renders");
        assert!(md.contains("No usage"), "{md}");
        assert!(
            table(&json!({"rows": []})).is_none(),
            "no grouping, no table"
        );
    }
}
