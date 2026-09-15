//! Reasoning effort — the one vocabulary for *how hard the model should think*.
//!
//! It used to exist only as a string suffix on a model spec (`opus@xhigh`), parsed inside the
//! `claude -p` argv builder. That put the knowledge in exactly one of four generation paths: with
//! `ANTHROPIC_API_KEY` set, the default judge spec `opus@xhigh` went to the bare Messages API as a
//! *model id* — a 404 on every judge call, on the default configuration. So the vocabulary lives
//! here in `core`, where the provider adapters, the CLI invocation seam and a stored `BenchTarget`
//! can all name the same closed set.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// How hard a model is asked to think. A closed set on purpose: an effort level that no adapter can
/// map is a run that silently measured something else, so an unrecognised word must fail to parse
/// rather than travel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Effort {
    /// Every level, weakest first. Ordering is meaningful: adapters whose provider offers fewer
    /// rungs than we name map the top of ours onto the top of theirs.
    pub const ALL: [Effort; 5] = [
        Effort::Low,
        Effort::Medium,
        Effort::High,
        Effort::XHigh,
        Effort::Max,
    ];

    /// The accepted spellings, for error messages that tell the operator what to write instead.
    pub const EXPECTED: &'static str = "low|medium|high|xhigh|max";

    /// Parse one level. `None` for anything else — never a nearest-match guess.
    pub fn parse(s: &str) -> Option<Effort> {
        match s {
            "low" => Some(Effort::Low),
            "medium" => Some(Effort::Medium),
            "high" => Some(Effort::High),
            "xhigh" => Some(Effort::XHigh),
            "max" => Some(Effort::Max),
            _ => None,
        }
    }

    /// The wire word — the same spelling `claude -p --effort` takes and the same one serde writes.
    pub fn as_str(&self) -> &'static str {
        match self {
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl fmt::Display for Effort {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Effort {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Effort::parse(s).ok_or_else(|| {
            format!(
                "unknown effort '{s}' (expected {expected})",
                expected = Effort::EXPECTED
            )
        })
    }
}

/// Split a trailing `@<effort>` off a model spec: `"opus@xhigh"` → `("opus", Some(Effort::XHigh))`.
///
/// Only a *known* level is split. A model id that happens to contain an `@` (`"weird@thing"`) is
/// left whole, because guessing there would silently rewrite the model an operator asked for.
pub fn split_effort(spec: &str) -> (&str, Option<Effort>) {
    if let Some((model, suffix)) = spec.rsplit_once('@') {
        if let Some(effort) = Effort::parse(suffix) {
            return (model, Some(effort));
        }
    }
    (spec, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_effort_only_splits_known_levels() {
        assert_eq!(split_effort("opus@xhigh"), ("opus", Some(Effort::XHigh)));
        assert_eq!(split_effort("sonnet"), ("sonnet", None));
        // The rule that keeps a model id intact: an unknown suffix is part of the model.
        assert_eq!(split_effort("weird@thing"), ("weird@thing", None));
        assert_eq!(split_effort("gpt-5@high"), ("gpt-5", Some(Effort::High)));
        assert_eq!(
            split_effort("gemini-2.5-pro@low"),
            ("gemini-2.5-pro", Some(Effort::Low))
        );
    }

    #[test]
    fn every_level_round_trips_through_its_wire_word() {
        for e in Effort::ALL {
            assert_eq!(Effort::parse(e.as_str()), Some(e));
            assert_eq!(
                serde_json::to_value(e).unwrap(),
                serde_json::json!(e.as_str()),
                "serde must write the same word the CLI flag takes"
            );
            assert_eq!(
                serde_json::from_value::<Effort>(serde_json::json!(e.as_str())).unwrap(),
                e
            );
        }
        assert!(Effort::parse("turbo").is_none());
        assert!("turbo".parse::<Effort>().is_err());
    }

    #[test]
    fn levels_order_weakest_first() {
        assert!(Effort::Low < Effort::Medium);
        assert!(Effort::XHigh < Effort::Max);
    }
}
