use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How a dimension is evaluated. `Llm` (the default) asks the judge model; every other kind is a
/// mechanical check scored into the same weighting / floor / aggregation pipeline at zero tokens —
/// the five text kinds locally and for free, [`Exec`](DimensionKind::Exec) remotely and for wall
/// clock. Additive and defaulted: a rubric written before kinds existed deserializes as all-`Llm`
/// and re-serializes byte-identically.
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
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
    /// The candidate output is written into a sandbox built from a pinned image and
    /// `check.cmd` is run there; the command's **exit code** is the verdict (0 → 1.0).
    ///
    /// The only kind that is neither local nor free, and the only one with a third outcome: a
    /// sandbox that could not run the command at all (timeout, auth, image pull, capacity) is
    /// **unavailable**, which voids the dimension for that case rather than scoring the candidate
    /// 0.0 — attributing our own outage to the model is the defect this kind exists to avoid.
    /// See `docs/BENCHMARK_FRAMEWORK.md` §3d.
    Exec,
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
            DimensionKind::Exec => "exec",
        }
    }

    /// True for the LLM-judged default — the only kind that costs a model call.
    pub fn is_llm(&self) -> bool {
        matches!(self, DimensionKind::Llm)
    }

    /// True for the sandboxed kind — the only deterministic kind that leaves this machine, can be
    /// `unavailable`, and costs wall clock. Callers that must not make a network call (a rubric
    /// validator, a dry run, an offline test) branch on this rather than on `!is_llm()`.
    pub fn is_exec(&self) -> bool {
        matches!(self, DimensionKind::Exec)
    }
}

/// Per-kind configuration for a deterministic dimension. Every field is optional, so one struct
/// serves all kinds and an `llm` dimension serializes without it at all.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// `exec` only: the sandbox image the case runs in — an image UUID, or `tag:NAME`.
    ///
    /// A UUID pins the exact machine and stamps the outcome `exact`; a tag is mutable, so it stamps
    /// `best-effort`. Dependencies and fixtures belong in this image, baked once — a case that
    /// installs its own dependencies is measuring the network.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// `exec` only: the command run inside the sandbox. Its exit code is the whole verdict; its
    /// stdout is never parsed for one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cmd: Option<String>,
    /// `exec` only: absolute path inside the sandbox where the candidate output is written before
    /// `cmd` runs (e.g. `/work/src/lib.rs`). `path` still applies first, so a model that answers
    /// `{"code": "..."}` can have the code extracted before it is written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub write: Option<String>,
    /// `exec` only: wall-clock ceiling for the command, in seconds. Unset uses the engine default.
    /// A timeout is `unavailable`, not a fail: we do not know what the code would have done.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// `exec` only: literal fixture environment variables set inside the sandbox.
    ///
    /// **Literal values only — never read from this host's environment.** The thing being executed
    /// is untrusted text a language model wrote, so there is no mechanism here by which a provider
    /// key, an admin key or a project key can reach it; credential-shaped names are refused by
    /// [`DimensionCheck::validate_exec`].
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
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
            image: None,
            cmd: None,
            write: None,
            timeout_secs: None,
            env: BTreeMap::new(),
        }
    }
}

/// Environment names an `exec` fixture may never set. Not a guess at every secret in the world — it
/// is the set this product itself hands around, plus the two generic shapes that are always a
/// mistake to put in front of model-written code. The real guarantee is structural (fixture env is
/// literal, and the host environment is never forwarded); this list is the loud second rung, so an
/// operator who pastes a key into a rubric is told rather than obeyed.
const FORBIDDEN_ENV_SUBSTRINGS: [&str; 6] =
    ["KEY", "TOKEN", "SECRET", "PASSWORD", "CREDENTIAL", "COOKIE"];

impl DimensionCheck {
    /// Nothing configured — so an `llm` dimension can omit the whole object when serializing.
    pub fn is_default(&self) -> bool {
        *self == DimensionCheck::default()
    }

