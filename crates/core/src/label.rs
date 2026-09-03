//! Human verdicts as data (M11).
//!
//! Before this module a human judgement had no home in the data model: a calibration set was a
//! JSONL file on whoever ran the runner's disk, and a "golden" dataset item carried an `expected`
//! string but never *who said so*. That makes the one input the whole judge-trust argument rests on
//! the least durable thing in the system — it cannot be listed, cannot be re-used by a second
//! calibration, and cannot be compared against the judge's own verdict on the same subject.
//!
//! A [`Label`] is one person's (or one process's) opinion about one subject, stored beside the
//! machine verdicts it is meant to check. It is deliberately *not* a [`crate::Score`]: a score is
//! something a judge produced and is budgeted, costed and alerted on; a label is ground truth and
//! has none of those. Conflating them is how κ history ended up encoded in a reserved rubric name.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::score::ScoreDim;

/// What a [`Label`] is about.
///
/// Three subjects rather than one free-text `(kind, id)` pair, because each one is a different
/// question and the store indexes them together: an event is production traffic a human graded, a
/// dataset item is a curated golden case, and a score is a human *reviewing the judge* — the
/// disagreement signal `GET /v1/scores?needs_review=1` reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(tag = "type", content = "id", rename_all = "snake_case")]
pub enum LabelSubject {
    Event(String),
    DatasetItem(String),
    Score(String),
}

impl LabelSubject {
    /// The stored discriminator column.
    pub fn kind(&self) -> &'static str {
        match self {
            LabelSubject::Event(_) => "event",
            LabelSubject::DatasetItem(_) => "dataset_item",
            LabelSubject::Score(_) => "score",
        }
    }

    /// The stored id column.
    pub fn id(&self) -> &str {
        match self {
            LabelSubject::Event(id) | LabelSubject::DatasetItem(id) | LabelSubject::Score(id) => id,
        }
    }

    /// Rebuild from the two stored columns. `None` for a discriminator this binary does not know —
    /// the caller decides whether that is a skipped row or an error, rather than this silently
    /// misfiling a newer writer's subject as an event.
    pub fn from_parts(kind: &str, id: &str) -> Option<Self> {
        match kind {
            "event" => Some(LabelSubject::Event(id.to_string())),
            "dataset_item" => Some(LabelSubject::DatasetItem(id.to_string())),
            "score" => Some(LabelSubject::Score(id.to_string())),
            _ => None,
        }
    }

    /// Parse the `subject=` query form, `"<kind>:<id>"`.
    pub fn parse(s: &str) -> Option<Self> {
        let (kind, id) = s.split_once(':')?;
        if id.is_empty() {
            return None;
        }
        LabelSubject::from_parts(kind.trim(), id.trim())
    }

    /// The `subject=` query form — the inverse of [`LabelSubject::parse`].
    pub fn to_query(&self) -> String {
        format!("{}:{}", self.kind(), self.id())
    }
}

/// One human verdict.
///
/// `value` is normalized to 0..1 exactly as [`crate::Score::value`] is over `max`, so a label and a
/// judge verdict on the same subject are directly comparable — which is the entire point, and the
/// reason there is no `max` here to get out of step with the judge's.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Label {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Defaulted so a keyed poster can omit it (the API derives it from the API key).
    #[serde(default)]
    pub project_id: String,
    pub subject: LabelSubject,
    /// The rubric this opinion was formed under, when there was one. A label with no rubric is a
    /// general quality opinion and calibrates any rubric; one with a rubric calibrates only that
    /// rubric, because "good" means a different thing under a different set of criteria.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
    /// Overall quality in 0..1.
    pub value: f64,
    /// The human's pass/fail call, when they made one explicitly. `None` means "derive it from
    /// `value` against the rubric's threshold" — kept distinct so a labeler who deliberately passed
    /// a 0.4 case is not overruled by a threshold.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<bool>,
    /// Per-dimension human scores, in the same shape the judge reports them, so a per-dimension
    /// disagreement is a subtraction rather than a schema translation.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<ScoreDim>,
    /// Who said so. Free text (a person, a team, `"import:<file>"`) — the provenance that makes a
    /// calibration result auditable at all, which is why it is required rather than optional.
    pub labeler: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

/// Bounds applied by the API on insert, so one posted label cannot balloon the table.
pub const MAX_LABEL_DIMENSIONS: usize = 32;
/// Max chars kept of `note` / `labeler`.
pub const MAX_LABEL_TEXT: usize = 2_000;

