use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A versioned, reusable evaluation dataset. Freezing makes the dataset immutable, which fixes
/// one half of run comparability — the input. It does not fix the other half. Where the cases came
/// from outside this project (`source: import`), the models under test accumulate exposure to that
/// material as it circulates, so two runs of the same frozen dataset months apart are not
/// interchangeable: a rise can be the target or judge having *seen* the cases rather than having
/// improved. Cases sampled from this project's own traffic (`source: events:recent`) cannot be
/// exposed that way, which is the strongest reason to prefer them over an import.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct Dataset {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub frozen: bool,
    /// Provenance, e.g. `events:recent`, `manual`, `import`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The dataset this one was forked from (M24), when it was forked rather than created.
    ///
    /// The link is what makes `version` mean anything: without it a v2 is just another row that
    /// happens to share a name, and "is this run's corpus the same corpus as that run's" has no
    /// answer beyond string equality. Absent on every dataset created directly.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_version() -> u32 {
    1
}

/// How hard one case is meant to be, on a fixed ladder that is **totally ordered**.
///
/// The ordering is the whole reason this is not another entry in `DatasetItem::tags`. A free-text
/// tag can group cases and cannot rank them, so it cannot answer the question the benchmark layer
/// exists for: *this model handles the easy and medium variants of my use case; only the hard ones
/// need the expensive config.* That is a routing decision, and it needs `<`.
///
/// The second thing a tier buys is **discrimination**. A rung every target passes carries no
/// information, and neither does one every target fails; a corpus that cannot see which of its
/// rungs is doing the separating is spending money to learn nothing. Both readings need the rungs
/// to be comparable, and comparable is a property of a closed ordered set, not of a string.
///
/// Three rungs, not four. A fourth is easy to add and hard to *use*: an operator who cannot tell
/// two adjacent rungs apart consistently produces a grading that is noise, and noise on the axis
/// the routing decision reads is worse than a coarser axis. Add one when a corpus demonstrably
/// saturates the top rung, not before.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum Difficulty {
    Easy,
    Medium,
    Hard,
}

impl Difficulty {
    /// Every rung, in ascending order — the list a CLI or API validates a spelling against.
    pub const ALL: &'static [Difficulty] =
        &[Difficulty::Easy, Difficulty::Medium, Difficulty::Hard];

    pub fn as_str(self) -> &'static str {
        match self {
            Difficulty::Easy => "easy",
            Difficulty::Medium => "medium",
            Difficulty::Hard => "hard",
        }
    }

    /// One wire/CLI spelling, or `None` for anything this ladder does not name.
    ///
    /// Case- and space-insensitive, because the spelling arrives from a shell argument and a query
    /// string as often as from a serializer.
    pub fn parse(s: &str) -> Option<Difficulty> {
        match s.trim().to_ascii_lowercase().as_str() {
            "easy" => Some(Difficulty::Easy),
            "medium" => Some(Difficulty::Medium),
            "hard" => Some(Difficulty::Hard),
            _ => None,
        }
    }
}

/// Deserialize a `difficulty` field, degrading an unrecognised value to `None` (ungraded) rather
/// than failing the whole record.
///
/// [`crate::UseCaseKind`] takes the same position for the same reason — an unknown `kind` degrades
/// to `Other` rather than refusing the registration, because a call site nobody can register just
/// goes unmonitored. A case nobody can read back is worse: it is a case that leaves the corpus.
///
/// The **departure** is where the unknown value lands. `UseCaseKind` has a catch-all rung;
/// `Difficulty` must never grow one, because a catch-all would have to sit somewhere on a total
/// order and every placement is a claim nobody made. So the degrade target is the absence the
/// field already models. That is not a coercion to the middle: `None` means *ungraded*, never
/// *medium*, everywhere in this codebase — an absent grade is not a middling grade.
///
/// **This is the READ half, and making it strict would be a defect.** Accepting an operator's grade
/// is the other job, and it is strict: a tier typed seconds ago and silently dropped produces a
/// corpus that reads as graded and is not (an 18-case benchmark shipped 8 of them on a fourth rung
/// that way). That refusal lives at the API boundary — `lighttrack-api`'s `difficulty_input` — so
/// this function can stay tolerant for the case it exists for: reading back a row a *different*
/// build wrote. A test there pins both halves together.
pub(crate) fn de_difficulty<'de, D>(d: D) -> Result<Option<Difficulty>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(match Option::<Value>::deserialize(d)? {
        Some(Value::String(s)) => Difficulty::parse(&s),
        _ => None,
    })
}

