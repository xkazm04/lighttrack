//! `get_prompt_quality` → per-served-version quality: how good each version of each registry prompt
//! is actually scoring in production.
//!
//! The interval is rendered as a column, not hidden behind the mean, because the table's whole job
//! is to stop a reader from concluding that a version judged four times beats one judged four
//! hundred. A row that cannot support a conclusion says so in the `n` beside it.

use serde_json::Value;

use crate::md::{f, Align, Table};

/// The evidence floor below which a row is marked as too thin to read as a comparison. Matches the
/// spirit of `CanaryPolicy::min_n`'s default rather than its exact value: this is a reading aid, not
/// a gate.
const THIN_N: u64 = 20;

pub(crate) fn table(v: &Value) -> Option<String> {
    let rows = v.as_array()?;
    if rows.is_empty() {
        return Some(
            "_No scored traffic for any prompt version in this window._ Stamp `metadata.prompt` \
             with the tag from `GET /v1/projects/:id/prompts/:name` and score some events."
                .to_string(),
        );
    }
    let mut t = Table::new(&[
        ("Prompt", Align::Left),
        ("Ver", Align::Right),
        ("n", Align::Right),
        ("Mean", Align::Right),
        ("95% CI", Align::Left),
        ("Pass", Align::Right),
        ("Cost", Align::Right),
    ]);
    for r in rows {
        let n = r.get("n").and_then(Value::as_u64).unwrap_or(0);
        t.row(vec![
            name(r),
            version(r),
            match n < THIN_N {
                true => format!("{n} ⚠"),
                false => n.to_string(),
            },
            format!("{:.3}", f(r, "mean")),
            format!("{:.3}–{:.3}", f(r, "ci95_low"), f(r, "ci95_high")),
            format!("{:.0}%", f(r, "pass_rate") * 100.0),
            format!("${:.4}", f(r, "cost_usd")),
        ]);
    }
    let mut out = t.render();
    if rows
        .iter()
        .any(|r| r.get("n").and_then(Value::as_u64).unwrap_or(0) < THIN_N)
    {
        out.push_str(&format!(
            "\n⚠ = fewer than {THIN_N} verdicts: the interval is wide and the mean is not yet \
             evidence of anything.\n"
        ));
    }
    Some(out)
}

/// The prompt's name, falling back to the raw tag for a tag that does not follow the
/// `name@vN` convention, and to an explicit label for the untagged bucket — which is a finding
/// (the app is not stamping the tag), not an empty cell.
fn name(r: &Value) -> String {
    match r.get("name").and_then(Value::as_str) {
        Some(n) => n.to_string(),
        None => match r.get("tag").and_then(Value::as_str) {
            Some(t) => t.to_string(),
            None => "(untagged)".to_string(),
        },
    }
}

fn version(r: &Value) -> String {
    match r.get("version").and_then(Value::as_u64) {
        Some(v) => format!("v{v}"),
        None => "—".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(name: &str, version: u64, n: u64) -> Value {
        json!({
            "tag": format!("{name}@v{version}"), "name": name, "version": version,
            "n": n, "mean": 0.812, "pass_rate": 0.75,
            "ci95_low": 0.79, "ci95_high": 0.83, "cost_usd": 1.2345,
        })
    }

    #[test]
    fn the_table_shows_the_interval_and_the_sample_size_beside_the_mean() {
        let md = table(&json!([row("support-reply", 4, 120)])).unwrap();
        assert!(md.contains("support-reply") && md.contains("v4"), "{md}");
        assert!(md.contains("0.812"), "{md}");
        assert!(md.contains("0.790–0.830"), "the interval is a column: {md}");
        assert!(md.contains("75%") && md.contains("$1.2345"), "{md}");
        assert!(!md.contains('⚠'), "120 verdicts is not thin: {md}");
    }

    /// The reading aid that matters: a row nobody should draw a conclusion from is marked, and the
    /// footnote says why.
    #[test]
    fn a_thin_row_is_flagged_and_explained() {
        let md = table(&json!([row("support-reply", 5, 3)])).unwrap();
        assert!(md.contains("3 ⚠"), "{md}");
        assert!(md.contains("not yet evidence"), "{md}");
    }

    #[test]
    fn an_untagged_bucket_reads_as_a_finding_not_an_empty_cell() {
        let md = table(&json!([{
            "tag": null, "n": 40, "mean": 0.5, "pass_rate": 0.5,
            "ci95_low": 0.4, "ci95_high": 0.6, "cost_usd": 0.1
        }]))
        .unwrap();
        assert!(md.contains("(untagged)"), "{md}");
        assert!(md.contains('—'), "no version to show: {md}");
    }

    #[test]
    fn an_empty_window_says_what_to_do_about_it() {
        let md = table(&json!([])).unwrap();
        assert!(md.contains("metadata.prompt"), "{md}");
    }

    /// A tag written by a client with its own scheme is still rendered — it must not be dropped from
    /// the table just because it is not `name@vN`.
    #[test]
    fn an_unconventional_tag_is_shown_whole() {
        let md = table(&json!([{
            "tag": "legacy-7", "n": 30, "mean": 0.6, "pass_rate": 0.6,
            "ci95_low": 0.5, "ci95_high": 0.7, "cost_usd": 0.0
        }]))
        .unwrap();
        assert!(md.contains("legacy-7"), "{md}");
    }

    #[test]
    fn a_non_array_body_has_no_renderer() {
        assert!(table(&json!({ "error": "nope" })).is_none());
    }
}
