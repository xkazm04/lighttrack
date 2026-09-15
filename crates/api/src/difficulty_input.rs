//! The **write** half of the difficulty ladder (M27): a tier a caller *stated* is refused when this
//! ladder does not name it.
//!
//! Reading a stored row and accepting an operator's grade are two different jobs, and one function
//! was doing both. `core::dataset::de_difficulty` degrades an unrecognised rung to `None`, which is
//! right for a *read*: a corpus exported from a system with a four-rung ladder imports rather than
//! fails, and a case nobody can read back is a case that leaves the corpus. Applied to a *write* the
//! same tolerance is a defect — the string was typed seconds ago on purpose, so dropping it produces
//! a corpus that reads as graded and is not.
//!
//! A live model×effort benchmark on 2026-09-07 used a fourth tier, `expert`, on 8 of its 18 cases.
//! All 8 were accepted with a 200 and stored ungraded; the per-tier analysis then reported a phantom
//! bucket, noticed only because a script divided by zero. Meanwhile the *listing filter* had refused
//! the same spelling on the query string since M27 — same closed vocabulary, enforced on one surface
//! and not the other.
//!
//! So the tier stays raw JSON until the handler has had a chance to refuse it. The types below carry
//! the core type through `#[serde(flatten)]` rather than restating its fields, because a request
//! shape that duplicates a stored shape drifts the day someone adds a column to one of them.

use serde::Deserialize;
use serde_json::Value;

use lighttrack_core::{BenchmarkCase, DatasetItem, Difficulty};

/// The rungs, spelled as the wire spells them. Sourced from [`Difficulty::ALL`] — the constant
/// documents itself as "the list a CLI or API validates a spelling against", and a refusal that
/// hardcoded them would go stale the day the ladder grows a rung.
fn rungs() -> Vec<&'static str> {
    Difficulty::ALL.iter().map(|d| d.as_str()).collect()
}

/// Parse a tier a caller stated — a query parameter or a body field — refusing anything the ladder
/// does not name.
///
/// Case- and space-insensitive, because [`Difficulty::parse`] is: `"HARD"` and `" hard "` are the
/// same rung an operator typed at a shell, and normalising them is the same courtesy the listing
/// filter and `lt datasets add` already extend. An empty string is *not* one of them — it is a
/// stated tier with nothing in it, which is what `lt`'s own `tier("")` refuses.
pub(crate) fn parse_stated_tier(s: &str) -> Result<Difficulty, String> {
    Difficulty::parse(s)
        .ok_or_else(|| format!("unknown difficulty {s:?}: expected one of {:?}", rungs()))
}

/// The tier a write carries, read off the raw JSON the caller sent.
///
/// Absent and `null` are the same thing and both mean UNGRADED — `lt datasets add` already refuses
/// to send the key at all rather than send `null`, on the grounds that the two must never diverge.
/// Every other shape (a number, an object, an unknown string) is a caller stating something this
/// ladder cannot name, and is refused rather than dropped.
pub(crate) fn stated_tier(stated: Option<&Value>) -> Result<Option<Difficulty>, String> {
    match stated {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => parse_stated_tier(s).map(Some),
        Some(other) => Err(format!(
            "unknown difficulty {other}: expected one of {:?}",
            rungs()
        )),
    }
}

/// A [`DatasetItem`] as it arrives from an operator: everything typed as usual, except the tier,
/// which stays raw long enough to be refused.
#[derive(Deserialize)]
pub(crate) struct StatedItem {
    #[serde(default)]
    difficulty: Option<Value>,
    #[serde(flatten)]
    item: DatasetItem,
}

impl StatedItem {
    pub(crate) fn into_item(self) -> Result<DatasetItem, String> {
        let mut item = self.item;
        // `flatten` routed `difficulty` to the outer field, so the inner one is the serde default
        // (`None`) and this assignment is the only thing that grades the case.
        item.difficulty = stated_tier(self.difficulty.as_ref())?;
        Ok(item)
    }
}

/// A [`BenchmarkCase`] as it arrives inline in a `create_benchmark` body. Same shape, same reason.
#[derive(Deserialize)]
pub(crate) struct StatedCase {
    #[serde(default)]
    difficulty: Option<Value>,
    #[serde(flatten)]
    case: BenchmarkCase,
}

impl StatedCase {
    pub(crate) fn into_case(self) -> Result<BenchmarkCase, String> {
        let mut case = self.case;
        case.difficulty = stated_tier(self.difficulty.as_ref())?;
        Ok(case)
    }
}

#[cfg(test)]
mod unit {
    use super::*;
    use serde_json::json;

