//! `get_capabilities` — the store backend's manifest.
//!
//! The half that changes behaviour is `unsupported`: a surface listed there answers HTTP 501 on this
//! deployment, permanently, and an agent that reads an empty result instead concludes the tenant has
//! no data. So the refused list is rendered first and loudly, not as a footnote after the supported
//! one — and when it is empty that fact is stated rather than left as an absence.

use serde_json::Value;

use crate::md::{s, Align, Table};

pub(crate) fn manifest(v: &Value) -> Option<String> {
    let backend = s(v, "backend");
    if backend.is_empty() {
        return None;
    }
    let mut out = format!("**Store backend:** `{backend}`\n\n");

    let refused: Vec<&str> = list(v, "unsupported");
    if refused.is_empty() {
        out.push_str("Every surface is implemented — no route answers 501 here.\n\n");
    } else {
        out.push_str(&format!(
            "⚠️ **Refused on this backend ({}):** {}\n\n_These routes answer HTTP 501 \
             `unsupported`. That is a permanent gap on this backend, never \"you have no data\"._\n\n",
            refused.len(),
            refused
                .iter()
                .map(|x| format!("`{x}`"))
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }

    let surfaces = list(v, "surfaces");
    if !surfaces.is_empty() {
        let mut t = Table::new(&[("Surface", Align::Left), ("", Align::Left)]);
        for name in &surfaces {
            t.row(vec![(*name).to_string(), "✅".into()]);
        }
        out.push_str(&t.render());
    }

    // Advisory caps are the other thing an operator must not learn by surprise: a limit that is not
    // enforced atomically can be exceeded by concurrent ingest.
    if let Some(atomic) = v.get("atomic_admission").and_then(Value::as_bool) {
        out.push_str(if atomic {
            "\nUsage caps are enforced atomically.\n"
        } else {
            "\n⚠️ Usage caps here are **advisory**: admission is not one critical section, so \
             concurrent ingest can cross a cap before it binds.\n"
        });
    }
    Some(out)
}

fn list<'a>(v: &'a Value, key: &str) -> Vec<&'a str> {
    v.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn the_refused_surfaces_lead_and_say_what_a_501_means() {
        let md = manifest(&json!({
            "backend": "firestore",
            "surfaces": ["events", "scores"],
            "unsupported": ["rollup", "limits_usage"],
            "atomic_admission": false
        }))
        .expect("renders");
        let refused_at = md.find("Refused").expect("names the refused set");
        let surface_at = md.find("Surface").expect("lists the surfaces");
        assert!(
            refused_at < surface_at,
            "the gap comes before the inventory"
        );
        assert!(md.contains("`rollup`"));
        assert!(md.contains("never \"you have no data\""));
        assert!(md.contains("advisory"), "an advisory cap must be disclosed");
    }

    /// The reference backend refuses nothing; saying so is not the same as saying nothing.
    #[test]
    fn a_complete_backend_says_so_rather_than_leaving_a_silence() {
        let md = manifest(&json!({
            "backend": "sqlite",
            "surfaces": ["events"],
            "unsupported": [],
            "atomic_admission": true
        }))
        .expect("renders");
        assert!(md.contains("no route answers 501"));
        assert!(md.contains("enforced atomically"));
    }

    #[test]
    fn an_unrecognised_shape_falls_back_to_json() {
        assert!(manifest(&json!({})).is_none());
        assert!(manifest(&json!([])).is_none());
    }
}
