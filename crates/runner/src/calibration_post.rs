//! Persisting a calibration result (M11): the record first, the reserved-rubric score derived
//! from it.
//!
//! The order is the point. κ history used to live *only* as a `Score` under
//! `lt:calibration:<judge>`, with the metrics packed into `reasoning` as a JSON string — so the one
//! fact that decides whether a judge can be believed was a blob in a free-text column that no gate
//! could query and the previous cycle's κ was recovered by scanning 500 scores client-side.
//!
//! Now the [`CalibrationRecord`] is the fact and the score is a *projection* of it, kept because it
//! is what feeds the API's rolling `score_drop` detector — a degrading κ still rides the existing
//! alert channel, with no parallel alerting built.

use anyhow::Result;
use serde_json::json;

use lighttrack_core::{Agreement, CalibrationRecord, JudgeTrustVerdict, ScoreKind};

use crate::cli::Cli;
use crate::http::{get, post};

/// What a cycle measured, in the shape both writes need.
pub(crate) struct Measured<'a> {
    pub(crate) project: Option<&'a str>,
    pub(crate) rubric_id: Option<&'a str>,
    pub(crate) judge: &'a str,
    pub(crate) dataset_id: Option<&'a str>,
    pub(crate) cost: f64,
}

/// Write the record, then the derived score. The record is written first because it is the durable
/// fact: if the score POST fails, trust is still correct and only the alert stream missed a tick.
pub(crate) fn persist(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    m: &Measured,
    a: &Agreement,
    reserved: &str,
) -> Result<CalibrationRecord> {
    let mut rec =
        CalibrationRecord::from_agreement(m.project.unwrap_or_default(), m.judge, m.rubric_id, a);
    rec.dataset_id = m.dataset_id.map(str::to_string);
    let mut body = serde_json::to_value(&rec)?;
    // An empty project is the "derive it from the API key" case; sending `""` would be a project id
    // of that name.
    if m.project.is_none() {
        if let Some(o) = body.as_object_mut() {
            o.remove("project_id");
        }
    }
    post(cli, http, "/v1/calibrations", &body)?;
    post_score(cli, http, m, a, reserved)?;
    Ok(rec)
}

/// The reserved-rubric `Score`, derived from the same numbers. Kept for the alert path only.
fn post_score(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    m: &Measured,
    a: &Agreement,
    reserved: &str,
) -> Result<()> {
    let metrics = json!({
        "kappa": a.cohen_kappa, "pearson": a.pearson, "mae": a.mae, "rmse": a.rmse, "bias": a.bias,
        "agreement_rate": a.agreement_rate, "human_pass_rate": a.human_pass_rate,
        "judge_pass_rate": a.judge_pass_rate, "n": a.n, "threshold": a.threshold,
        "kappa_bar": a.kappa_bar, "trusted": a.trusted, "judge_cost_usd": m.cost,
    });
    let mut body = json!({
        "rubric": reserved,
        // A calibration probe measures the *judge*, not the product. Typing it keeps it out of
        // every quality rollup that would otherwise average it in with real verdicts.
        "kind": ScoreKind::Calibration.as_str(),
        "value": a.cohen_kappa,
        "max": 1.0,
        "pass": a.trusted,
        "reasoning": metrics.to_string(),
        "scored_by": m.judge,
        "cost_usd": m.cost,
    });
    if let Some(r) = m.rubric_id {
        body["rubric_id"] = json!(r);
    }
    if let Some(pr) = m.project {
        body["project_id"] = json!(pr);
    }
    post(cli, http, "/v1/scores", &body)?;
    Ok(())
}

/// The previous cycle's κ for this exact `(rubric, judge)` pair.
///
/// One indexed lookup, replacing a scan of the newest 500 scores for a reserved rubric name — which
/// silently returned `None` (i.e. "no baseline, nothing has drifted") the moment a busy project
/// pushed the last calibration off the end of that page.
///
/// Best-effort: a read failure ⇒ `None`, so a transient API blip does not abort the cycle.
pub(crate) fn previous_kappa(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    project: Option<&str>,
    rubric_id: Option<&str>,
    judge: &str,
) -> Option<f64> {
    let mut path = format!("/v1/judges/trust?judge={}", urlencode(judge));
    if let Some(p) = project {
        path.push_str(&format!("&project={}", urlencode(p)));
    }
    if let Some(r) = rubric_id {
        path.push_str(&format!("&rubric_id={}", urlencode(r)));
    }
    let v: JudgeTrustVerdict = get(cli, http, &path).ok()?;
    v.calibration.map(|c| c.kappa)
}

/// Percent-encode a query value. A judge is `provider/model` and a rubric id is caller-chosen, so
/// neither can be pasted into a query string raw.
fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A judge is `provider/model`; pasted raw, the `/` would land in the query string as a path
    /// character and the lookup would silently miss.
    #[test]
    fn query_values_are_encoded() {
        assert_eq!(urlencode("anthropic/haiku"), "anthropic%2Fhaiku");
        assert_eq!(urlencode("rb-1_2.3~x"), "rb-1_2.3~x");
        assert_eq!(urlencode("a b&c=d"), "a%20b%26c%3Dd");
    }
}
