//! Golden sets that live in the database, and the import that puts them there (M11).
//!
//! Until now a calibration set was a JSONL file on whoever ran the runner's disk. That file is the
//! single input the entire judge-trust argument rests on, and it could not be listed, re-used by a
//! second calibration, diffed against last month's, or attributed to the person who graded it —
//! which is exactly why D15's calibration carries the caveat "n=12, and ours".
//!
//! [`from_dataset`] builds the same `CalibrationItem`s out of a stored dataset and its labels, so
//! `--file` becomes an *import* path rather than the only path.

use anyhow::{bail, Context, Result};
use serde_json::json;

use lighttrack_core::{CalibrationItem, DatasetItem, Label, LabelSubject};

use crate::cli::Cli;
use crate::http::{get, post};

/// Build a calibration set from a stored dataset: its items, paired with the human verdicts on them.
///
/// An item with **no** label is skipped rather than defaulted to 0.0 — a case nobody graded is not a
/// case the judge got wrong, and folding it in as a zero would manufacture a judge regression out of
/// an incomplete labelling pass. The count of skips is returned so the caller can say so out loud.
pub(crate) fn from_dataset(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    dataset_id: &str,
) -> Result<(Vec<CalibrationItem>, usize)> {
    let items: Vec<DatasetItem> = get(cli, http, &format!("/v1/datasets/{dataset_id}/items"))
        .with_context(|| format!("reading dataset {dataset_id}"))?;
    let labels: Vec<Label> = get(cli, http, &format!("/v1/datasets/{dataset_id}/labels"))
        .with_context(|| format!("reading labels for dataset {dataset_id}"))?;
    Ok(pair(&items, &labels))
}

/// Pair items with their labels. Pure, so the "an unlabelled case is skipped, never scored 0" rule
/// is testable without a server.
fn pair(items: &[DatasetItem], labels: &[Label]) -> (Vec<CalibrationItem>, usize) {
    let mut out = Vec::new();
    let mut unlabelled = 0usize;
    for it in items {
        // Newest-first would be wrong here: `labels_for_dataset` returns oldest-first, and the
        // *latest* grade is the current one, so the last match wins.
        let label = labels
            .iter()
            .rfind(|l| matches!(&l.subject, LabelSubject::DatasetItem(id) if id == &it.id));
        let Some(l) = label else {
            unlabelled += 1;
            continue;
        };
        // A calibration is judge-only: the output being judged must already exist. An item with no
        // stored output has nothing to re-judge, so it is not a calibration case either.
        let Some(output) = it.output.clone() else {
            unlabelled += 1;
            continue;
        };
        out.push(CalibrationItem {
            input: it.input.clone(),
            output,
            context: it.context.clone(),
            expected: it.expected.clone(),
            human_score: l.value,
            note: l.note.clone().or_else(|| Some(l.labeler.clone())),
        });
    }
    (out, unlabelled)
}

/// One line of a labels import file. Deliberately the *same* shape a calibration JSONL already has
/// (`human_score` + optional `note`) plus the subject and labeler, so an existing golden file
/// becomes importable by adding two fields rather than being rewritten.
#[derive(Debug, serde::Deserialize)]
struct ImportRow {
    /// `"event:<id>"` / `"dataset_item:<id>"` / `"score:<id>"`.
    subject: String,
    human_score: f64,
    #[serde(default)]
    pass: Option<bool>,
    #[serde(default)]
    rubric_id: Option<String>,
    #[serde(default)]
    labeler: Option<String>,
    #[serde(default)]
    note: Option<String>,
}

