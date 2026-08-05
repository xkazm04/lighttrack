//! `list_datasets`, `get_dataset`, and `list_dataset_items`.

use serde_json::Value;

use crate::md::{opt_s, s, short_ts, trunc, u, Align, Table};

pub(crate) fn list(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No datasets._".to_string());
    }
    let mut t = Table::new(&[
        ("Name", Align::Left),
        ("Ver", Align::Right),
        ("Frozen", Align::Left),
        ("Source", Align::Left),
        ("Created", Align::Left),
        ("Dataset id", Align::Left),
    ]);
    for r in rows {
        let frozen = r.get("frozen").and_then(Value::as_bool).unwrap_or(false);
        t.row(vec![
            trunc(s(r, "name"), 28),
            u(r, "version").to_string(),
            if frozen { "🔒".into() } else { "—".into() },
            opt_s(r, "source")
                .filter(|x| !x.is_empty())
                .map(|x| trunc(x, 18))
                .unwrap_or_else(|| "—".into()),
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
    let frozen = v.get("frozen").and_then(Value::as_bool).unwrap_or(false);
    let mut out = format!(
        "### Dataset `{}` {}\n\n",
        s(v, "name"),
        if frozen {
            "🔒 frozen"
        } else {
            "✏️ editable"
        }
    );
    out.push_str(&format!("- **Id:** {id}\n"));
    out.push_str(&format!("- **Version:** {}\n", u(v, "version")));
    if let Some(src) = opt_s(v, "source").filter(|x| !x.is_empty()) {
        out.push_str(&format!("- **Source:** {src}\n"));
    }
    out.push_str(&format!(
        "- **Created:** {}\n",
        short_ts(s(v, "created_at"))
    ));
    Some(out)
}

pub(crate) fn items(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some("_No items in this dataset._".to_string());
    }
    let mut t = Table::new(&[
        ("#", Align::Right),
        ("Input", Align::Left),
        ("Expected", Align::Left),
        ("Tags", Align::Left),
        ("Item id", Align::Left),
    ]);
    for (i, r) in rows.iter().enumerate() {
        let expected = opt_s(r, "expected")
            .filter(|x| !x.is_empty())
            .map(|x| trunc(x, 32))
            .unwrap_or_else(|| "—".into());
        let tags = r
            .get("tags")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        let tags_cell = if tags.is_empty() {
            "—".into()
        } else {
            trunc(&tags, 18)
        };
        t.row(vec![
            (i + 1).to_string(),
            trunc(s(r, "input"), 40),
            expected,
            tags_cell,
            s(r, "id").to_string(),
        ]);
    }
    Some(format!("**{} item(s)**\n\n{}", rows.len(), t.render()))
}
