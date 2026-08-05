//! `list_rubrics` and `get_rubric` (weighted, anchored scoring dimensions).

use serde_json::Value;

use crate::md::{opt_f, s, short_ts, trunc, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No rubrics._".to_string());
    }
    let mut t = Table::new(&[
        ("Name", Align::Left),
        ("Dims", Align::Right),
        ("Threshold", Align::Right),
        ("Created", Align::Left),
        ("Rubric id", Align::Left),
    ]);
    for r in rows {
        let dims = r
            .get("dimensions")
            .and_then(Value::as_array)
            .map(|a| a.len())
            .unwrap_or(0);
        t.row(vec![
            trunc(s(r, "name"), 28),
            dims.to_string(),
            format!("{:.2}", opt_f(r, "threshold").unwrap_or(0.7)),
            short_ts(s(r, "created_at")),
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
    let dims = v.get("dimensions").and_then(Value::as_array)?;
    let mut out = format!("### Rubric `{}`\n\n", s(v, "name"));
    out.push_str(&format!("- **Id:** {id}\n"));
    out.push_str(&format!(
        "- **Pass threshold:** {:.2}\n\n",
        opt_f(v, "threshold").unwrap_or(0.7)
    ));
    let mut t = Table::new(&[
        ("Dimension", Align::Left),
        ("Weight", Align::Right),
        ("Floor", Align::Right),
        ("Measures", Align::Left),
    ]);
    for d in dims {
        let floor = opt_f(d, "floor")
            .map(|x| format!("{x:.2}"))
            .unwrap_or_else(|| "—".into());
        t.row(vec![
            s(d, "key").to_string(),
            format!("{:.2}", opt_f(d, "weight").unwrap_or(1.0)),
            floor,
            trunc(s(d, "description"), 48),
        ]);
    }
    out.push_str(&t.render());
    Some(out)
}
