//! The judging contract a scoring command runs under: freeform criteria text (`--rubric`) or a
//! structured, weighted rubric fetched by id (`--rubric-id`).
//!
//! Every command that judges caller-supplied text — `score`, `score-text`, `score-traces` — resolves
//! the same two flags into the same [`Judge`], so the weighted-dimension / gating-floor methodology
//! of `docs/BENCHMARK_FRAMEWORK.md` §3 is reachable from all of them and spelled identically in each.
//! (`bench` takes its rubric from the stored benchmark's `rubric_id`, and `calibrate` re-judges a
//! labeled file, so both resolve the contract their own way.)

use anyhow::{bail, Context, Result};

use lighttrack_core::{Rubric, ScoreDetail, ScoreKind};
use lighttrack_engine::{
    build_judge_prompt, parse_judge_spec, run_judge, run_rubric_judge, EngineConfig,
};

use crate::cli::Cli;
use crate::http::get;
use crate::provenance::{freeform_detail, rubric_detail, weakest_reasoning};

/// The judging contract for one run. The `label` is the `rubric` field written on every posted
/// score — and, for `score-traces`, the key its idempotency check matches on.
pub(crate) enum Judge {
    Freeform(String),
    Structured(Box<Rubric>),
}

/// A unified judge verdict, whichever contract produced it — the shape a score body needs.
pub(crate) struct Verdict {
    pub value: f64,
    pub max: f64,
    pub pass: bool,
    pub reasoning: String,
    pub scored_by: String,
    pub cost_usd: Option<f64>,
    /// Per-dimension provenance (D11). Posted by the event/ad-hoc scorers; the trace door stamps its
    /// own coverage instead, so `score-traces` does not send it.
    pub detail: ScoreDetail,
}

impl Judge {
    /// Resolve the contract; exactly one of `--rubric` / `--rubric-id` is required. Fetching the
    /// rubric here — once, before any work — means a bad id fails before the first paid judge call
    /// rather than on every cycle of a loop.
    pub(crate) fn resolve(
        cli: &Cli,
        http: &reqwest::blocking::Client,
        rubric_text: Option<&str>,
        rubric_id: Option<&str>,
    ) -> Result<Judge> {
        match (rubric_text, rubric_id) {
            (Some(t), None) => Ok(Judge::Freeform(t.to_string())),
            (None, Some(id)) => {
                let r: Rubric = get(cli, http, &format!("/v1/rubrics/{id}"))
                    .with_context(|| format!("fetching rubric '{id}'"))?;
                Ok(Judge::Structured(Box::new(r)))
            }
            (Some(_), Some(_)) => bail!("pass exactly one of --rubric or --rubric-id, not both"),
            (None, None) => bail!("one of --rubric or --rubric-id is required"),
        }
    }

    /// What the posted score's `rubric` field says: the criteria text, or the rubric's name.
    pub(crate) fn label(&self) -> &str {
        match self {
            Judge::Freeform(text) => text,
            Judge::Structured(r) => &r.name,
        }
    }

    /// The stored rubric this contract judges against, when there is one.
    ///
    /// Stamped onto every verdict beside the label. The label is what a human reads; this is what a
    /// query joins on — it survives a rename and separates two rubrics that share a name, neither of
    /// which the label can do.
    pub(crate) fn rubric_id(&self) -> Option<&str> {
        match self {
            Judge::Freeform(_) => None,
            Judge::Structured(r) => Some(&r.id),
        }
    }

    /// What sort of verdict this contract produces.
    pub(crate) fn kind(&self) -> ScoreKind {
        match self {
            Judge::Freeform(_) => ScoreKind::Freeform,
            Judge::Structured(_) => ScoreKind::Rubric,
        }
    }

    /// Judge one input/output pair under this contract.
    pub(crate) fn judge(
        &self,
        engine: &EngineConfig,
        input: &str,
        output: &str,
    ) -> Result<Verdict> {
        let (jp, jm) = parse_judge_spec(&engine.model);
        match self {
            Judge::Freeform(text) => {
                let prompt = build_judge_prompt(text, input, output);
                let o = run_judge(engine, &jp, &jm, &prompt).context("judge failed")?;
                Ok(Verdict {
                    value: o.verdict.score,
                    max: o.verdict.max,
                    pass: o.verdict.pass,
                    reasoning: o.verdict.reasoning.clone(),
                    scored_by: o.model.clone(),
                    cost_usd: o.cost_usd,
                    detail: freeform_detail(&o),
                })
            }
            Judge::Structured(r) => {
                let o = run_rubric_judge(engine, &jp, &jm, r, input, None, output, 1, 1)
                    .context("rubric judge failed")?;
                let detail = rubric_detail(&o);
                Ok(Verdict {
                    value: o.overall,
                    max: 1.0,
                    pass: o.pass,
                    // The judge's own words for the weakest dimension — a template restating the
                    // rubric's shape tells a reader nothing they couldn't already compute. An
                    // all-deterministic rubric produces no prose at all, hence the fallback.
                    reasoning: weakest_reasoning(&detail).unwrap_or_else(|| {
                        format!("rubric '{}' ({} dims)", r.name, o.dimensions.len())
                    }),
                    scored_by: o.model,
                    cost_usd: o.cost_usd,
                    detail,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::RubricDimension;

    fn rubric(name: &str) -> Rubric {
        Rubric {
            id: "r1".into(),
            project_id: "p1".into(),
            name: name.into(),
            dimensions: vec![RubricDimension {
                key: "correctness".into(),
                description: "right?".into(),
                weight: 1.0,
                anchors: Vec::new(),
                floor: None,
                kind: Default::default(),
                check: Default::default(),
            }],
            threshold: 0.7,
            version: 1,
            supersedes: None,
            created_at: chrono::Utc::now(),
        }
    }

    /// The label is what lands in `Score.rubric`, so a structured contract must be identified by the
    /// rubric's *name* — never by its dimensions or its id.
    #[test]
    fn label_is_the_criteria_text_or_the_rubric_name() {
        assert_eq!(Judge::Freeform("be helpful".into()).label(), "be helpful");
        assert_eq!(
            Judge::Structured(Box::new(rubric("support-quality"))).label(),
            "support-quality"
        );
    }
}