/// One case in a dataset. `output` is a captured/candidate response; `expected` is a golden reference.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
pub struct DatasetItem {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub dataset_id: String,
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// How hard this case is meant to be (M27). `None` is **ungraded**, never "medium": nobody
    /// graded it, and imputing a middle rung would put unexamined cases into the tier the routing
    /// decision reads most closely. A rung an operator *states* on a write is refused when this
    /// ladder does not name it; one read back from storage degrades — see [`de_difficulty`].
    #[serde(
        default,
        deserialize_with = "de_difficulty",
        skip_serializing_if = "Option::is_none"
    )]
    pub difficulty: Option<Difficulty>,
    /// The real event this item was sampled from, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_event_id: Option<String>,
    /// Anonymization audit, e.g. `{"method":"regex+llm","redactions":3}`.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub anonymization: Value,
    /// Fingerprint of the normalised `input` (M24), stored so near-duplicate collapse is an index
    /// lookup instead of a scan of every case's text. Absent on items written before the column
    /// existed and on backends that do not compute it — which is why dedupe treats a `NULL` as "not
    /// known to be a duplicate" rather than as a match.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_hash: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The property a tag could never have, and the reason this type is an enum.
    #[test]
    fn the_ladder_is_ordered() {
        assert!(Difficulty::Easy < Difficulty::Medium);
        assert!(Difficulty::Medium < Difficulty::Hard);
        assert!(Difficulty::Easy < Difficulty::Hard);
        let mut rungs = vec![Difficulty::Hard, Difficulty::Easy, Difficulty::Medium];
        rungs.sort();
        assert_eq!(rungs, Difficulty::ALL.to_vec());
        // Ungraded sorts below every rung, which is what `Option`'s own ordering already says —
        // pinned here because a report that groups by tier depends on it.
        assert!(None < Some(Difficulty::Easy));
    }

    #[test]
    fn a_spelling_round_trips_through_the_wire_and_the_cli() {
        for d in Difficulty::ALL {
            assert_eq!(Difficulty::parse(d.as_str()), Some(*d));
            assert_eq!(serde_json::to_value(d).expect("ser"), json!(d.as_str()));
        }
        assert_eq!(Difficulty::parse("  HARD "), Some(Difficulty::Hard));
        assert_eq!(Difficulty::parse("trivial"), None);
    }

    /// An unknown rung must cost the field, never the case.
    #[test]
    fn an_unknown_wire_value_degrades_to_ungraded_rather_than_failing_the_item() {
        let item: DatasetItem =
            serde_json::from_value(json!({ "input": "2+2", "difficulty": "impossible" }))
                .expect("an unreadable grade must not take the case with it");
        assert_eq!(item.difficulty, None);
        assert_eq!(item.input, "2+2");

        // And nothing degrades to a middle rung: `None` is ungraded, not medium.
        for wire in [json!(null), json!(3), json!({ "tier": "hard" })] {
            let item: DatasetItem =
                serde_json::from_value(json!({ "input": "i", "difficulty": wire })).expect("item");
            assert_eq!(item.difficulty, None);
        }
    }

    /// A case stored before M27 has no `difficulty` key, and must still have none after a
    /// read-modify-write through this type.
    #[test]
    fn a_pre_m27_item_round_trips_byte_identically() {
        let stored = r#"{"id":"i1","dataset_id":"d1","input":"2+2","expected":"4","tags":["t"]}"#;
        let item: DatasetItem = serde_json::from_str(stored).expect("parse");
        assert_eq!(item.difficulty, None);
        assert_eq!(serde_json::to_string(&item).expect("ser"), stored);
    }

    #[test]
    fn a_graded_item_serializes_the_rung_and_reads_it_back() {
        let item: DatasetItem =
            serde_json::from_value(json!({ "input": "i", "difficulty": "hard" })).expect("item");
        assert_eq!(item.difficulty, Some(Difficulty::Hard));
        let wire = serde_json::to_value(&item).expect("ser");
        assert_eq!(wire["difficulty"], json!("hard"));
    }
}