    /// **Both halves, pinned together.** They are deliberately different, and the difference is the
    /// whole feature: collapsing them into one behaviour — in either direction — must fail here.
    ///
    /// Strict on the way in, because the operator typed the string on purpose. Tolerant on the way
    /// out, because a stored row with a rung this build does not know is still a case, and refusing
    /// to read it would drop it from the corpus entirely.
    #[test]
    fn a_stated_tier_is_refused_where_a_stored_one_degrades() {
        // Write: refused, and the refusal names the ladder.
        let err =
            stated_tier(Some(&json!("expert"))).expect_err("a fourth rung is not stated here");
        assert!(err.contains("expert"), "{err}");
        for rung in ["easy", "medium", "hard"] {
            assert!(err.contains(rung), "the refusal names {rung}: {err}");
        }

        // Read: the very same spelling, arriving from storage through serde, degrades to ungraded
        // and keeps the case. This is `de_difficulty`, and it must stay this way.
        let stored: DatasetItem =
            serde_json::from_value(json!({ "input": "2+2", "difficulty": "expert" }))
                .expect("an unreadable grade must not take the case with it");
        assert_eq!(stored.difficulty, None);
        assert_eq!(stored.input, "2+2");

        // And the same for a benchmark's inline cases, which is where the live incident landed:
        // the stored `dataset` column is JSON read back through this same tolerant path.
        let stored: BenchmarkCase =
            serde_json::from_value(json!({ "input": "i", "difficulty": "expert" }))
                .expect("case survives");
        assert_eq!(stored.difficulty, None);
    }

    /// The outcome space of a stated tier, one row per shape a caller can send.
    #[test]
    fn every_shape_a_caller_can_state_has_a_settled_answer() {
        // Absent and null are the same UNGRADED, and neither is an error.
        assert_eq!(stated_tier(None).expect("absent"), None);
        assert_eq!(stated_tier(Some(&Value::Null)).expect("null"), None);

        // The three rungs, and the spellings a shell produces.
        assert_eq!(
            stated_tier(Some(&json!("easy"))).expect("easy"),
            Some(Difficulty::Easy)
        );
        assert_eq!(
            stated_tier(Some(&json!("HARD"))).expect("case-insensitive, as the filter is"),
            Some(Difficulty::Hard)
        );
        assert_eq!(
            stated_tier(Some(&json!("  medium "))).expect("trimmed"),
            Some(Difficulty::Medium)
        );

        // Everything else is a refusal, including the empty string: a stated tier with nothing in
        // it is not the same as not stating one.
        for bad in [
            json!("expert"),
            json!(""),
            json!("   "),
            json!(3),
            json!(0.5),
            json!({ "tier": "hard" }),
            json!(["hard"]),
            json!(true),
        ] {
            let err = stated_tier(Some(&bad)).expect_err("refused: {bad}");
            assert!(
                err.starts_with("unknown difficulty"),
                "{bad} → {err}: the message shape the listing filter returns"
            );
        }
    }

    /// The request shapes carry every other field through untouched — the reason they `flatten` the
    /// core type instead of restating it.
    #[test]
    fn the_rest_of_the_body_survives_the_detour_through_raw_json() {
        let stated: StatedItem = serde_json::from_value(json!({
            "input": "2+2", "expected": "4", "output": "four", "context": "arithmetic",
            "tags": ["golden"], "difficulty": "Hard", "source_event_id": "e1",
            "anonymization": { "method": "regex", "redactions": 2 },
        }))
        .expect("body");
        let item = stated.into_item().expect("graded");
        assert_eq!(item.difficulty, Some(Difficulty::Hard));
        assert_eq!(item.expected.as_deref(), Some("4"));
        assert_eq!(item.output.as_deref(), Some("four"));
        assert_eq!(item.context.as_deref(), Some("arithmetic"));
        assert_eq!(item.tags, ["golden"]);
        assert_eq!(item.source_event_id.as_deref(), Some("e1"));
        assert_eq!(item.anonymization["redactions"], 2);
        // Normalised on the way in, so the corpus holds one spelling per rung.
        assert_eq!(
            serde_json::to_value(&item).expect("ser")["difficulty"],
            json!("hard")
        );

        let stated: StatedCase =
            serde_json::from_value(json!({ "input": "i", "expected": "e", "output": "o" }))
                .expect("case");
        let case = stated.into_case().expect("ungraded is fine");
        assert_eq!(case.difficulty, None);
        assert_eq!(case.expected.as_deref(), Some("e"));
        assert_eq!(case.output.as_deref(), Some("o"));
    }
}

#[cfg(test)]
#[path = "tests_difficulty_write.rs"]
mod tests;