/// `lt-runner labels import <file>` — write a labelled file into the ledger through the API.
///
/// The migration path off files: run it once and the same grades become queryable, re-usable and
/// attributable. `labeler` falls back to `import:<file>` so a row that names nobody is still
/// attributable to *something* — an unattributable verdict is the one thing the ledger refuses.
pub(crate) fn import(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    file: &str,
    project: Option<&str>,
    labeler: Option<&str>,
) -> Result<()> {
    let text = std::fs::read_to_string(file).with_context(|| format!("reading {file}"))?;
    let rows = parse_rows(&text, file)?;
    if rows.is_empty() {
        bail!("no labels in {file}");
    }
    let fallback = labeler
        .map(str::to_string)
        .unwrap_or_else(|| format!("import:{file}"));
    let mut written = 0usize;
    for (i, r) in rows.iter().enumerate() {
        let mut body = json!({
            "subject": r.subject,
            "value": r.human_score,
            "labeler": r.labeler.clone().unwrap_or_else(|| fallback.clone()),
        });
        if let Some(p) = project {
            body["project_id"] = json!(p);
        }
        if let Some(v) = r.pass {
            body["pass"] = json!(v);
        }
        if let Some(v) = &r.rubric_id {
            body["rubric_id"] = json!(v);
        }
        if let Some(v) = &r.note {
            body["note"] = json!(v);
        }
        post(cli, http, "/v1/labels", &body)
            .with_context(|| format!("{file}: label {} ({})", i + 1, r.subject))?;
        written += 1;
    }
    println!("imported {written} label(s) from {file}");
    Ok(())
}

/// JSON array or JSONL, with the same blank/`//` tolerance `calibrate::parse_items` has — an
/// operator should not have to remember which of two files takes which shape.
fn parse_rows(text: &str, file: &str) -> Result<Vec<ImportRow>> {
    if text.trim_start().starts_with('[') {
        return serde_json::from_str(text)
            .with_context(|| format!("{file}: invalid JSON array of labels"));
    }
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("//") {
            continue;
        }
        out.push(
            serde_json::from_str::<ImportRow>(line).with_context(|| format!("{file}:{}", i + 1))?,
        );
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, Utc};

    fn item(id: &str, output: Option<&str>) -> DatasetItem {
        DatasetItem {
            id: id.into(),
            dataset_id: "ds".into(),
            input: format!("in-{id}"),
            output: output.map(str::to_string),
            expected: None,
            context: None,
            tags: vec![],
            source_event_id: None,
            anonymization: serde_json::Value::Null,
            input_hash: None,
        }
    }

    fn label(item_id: &str, value: f64, age_secs: i64) -> Label {
        Label {
            id: format!("l-{item_id}-{age_secs}"),
            project_id: "p".into(),
            subject: LabelSubject::DatasetItem(item_id.into()),
            rubric_id: None,
            value,
            pass: None,
            dimensions: vec![],
            labeler: "reviewer".into(),
            note: None,
            created_at: Utc::now() - Duration::seconds(age_secs),
        }
    }

    /// The rule that keeps a κ honest: an ungraded case is not a case the judge failed.
    #[test]
    fn an_unlabelled_case_is_skipped_rather_than_scored_zero() {
        let items = vec![item("a", Some("out-a")), item("b", Some("out-b"))];
        let (set, skipped) = pair(&items, &[label("a", 0.9, 0)]);
        assert_eq!(set.len(), 1);
        assert_eq!(set[0].human_score, 0.9);
        assert_eq!(skipped, 1, "the ungraded case is reported, not folded in");
    }

    /// A case with no stored output has nothing to re-judge — calibration is judge-only.
    #[test]
    fn a_case_with_no_output_is_not_a_calibration_case() {
        let (set, skipped) = pair(&[item("a", None)], &[label("a", 0.9, 0)]);
        assert!(set.is_empty());
        assert_eq!(skipped, 1);
    }

    /// Re-grading a case must actually re-grade it: the newest label wins.
    #[test]
    fn the_latest_grade_on_a_case_is_the_one_used() {
        let labels = vec![label("a", 0.2, 60), label("a", 0.95, 0)];
        let (set, _) = pair(&[item("a", Some("out"))], &labels);
        assert_eq!(set[0].human_score, 0.95);
    }

    #[test]
    fn an_import_file_may_be_jsonl_or_an_array_and_tolerates_comments() {
        let jsonl = "// a comment\n\n{\"subject\":\"event:e1\",\"human_score\":0.9}\n";
        assert_eq!(parse_rows(jsonl, "f").unwrap().len(), 1);
        let array = "[{\"subject\":\"event:e1\",\"human_score\":0.9}]";
        assert_eq!(parse_rows(array, "f").unwrap().len(), 1);
        // A malformed line names its own line number rather than failing anonymously.
        let bad = "{\"subject\":\"event:e1\",\"human_score\":0.9}\n{oops}\n";
        let err = parse_rows(bad, "golden.jsonl").unwrap_err().to_string();
        assert!(err.contains("golden.jsonl:2"), "{err}");
    }
}
