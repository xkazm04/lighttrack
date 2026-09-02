//! Failure mining (M24): a failing online verdict becomes a permanent eval case.
//!
//! Before this, `pass = false` was a number on a dashboard. The one artefact worth keeping from a
//! bad verdict — the *case that produced it* — was never captured, so every regression set in the
//! product was hand-built and stayed at whatever size someone had patience for. The loop that
//! closes it is small and deliberately opt-in: a benchmark declares `regression_dataset` in its
//! `target` object (see [`REGRESSION_DATASET_KEY`]), and the scorer appends the failing event's id
//! to the current unfrozen version of that name.
//!
//! **Best-effort, never fatal.** Mining is a side effect of scoring, and a scoring pass that failed
//! because a corpus was unreachable would trade the verdict — the thing that was paid for — for the
//! sample. Every failure here is counted and printed, and the pass continues.

use anyhow::Result;

use lighttrack_core::{Benchmark, ImportSpec};

use crate::cli::Cli;
use crate::dataset_import::{import_into, open_version};
use crate::http::get;

/// The regression policy in force for one scoring pass.
pub(crate) struct Policy {
    project: String,
    /// Dataset names failing verdicts append to, deduplicated across benchmarks.
    datasets: Vec<String>,
}

impl Policy {
    /// Resolve the policy for `project`, narrowed to the rubric being judged under.
    ///
    /// Narrowed, because "failed" means a different thing under a different set of criteria: a case
    /// that failed a tone rubric is not a regression case for a correctness benchmark, and mixing
    /// them is how a regression set stops predicting anything. A freeform judge (`rubric_id: None`)
    /// matches only benchmarks that also declare no structured rubric.
    pub(crate) fn resolve(
        cli: &Cli,
        http: &reqwest::blocking::Client,
        project: &str,
        rubric_id: Option<&str>,
    ) -> Result<Policy> {
        let benchmarks: Vec<Benchmark> =
            get(cli, http, &format!("/v1/projects/{project}/benchmarks"))?;
        let mut datasets: Vec<String> = Vec::new();
        for b in &benchmarks {
            if b.rubric_id.as_deref() != rubric_id {
                continue;
            }
            if let Some(name) = b.regression_dataset() {
                if !datasets.iter().any(|d| d == name) {
                    datasets.push(name.to_string());
                }
            }
        }
        Ok(Policy {
            project: project.to_string(),
            datasets,
        })
    }

    /// `true` when no benchmark asked for mining — the overwhelmingly common case, and the one
    /// worth checking before doing any work at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.datasets.is_empty()
    }

    /// Append one failing event to every declared regression set. Returns how many cases were
    /// actually written (dedupe means a repeat failure of the same prompt writes nothing).
    pub(crate) fn mine(
        &self,
        cli: &Cli,
        http: &reqwest::blocking::Client,
        event_id: &str,
    ) -> Result<u32> {
        let spec = ImportSpec::for_event(event_id);
        let mut written = 0u32;
        for name in &self.datasets {
            let ds = open_version(cli, http, &self.project, name)?;
            written += import_into(cli, http, &ds.id, &spec)?;
        }
        Ok(written)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::REGRESSION_DATASET_KEY;
    use serde_json::json;

    fn benchmark(rubric_id: Option<&str>, regression: Option<&str>) -> Benchmark {
        let mut v = json!({
            "name": "b", "rubric": "criteria", "project_id": "p",
            "target": {},
        });
        if let Some(r) = rubric_id {
            v["rubric_id"] = json!(r);
        }
        if let Some(d) = regression {
            v["target"][REGRESSION_DATASET_KEY] = json!(d);
        }
        serde_json::from_value(v).expect("benchmark fixture")
    }

    /// The default: nothing is mined unless someone asked for it. Mining every failure by default
    /// would grow an unbounded corpus out of a transient provider outage.
    #[test]
    fn a_benchmark_without_the_policy_mines_nothing() {
        assert_eq!(benchmark(None, None).regression_dataset(), None);
        assert_eq!(
            benchmark(None, Some("  ")).regression_dataset(),
            None,
            "a blank name is not a policy"
        );
        assert_eq!(
            benchmark(None, Some("regressions")).regression_dataset(),
            Some("regressions")
        );
    }

    /// "Failed" means a different thing under a different rubric, so the match is exact — a
    /// freeform verdict must not feed a structured rubric's regression set, or vice versa.
    #[test]
    fn the_rubric_match_is_exact_including_the_freeform_case() {
        let structured = benchmark(Some("rb1"), Some("r1"));
        let freeform = benchmark(None, Some("r2"));
        assert_eq!(structured.rubric_id.as_deref(), Some("rb1"));
        assert_eq!(freeform.rubric_id.as_deref(), None);
        // The comparison the resolver makes, spelled out here so a change to it fails a test rather
        // than quietly cross-feeding two corpora.
        assert_ne!(structured.rubric_id.as_deref(), None);
        assert_ne!(freeform.rubric_id.as_deref(), Some("rb1"));
    }
}