    /// Validate an `exec` dimension's configuration, naming the dimension in every error.
    ///
    /// Called when a rubric is **created**, so a misconfigured `exec` is a 400 at authoring time
    /// rather than a surprise inside the run that was meant to gate a deploy — the same "operator
    /// errors are loud" rule the text kinds already follow, moved earlier because this kind's
    /// failures cost wall clock and a remote round trip to discover.
    pub fn validate_exec(&self, key: &str) -> Result<(), String> {
        let need = |v: &Option<String>, field: &str| -> Result<String, String> {
            match v.as_deref().map(str::trim) {
                Some(s) if !s.is_empty() => Ok(s.to_string()),
                _ => Err(format!(
                    "rubric dimension '{key}' (exec) has no `check.{field}`"
                )),
            }
        };
        need(&self.image, "image")?;
        need(&self.cmd, "cmd")?;
        let write = need(&self.write, "write")?;
        if !write.starts_with('/') {
            return Err(format!(
                "rubric dimension '{key}' (exec) has `check.write` = `{write}`, which is not an \
                 absolute path inside the sandbox"
            ));
        }
        if self.timeout_secs == Some(0) {
            return Err(format!(
                "rubric dimension '{key}' (exec) has `check.timeout_secs` = 0"
            ));
        }
        for name in self.env.keys() {
            let upper = name.to_uppercase();
            if FORBIDDEN_ENV_SUBSTRINGS.iter().any(|f| upper.contains(f)) {
                return Err(format!(
                    "rubric dimension '{key}' (exec) sets `check.env.{name}`, whose name looks like \
                     a credential. A sandbox runs untrusted model-written code and never receives \
                     one; use a fixture value with a different name, or serve the secret from \
                     inside the image."
                ));
            }
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

/// One scored dimension of a rubric (e.g. correctness, completeness, faithfulness, concision).
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
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
    /// Which generation of this rubric this is. Editing a rubric changes what a score *means*, so
    /// comparing verdicts across an edit is comparing two different measurements — and nothing
    /// recorded that an edit had happened. Starts at 1; `POST /v1/rubrics/:id/versions` mints the
    /// next.
    ///
    /// A new version is a **new row with a new id**, not a mutation: the old rubric must stay
    /// readable, because the verdicts that cite it are still stored and still cite it.
    ///
    /// Omitted from the wire at generation 1, which is the same statement as absence (absent
    /// deserializes to 1) and keeps a pre-versioning rubric serializing byte-identically.
    #[serde(default = "default_version", skip_serializing_if = "is_first_version")]
    pub version: u32,
    /// The rubric id this one replaces, so the chain is walkable in both directions. `None` on the
    /// first version of a rubric.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

/// The pass threshold a rubric gets when none is given. One constant, read by the core default and
/// the API's request default alike — they were two literals that happened to agree.
pub const DEFAULT_RUBRIC_THRESHOLD: f64 = 0.7;

fn default_threshold() -> f64 {
    DEFAULT_RUBRIC_THRESHOLD
}

/// A rubric written before versioning existed is generation 1 — the reading that keeps every stored
/// verdict citing a coherent generation, rather than an unnumbered one.
fn default_version() -> u32 {
    1
}

fn is_first_version(v: &u32) -> bool {
    *v == 1
}

impl Rubric {
    /// The next generation of this rubric: a **new row with a new id**, linked back to this one.
    ///
    /// Not a mutation, on purpose. Verdicts already stored cite this rubric's id, and editing the
    /// row underneath them would silently change what those verdicts claim to have measured — the
    /// same class of restatement the revenue upsert refuses. `dimensions` and `threshold` come from
    /// the caller; identity, lineage and the clock do not.
    pub fn next_version(&self, dimensions: Vec<RubricDimension>, threshold: f64) -> Rubric {
        Rubric {
            id: crate::new_id(),
            project_id: self.project_id.clone(),
            name: self.name.clone(),
            dimensions,
            threshold,
            version: self.version.saturating_add(1),
            supersedes: Some(self.id.clone()),
            created_at: Utc::now(),
        }
    }
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
    fn exec_dimension_round_trips_its_config() {
        let src = json!({
            "key": "passes", "description": "compiles and passes", "weight": 3.0, "floor": 1.0,
            "kind": "exec",
            // `case_sensitive` and `trim` always serialize on a non-default check — the same
            // shape the text-kind round-trip above asserts.
            "check": { "case_sensitive": false, "trim": true,
                       "image": "tag:lt-py:v1", "cmd": "pytest -q", "write": "/work/s.py",
                       "timeout_secs": 180, "env": { "FIXTURE_SEED": "7" } }
        });
        let d: RubricDimension = serde_json::from_value(src.clone()).expect("dimension");
        assert_eq!(d.kind, DimensionKind::Exec);
        assert!(d.kind.is_exec() && !d.kind.is_llm());
        assert_eq!(d.check.timeout_secs, Some(180));
        d.check.validate_exec(&d.key).expect("valid");
        assert_eq!(serde_json::to_value(&d).expect("re-serialize"), src);
    }

    /// The exec fields are additive: a rubric that never heard of them is untouched on the wire.
    #[test]
    fn exec_fields_are_invisible_to_rubrics_that_do_not_use_them() {
        let d: RubricDimension =
            serde_json::from_value(json!({ "key": "a", "description": "", "weight": 1.0 }))
                .expect("dimension");
        let back = serde_json::to_value(&d).expect("re-serialize");
        for absent in [
            "image",
            "cmd",
            "write",
            "timeout_secs",
            "env",
            "kind",
            "check",
        ] {
            assert!(
                back.get(absent).is_none(),
                "{absent} must not appear: {back}"
            );
        }
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
            (DimensionKind::Exec, "exec"),
        ] {
            assert_eq!(k.as_str(), s);
            assert_eq!(serde_json::to_value(k).expect("kind"), json!(s));
        }
    }
}
