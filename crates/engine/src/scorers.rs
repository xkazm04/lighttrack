//! Deterministic (non-LLM) rubric dimensions: mechanical checks evaluated locally at zero tokens and
//! zero cost, feeding the *same* weighting / floor / aggregation pipeline as LLM dimensions.
//!
//! Two failure modes are kept apart on purpose. A **misconfigured** dimension (a `regex` with no
//! pattern, an `exact` with nothing to compare against, an unparseable numeric target) is an operator
//! bug and returns [`EngineError::Other`] naming the dimension — it must never masquerade as a
//! candidate that scored 0. A **candidate** that simply fails the check scores 0.0 with the reason
//! recorded, exactly as auditable as an LLM verdict.
//!
//! Each deterministic dimension is evaluated once: it is exactly reproducible, so re-running cannot
//! move it. That is why it stays out of the cross-sample `agreement` number (see [`crate::judge`]).

use regex::RegexBuilder;
use serde_json::Value;

use lighttrack_core::{DimensionCheck, DimensionKind, Rubric, RubricDimension};

use crate::{EngineError, Result};

/// One deterministic dimension's verdict: its score plus why it got it.
pub(crate) struct DetScore {
    pub(crate) key: String,
    pub(crate) score: f64,
    pub(crate) reasoning: String,
}

/// Longest value echoed into a reasoning string (the stored detail is capped again at the API).
const SNIP: usize = 120;

fn snip(s: &str) -> String {
    if s.chars().count() <= SNIP {
        return s.to_string();
    }
    s.chars().take(SNIP - 1).chain(std::iter::once('…')).collect()
}

/// True when this rubric has at least one dimension the judge model must actually score.
pub(crate) fn has_llm_dims(rubric: &Rubric) -> bool {
    rubric.dimensions.iter().any(|d| d.kind.is_llm())
}

/// Evaluate every deterministic dimension of `rubric` against the candidate `output` (and the case's
/// `expected` reference), in rubric order. An all-`llm` rubric yields an empty vec and costs nothing.
pub(crate) fn evaluate_all(
    rubric: &Rubric,
    expected: Option<&str>,
    output: &str,
) -> Result<Vec<DetScore>> {
    rubric
        .dimensions
        .iter()
        .filter(|d| !d.kind.is_llm())
        .map(|d| {
            let (score, reasoning) = evaluate(d, expected, output)?;
            Ok(DetScore { key: d.key.clone(), score, reasoning })
        })
        .collect()
}

/// Score one deterministic dimension. `Ok((score, reasoning))` covers both pass and fail; `Err` is
/// reserved for a rubric that cannot be evaluated at all.
fn evaluate(d: &RubricDimension, expected: Option<&str>, output: &str) -> Result<(f64, String)> {
    let c = &d.check;
    let kind = d.kind.as_str();
    // The part of the output under test: the whole thing, or the value at a JSON Pointer. A path that
    // does not resolve is the *candidate's* failure, not the operator's.
    let subject = match select(c, output) {
        Ok(s) => s,
        Err(why) => return Ok((0.0, format!("{kind}: {why} → fail"))),
    };

    Ok(match d.kind {
        DimensionKind::Exact => {
            let want = target(d, expected)?;
            verdict(
                same(&subject, &want, c.case_sensitive),
                format!("exact: expected `{}`, got `{}`", snip(&want), snip(&subject)),
            )
        }
        DimensionKind::Contains => {
            let want = target(d, expected)?;
            let (hay, needle) = folded(&subject, &want, c.case_sensitive);
            verdict(
                hay.contains(&needle),
                format!("contains: looked for `{}` in `{}`", snip(&want), snip(&subject)),
            )
        }
        DimensionKind::Regex => {
            let pattern = c.pattern.as_deref().ok_or_else(|| {
                EngineError::Other(format!(
                    "rubric dimension '{}' (regex) has no `check.pattern`",
                    d.key
                ))
            })?;
            let re = RegexBuilder::new(pattern)
                .case_insensitive(!c.case_sensitive)
                .build()
                .map_err(|e| {
                    EngineError::Other(format!(
                        "rubric dimension '{}' has an invalid regex `{pattern}`: {e}",
                        d.key
                    ))
                })?;
            verdict(
                re.is_match(&subject),
                format!("regex: /{pattern}/ against `{}`", snip(&subject)),
            )
        }
        DimensionKind::Numeric => {
            let raw = target(d, expected)?;
            let want: f64 = raw.parse().map_err(|_| {
                EngineError::Other(format!(
                    "rubric dimension '{}' (numeric) target `{raw}` is not a number",
                    d.key
                ))
            })?;
            let tol = c.tolerance.unwrap_or(0.0).abs();
            match first_number(&subject) {
                None => (
                    0.0,
                    format!(
                        "numeric: expected `{want}`, no number in `{}` → fail",
                        snip(&subject)
                    ),
                ),
                Some(got) => verdict(
                    (got - want).abs() <= tol,
                    format!("numeric: expected `{want}`, got `{got}`, tolerance {tol}"),
                ),
            }
        }
        DimensionKind::JsonValid => {
            // A path that resolved already proved the output is JSON; without one, parse it here.
            let parses = c.path.is_some() || serde_json::from_str::<Value>(&subject).is_ok();
            match (parses, c.expect.as_deref()) {
                (false, _) => (
                    0.0,
                    format!("json_valid: `{}` is not valid JSON → fail", snip(&subject)),
                ),
                (true, None) => (1.0, "json_valid: output parses as JSON → pass".to_string()),
                (true, Some(want)) => {
                    let want = if c.trim { want.trim() } else { want };
                    verdict(
                        same(&subject, want, c.case_sensitive),
                        format!(
                            "json_valid: parses, expected `{}` at {}, got `{}`",
                            snip(want),
                            c.path.as_deref().unwrap_or("the root"),
                            snip(&subject)
                        ),
                    )
                }
            }
        }
        // Unreachable: `evaluate_all` filters LLM dimensions out. Defensive rather than silent.
        DimensionKind::Llm => {
            return Err(EngineError::Other(format!(
                "rubric dimension '{}' is LLM-judged and has no deterministic check",
                d.key
            )))
        }
    })
}

