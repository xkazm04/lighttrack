//! The backend-independent half of `Store::import_dataset_items` (M24).
//!
//! Selection is SQL and belongs to each backend; everything *after* selection is identical work no
//! backend should re-derive — and one of those steps is load-bearing for safety. A case mined out
//! of production traffic is production text: `lt-runner dataset build` has always run it through
//! [`lighttrack_anon::scrub`] before it became a stored case, and a server-side import that skipped
//! that would be a strictly easier way to copy live PII into an eval corpus than the path it
//! replaces. So the scrub happens here, on the way in, for every backend — not in one handler that
//! another caller can bypass.
//!
//! The regex pass only. The optional LLM scrub `dataset build` can add stays in the runner: it is a
//! paid model call, and a store method is not a place to make one.

use std::collections::HashSet;

use serde_json::json;

use lighttrack_core::{input_fingerprint, new_id, DatasetItem, SamplingStrategy};

/// One row a backend's selection query matched, before it becomes a case.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub event_id: String,
    pub input: String,
    pub output: Option<String>,
    pub tags: Vec<String>,
}

/// Scrub a candidate and stamp it as a dataset item: same anonymization audit shape
/// (`{"method":"regex","redactions":n}`) `dataset build` writes, so an imported case and a
/// runner-built one are indistinguishable downstream.
pub fn to_item(dataset_id: &str, c: &Candidate) -> DatasetItem {
    let scrubbed_in = lighttrack_anon::scrub(&c.input);
    let (output, r_out) = match c.output.as_deref() {
        Some(o) => {
            let s = lighttrack_anon::scrub(o);
            (Some(s.text), s.redactions)
        }
        None => (None, 0),
    };
    let redactions = scrubbed_in.redactions + r_out;
    DatasetItem {
        id: new_id(),
        dataset_id: dataset_id.to_string(),
        // The fingerprint is of the SCRUBBED text, so two cases that differ only in the PII the
        // scrubber replaced collapse — which is the common shape of a duplicate in sampled traffic
        // (the same prompt template, a different customer name).
        input_hash: Some(input_fingerprint(&scrubbed_in.text)),
        input: scrubbed_in.text,
        output,
        expected: None,
        context: None,
        tags: c.tags.clone(),
        source_event_id: Some(c.event_id.clone()),
        anonymization: json!({ "method": "regex", "redactions": redactions }),
    }
}

/// Turn matched candidates into the items to insert: scrub, then drop near-duplicates when the spec
/// asked for it.
///
/// `existing` is the target set's already-stored fingerprints. A candidate whose fingerprint is
/// already there — or that duplicates an earlier candidate in this same batch — is dropped, which is
/// the case a single-pass `INSERT … WHERE NOT EXISTS` would miss.
pub fn prepare(
    dataset_id: &str,
    candidates: &[Candidate],
    dedupe: bool,
    existing: &HashSet<String>,
) -> Vec<DatasetItem> {
    let mut seen = existing.clone();
    let mut out = Vec::with_capacity(candidates.len());
    for c in candidates {
        let item = to_item(dataset_id, c);
        if dedupe {
            let Some(h) = item.input_hash.clone() else {
                out.push(item);
                continue;
            };
            if !seen.insert(h) {
                continue;
            }
        }
        out.push(item);
    }
    out
}

/// The per-group cap a [`SamplingStrategy::Stratified`] import applies, given how many
/// `(model, status)` groups the filter matched and how many cases were asked for.
///
/// At least one per group: the entire reason to stratify is that a group with three calls all week
/// is the one a `recent` sample never shows, and a quota that rounded it to zero would reproduce
/// exactly the bias being corrected. The result can therefore exceed `n` when there are more groups
/// than cases — deliberately, and the caller reports what it actually wrote.
pub fn stratum_quota(n: usize, groups: usize) -> usize {
    if groups == 0 {
        return 0;
    }
    n.div_ceil(groups).max(1)
}

/// `true` when the strategy narrows to failures — the shared reading of `errors-only` both backends
/// apply to their own `status` / `pass` column.
pub fn is_errors_only(s: SamplingStrategy) -> bool {
    s == SamplingStrategy::Errors
}

/// The prompt text inside a stored `events.input` / `events.output` column.
///
/// Both are JSON, and the overwhelmingly common shape is a bare JSON string — storing that verbatim
/// would put a case into the corpus wrapped in quotes and escapes, which a judge then reads as part
/// of the prompt. Anything structured is kept as-is: re-rendering an object would lose fields, and
/// the runner's own builder makes the same call.
pub fn text_of(raw: &str) -> String {
    match serde_json::from_str::<serde_json::Value>(raw) {
        Ok(serde_json::Value::String(s)) => s,
        _ => raw.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(id: &str, input: &str) -> Candidate {
        Candidate {
            event_id: id.to_string(),
            input: input.to_string(),
            output: Some("ok".to_string()),
            tags: vec!["t".to_string()],
        }
    }

    /// The property the whole module exists for: production text does not reach a dataset item
    /// unscrubbed, and the audit says so.
    #[test]
    fn an_imported_case_is_scrubbed_and_stamped() {
        let item = to_item("ds", &candidate("e1", "mail me at bob@example.com"));
        assert!(
            !item.input.contains("bob@example.com"),
            "the address must not survive into the corpus"
        );
        assert_eq!(item.anonymization["method"], "regex");
        assert_eq!(item.anonymization["redactions"], 1);
        assert_eq!(item.source_event_id.as_deref(), Some("e1"));
        assert!(item.input_hash.is_some(), "dedupe needs the fingerprint");
    }

    /// Two cases differing only in the PII the scrubber replaced are the same case.
    #[test]
    fn the_fingerprint_is_taken_after_the_scrub() {
        let a = to_item("ds", &candidate("e1", "refund order for bob@example.com"));
        let b = to_item("ds", &candidate("e2", "refund order for eve@example.com"));
        assert_eq!(a.input_hash, b.input_hash);
    }

    #[test]
    fn dedupe_collapses_within_the_batch_and_against_the_existing_set() {
        let batch = vec![
            candidate("e1", "summarise this"),
            candidate("e2", "Summarise   THIS"),
            candidate("e3", "something else"),
        ];
        let none = HashSet::new();
        assert_eq!(
            prepare("ds", &batch, false, &none).len(),
            3,
            "without dedupe every matched row is written"
        );
        let kept = prepare("ds", &batch, true, &none);
        assert_eq!(kept.len(), 2, "the in-batch duplicate collapses");

        let existing: HashSet<String> = [kept[0].input_hash.clone().expect("hash")]
            .into_iter()
            .collect();
        assert_eq!(
            prepare("ds", &batch, true, &existing).len(),
            1,
            "a case already in the target set is not re-imported"
        );
    }

    /// A rare group must never be quota'd to zero — that is the bias stratification corrects.
    #[test]
    fn every_stratum_gets_at_least_one() {
        assert_eq!(stratum_quota(10, 4), 3);
        assert_eq!(stratum_quota(10, 10), 1);
        assert_eq!(stratum_quota(3, 10), 1);
        assert_eq!(stratum_quota(10, 0), 0);
    }
}
