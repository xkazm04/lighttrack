use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// How a dimension is evaluated. `Llm` (the default) asks the judge model; every other kind is a
/// mechanical check the engine runs locally at zero tokens and zero cost, scored into the same
/// weighting / floor / aggregation pipeline. Additive and defaulted: a rubric written before kinds
/// existed deserializes as all-`Llm` and re-serializes byte-identically.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DimensionKind {
    /// Scored by the judge model against the dimension's description and anchors.
    #[default]
    Llm,
    /// The output must equal the target exactly (after the configured trim/case handling).
    Exact,
    /// The output must match `check.pattern` (unanchored regex).
    Regex,
    /// The output's number must be within `check.tolerance` of the target.
    Numeric,
    /// The output must parse as JSON (and, with `check.expect`, carry that value at `check.path`).
    JsonValid,
    /// The output must contain the target as a substring.
    Contains,
}

impl DimensionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            DimensionKind::Llm => "llm",
            DimensionKind::Exact => "exact",
            DimensionKind::Regex => "regex",
            DimensionKind::Numeric => "numeric",
            DimensionKind::JsonValid => "json_valid",
            DimensionKind::Contains => "contains",
        }
    }

    /// True for the LLM-judged default — the only kind that costs a model call.
    pub fn is_llm(&self) -> bool {
        matches!(self, DimensionKind::Llm)
    }
}

/// Per-kind configuration for a deterministic dimension. Every field is optional, so one struct
/// serves all kinds and an `llm` dimension serializes without it at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DimensionCheck {
    /// The literal target for `exact` / `contains` / `numeric` (and, optionally, `json_valid`).
    /// Defaults to the case's `expected` reference answer when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expect: Option<String>,
    /// `regex` only: the pattern the output must match somewhere (unanchored).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// `numeric` only: absolute tolerance around the target. Unset = exact equality.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tolerance: Option<f64>,
    /// JSON Pointer (RFC 6901, e.g. `/data/answer`) selecting the part of a JSON output to check.
    /// Unset = check the whole output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Compare case-sensitively (default: false — mechanical checks shouldn't fail on casing).
    #[serde(default)]
    pub case_sensitive: bool,
    /// Trim surrounding whitespace from both sides before comparing (default: true).
    #[serde(default = "default_true")]
    pub trim: bool,
}

impl Default for DimensionCheck {
    fn default() -> Self {
        DimensionCheck {
            expect: None,
            pattern: None,
            tolerance: None,
            path: None,
            case_sensitive: false,
            trim: true,
        }
    }
}

impl DimensionCheck {
    /// Nothing configured — so an `llm` dimension can omit the whole object when serializing.
    pub fn is_default(&self) -> bool {
        *self == DimensionCheck::default()
    }
}

fn default_true() -> bool {
    true
}

/// One scored dimension of a rubric (e.g. correctness, completeness, faithfulness, concision).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RubricDimension {
    /// Stable key used in the judge's JSON output (must be a valid identifier-ish string).
    pub key: String,
    /// What this dimension measures.
    pub description: String,
    /// Relative weight in the overall score.
    #[serde(default = "default_weight")]
    pub weight: f64,
    /// Anchored level descriptions, e.g. ["1.0 = fully correct & verifiable", "0.5 = minor error", "0 = wrong"].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub anchors: Vec<String>,
    /// Gating floor: if this dimension scores below it, the case fails regardless of the overall.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// How this dimension is evaluated (default `llm`). Deterministic kinds are checked locally and
    /// are never narrated to the judge model, so they cannot be double-counted.
    #[serde(default, skip_serializing_if = "DimensionKind::is_llm")]
    pub kind: DimensionKind,
    /// Configuration for a deterministic `kind`. Ignored when `kind` is `llm`.
    #[serde(default, skip_serializing_if = "DimensionCheck::is_default")]
    pub check: DimensionCheck,
}

fn default_weight() -> f64 {
    1.0
}

/// A weighted, anchored rubric — the judge's scoring contract (see docs/BENCHMARK_FRAMEWORK.md §3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Rubric {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    pub dimensions: Vec<RubricDimension>,
    /// Overall pass threshold (weighted score, 0–1).
    #[serde(default = "default_threshold")]
    pub threshold: f64,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

fn default_threshold() -> f64 {
    0.7
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A rubric written before dimension kinds existed must deserialize as all-`llm` and serialize
    /// back to exactly the same JSON — the new fields are additive, defaulted, and invisible.
    #[test]
    fn legacy_rubric_round_trips_byte_identically() {
        let legacy = json!({
            "id": "r1", "project_id": "p1", "name": "quality",
            "dimensions": [
                { "key": "correctness", "description": "right?", "weight": 2.0,
                  "anchors": ["1.0 = yes"], "floor": 0.5 },
                { "key": "concision", "description": "short?", "weight": 1.0 }
            ],
            "threshold": 0.7,
            "created_at": "2026-01-01T00:00:00Z"
        });
        let r: Rubric = serde_json::from_value(legacy.clone()).expect("legacy rubric");
        assert!(r.dimensions.iter().all(|d| d.kind == DimensionKind::Llm));
        assert!(r.dimensions.iter().all(|d| d.check.is_default()));
        assert_eq!(serde_json::to_value(&r).expect("re-serialize"), legacy);
    }

    #[test]
    fn deterministic_dimension_round_trips_its_config() {
        let src = json!({
            "key": "answer", "description": "exact answer", "weight": 1.0, "floor": 1.0,
            "kind": "numeric",
            "check": { "expect": "42", "tolerance": 0.1, "path": "/value",
                       "case_sensitive": false, "trim": true }
        });
        let d: RubricDimension = serde_json::from_value(src.clone()).expect("dimension");
        assert_eq!(d.kind, DimensionKind::Numeric);
        assert_eq!(d.check.tolerance, Some(0.1));
        assert!(!d.check.case_sensitive, "case-insensitive is the default");
        assert_eq!(serde_json::to_value(&d).expect("re-serialize"), src);
    }

    #[test]
    fn kind_names_are_stable() {
        for (k, s) in [
            (DimensionKind::Llm, "llm"),
            (DimensionKind::Exact, "exact"),
            (DimensionKind::Regex, "regex"),
            (DimensionKind::Numeric, "numeric"),
            (DimensionKind::JsonValid, "json_valid"),
            (DimensionKind::Contains, "contains"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(serde_json::to_value(k).expect("kind"), json!(s));
        }
    }
}