/// Attach a pass/fail tail to a check's description, so every mechanical verdict reads the same way.
fn verdict(pass: bool, detail: String) -> (f64, String) {
    let mark = if pass { "pass" } else { "fail" };
    (if pass { 1.0 } else { 0.0 }, format!("{detail} → {mark}"))
}

/// The literal this dimension compares against: `check.expect`, else the case's reference answer.
fn target(d: &RubricDimension, expected: Option<&str>) -> Result<String> {
    let raw = d.check.expect.as_deref().or(expected).ok_or_else(|| {
        EngineError::Other(format!(
            "rubric dimension '{}' ({}) has no target: set `check.expect`, or give the case an \
             `expected` reference answer",
            d.key,
            d.kind.as_str()
        ))
    })?;
    Ok(if d.check.trim { raw.trim().to_string() } else { raw.to_string() })
}

/// Narrow the output to the configured JSON Pointer, then trim. `Err` carries the candidate-facing
/// reason the selection failed.
fn select(c: &DimensionCheck, output: &str) -> std::result::Result<String, String> {
    let raw = match &c.path {
        None => output.to_string(),
        Some(p) => {
            let v: Value = serde_json::from_str(output.trim())
                .map_err(|e| format!("output is not JSON ({e}), so path `{p}` cannot be read"))?;
            match v.pointer(p).ok_or_else(|| format!("no value at path `{p}`"))? {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            }
        }
    };
    Ok(if c.trim { raw.trim().to_string() } else { raw })
}

fn same(a: &str, b: &str, case_sensitive: bool) -> bool {
    if case_sensitive {
        a == b
    } else {
        a.to_lowercase() == b.to_lowercase()
    }
}

fn folded(a: &str, b: &str, case_sensitive: bool) -> (String, String) {
    if case_sensitive {
        (a.to_string(), b.to_string())
    } else {
        (a.to_lowercase(), b.to_lowercase())
    }
}

/// The output's number: the whole (trimmed) subject if it parses, else the first numeric token in it —
/// so `"41.6"`, `"The answer is 41.6."` and `"1.2e3"` all yield a number to compare.
fn first_number(s: &str) -> Option<f64> {
    if let Ok(v) = s.trim().parse::<f64>() {
        return Some(v);
    }
    let re = RegexBuilder::new(r"-?\d+(?:\.\d+)?(?:[eE][-+]?\d+)?").build().ok()?;
    re.find(s).and_then(|m| m.as_str().parse::<f64>().ok())
}

#[cfg(test)]
mod tests;