impl Label {
    /// Clamp `value` into 0..1 and enforce the storage bounds. A label outside the range is a
    /// caller mistake we normalize rather than reject, because a rejected label is a human opinion
    /// thrown away — but an unbounded `dimensions` vector is storage abuse and is truncated.
    pub fn capped(mut self) -> Self {
        self.value = self.value.clamp(0.0, 1.0);
        self.dimensions.truncate(MAX_LABEL_DIMENSIONS);
        self.labeler = cap(&self.labeler);
        self.note = self.note.as_deref().map(cap);
        self
    }

    /// The human's pass/fail call, falling back to `value >= threshold`.
    pub fn passed(&self, threshold: f64) -> bool {
        self.pass.unwrap_or(self.value >= threshold)
    }
}

fn cap(s: &str) -> String {
    if s.chars().count() <= MAX_LABEL_TEXT {
        return s.to_string();
    }
    s.chars().take(MAX_LABEL_TEXT).collect()
}

/// How `GET /v1/labels` narrows the ledger.
#[derive(Debug, Clone, Default)]
pub struct LabelFilter {
    pub project: Option<String>,
    pub subject: Option<LabelSubject>,
    pub rubric_id: Option<String>,
    /// `0` means [`LabelFilter::DEFAULT_LIMIT`].
    pub limit: usize,
    /// Opaque keyset cursor from a previous page.
    pub cursor: Option<String>,
}

impl LabelFilter {
    pub const DEFAULT_LIMIT: usize = 100;
    pub const MAX_LIMIT: usize = 1000;

    pub fn effective_limit(&self) -> usize {
        match self.limit {
            0 => Self::DEFAULT_LIMIT,
            n => n.min(Self::MAX_LIMIT),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subject_round_trips_through_its_stored_columns_and_its_query_form() {
        for s in [
            LabelSubject::Event("e1".into()),
            LabelSubject::DatasetItem("d1".into()),
            LabelSubject::Score("s1".into()),
        ] {
            assert_eq!(
                LabelSubject::from_parts(s.kind(), s.id()).as_ref(),
                Some(&s),
                "stored columns must rebuild {s:?}"
            );
            assert_eq!(LabelSubject::parse(&s.to_query()).as_ref(), Some(&s));
        }
    }

    /// A discriminator this binary does not know must not decode as *something else*: misfiling a
    /// newer writer's subject as an event would attach a human verdict to the wrong row.
    #[test]
    fn an_unknown_subject_kind_decodes_to_nothing_rather_than_to_an_event() {
        assert!(LabelSubject::from_parts("trace", "t1").is_none());
        assert!(LabelSubject::parse("trace:t1").is_none());
        assert!(LabelSubject::parse("event:").is_none());
        assert!(LabelSubject::parse("nocolon").is_none());
    }

    #[test]
    fn capping_clamps_the_value_and_bounds_the_text() {
        let l = Label {
            id: "l".into(),
            project_id: "p".into(),
            subject: LabelSubject::Event("e".into()),
            rubric_id: None,
            value: 4.5,
            pass: None,
            dimensions: vec![],
            labeler: "x".repeat(MAX_LABEL_TEXT + 50),
            note: None,
            created_at: Utc::now(),
        }
        .capped();
        assert_eq!(l.value, 1.0);
        assert_eq!(l.labeler.chars().count(), MAX_LABEL_TEXT);
        assert_eq!(Label { value: -2.0, ..l }.capped().value, 0.0);
    }

    /// An explicit human pass/fail is never overruled by the threshold — the whole reason `pass` is
    /// an `Option` rather than derived.
    #[test]
    fn an_explicit_pass_beats_the_threshold() {
        let base = Label {
            id: "l".into(),
            project_id: "p".into(),
            subject: LabelSubject::Event("e".into()),
            rubric_id: None,
            value: 0.4,
            pass: None,
            dimensions: vec![],
            labeler: "me".into(),
            note: None,
            created_at: Utc::now(),
        };
        assert!(!base.passed(0.7));
        assert!(Label {
            pass: Some(true),
            ..base.clone()
        }
        .passed(0.7));
        assert!(!Label {
            pass: Some(false),
            value: 0.99,
            ..base
        }
        .passed(0.7));
    }
}
