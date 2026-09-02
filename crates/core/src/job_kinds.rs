//! Typed payloads for each [`JobKind`](crate::JobKind).
//!
//! The queue's payload stays a `serde_json::Value` on the row — a typed column would force a
//! migration for every new kind — but the *shape* each kind expects is declared here rather than
//! rediscovered by a chain of `payload.get("x").and_then(Value::as_u64).unwrap_or(1)` at the one
//! place that executes it. Two things follow, and both were missing while `bench_run` was the only
//! kind:
//!
//! * `POST /v1/jobs` can **validate before enqueue**, so a typo'd payload is a 400 at the door
//!   instead of a job that claims, runs, fails, retries twice and dead-letters;
//! * the worker parses once, at the top of its dispatch, and works with a struct.
//!
//! Every field that has a sensible default has one, so a payload stays as small as the caller's
//! intent. The required fields are the ones with no defensible default — the benchmark to run, the
//! project to score — and their absence is exactly what validation is for.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::job::JobKind;

/// Run a stored benchmark (the original, and still the only kind that spends generation budget).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchRunPayload {
    pub benchmark_id: String,
    #[serde(default = "one_u32")]
    pub samples: u32,
    #[serde(default = "one_u32")]
    pub gen_samples: u32,
    #[serde(default)]
    pub heal: bool,
    #[serde(default)]
    pub pairwise: bool,
    /// Bounded parallelism; `None` = the worker's own `--jobs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub jobs: Option<usize>,
    /// Provenance passthrough for a version-triggered enqueue (prompt registry).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// The prompt's **registry name** — the key a `BenchTarget.prompt_ref` matches on, and so the
    /// only way the runner can tell which of a matrix's targets `version` overrides. Without it a
    /// version-triggered run resolved whatever each target's own ref said, which is precisely the
    /// "gate that does not see its target" the id/version provenance pair could not fix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_name: Option<String>,
}

/// Judge recent unscored events for a project — one cycle of what `lt-runner score` loops over.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreEventsPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(flatten)]
    pub judge: JudgeSpec,
    /// Only judge events carrying this `metadata.prompt` tag (M23), so a queued scoring cycle can
    /// put its paid judge calls on the version a promotion decision is pending for.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt_tag: Option<String>,
    #[serde(default = "ten")]
    pub limit: usize,
}

/// Judge whole traces for a project — one cycle of `lt-runner score-traces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScoreTracesPayload {
    pub project: String,
    #[serde(flatten)]
    pub judge: JudgeSpec,
    #[serde(default = "one_usize")]
    pub sample_every: usize,
    #[serde(default)]
    pub errors_always: bool,
    #[serde(default = "settle_secs")]
    pub settle_secs: i64,
    #[serde(default = "hundred")]
    pub limit: usize,
    /// Judge spec `[provider/]model` override for this cycle.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judge_model: Option<String>,
}

/// Sample live events into a frozen dataset — one cycle of `lt-runner schedule`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatasetSamplePayload {
    pub project: String,
    #[serde(default = "online")]
    pub name_prefix: String,
    #[serde(default = "fifty")]
    pub n: usize,
    #[serde(default)]
    pub llm_scrub: bool,
}

/// Re-measure judge/human agreement against a golden set — one cycle of `lt-runner calibrate --watch`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalibratePayload {
    /// Path to the golden set, **on the worker's filesystem**: a file import, kept for the
    /// deployments whose labelled data has always lived there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    /// The stored dataset whose items + [`crate::Label`]s are the golden set (M11). The preferred
    /// source: a labelled set on the worker's disk cannot be listed, re-used by a second
    /// calibration, or audited, and it is the one input the whole judge-trust argument rests on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_id: Option<String>,
    #[serde(flatten)]
    pub judge: JudgeSpec,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default = "seven_tenths")]
    pub threshold: f64,
    #[serde(default = "six_tenths")]
    pub kappa_bar: f64,
    #[serde(default = "fifteen_hundredths")]
    pub drift_threshold: f64,
    #[serde(default = "one_u32")]
    pub samples: u32,
}

/// Push this instance's digest to a collective hub and record the ack — one cycle of what
/// `lt collective contribute` does by hand.
///
/// `hub_key_ref` is the **name of an environment variable**, never the key: a schedule row and a
/// job payload are both readable by anything that can read the queue, and a hub credential sitting
/// in one would be a secret at rest in the observability database. Absent ⇒
/// `LIGHTTRACK_COLLECTIVE_HUB_KEY`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContributePayload {
    /// The hub's base URL (`https://hub.example`), trailing slash tolerated.
    pub hub: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hub_key_ref: Option<String>,
    /// k-anonymity floor for the digest build; `None` ⇒ the server's default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_cases: Option<u32>,
}

impl ContributePayload {
    /// A hub with no host is the one field with no defensible default, and the failure it produces
    /// (a POST to nowhere, every interval, forever) is exactly what enqueue-time validation is for.
    pub fn validate(&self) -> Result<(), String> {
        let hub = self.hub.trim();
        if hub.is_empty() {
            return Err("'hub' is required: the base URL of the collective hub to push to".into());
        }
        if !(hub.starts_with("http://") || hub.starts_with("https://")) {
            return Err(format!(
                "'hub' must be an absolute http(s) URL, got {hub:?}"
            ));
        }
        Ok(())
    }
}

impl CalibratePayload {
    /// Exactly one golden-set source. Said here, at enqueue time, rather than by the worker after
    /// it has claimed the job: "neither" is a job that can only ever fail, and "both" is two
    /// different answers to *what was measured* on a record that claims to describe one set.
    pub fn validate_source(&self) -> Result<(), String> {
        match (self.file.as_deref(), self.dataset_id.as_deref()) {
            (Some(f), None) if !f.trim().is_empty() => Ok(()),
            (None, Some(d)) if !d.trim().is_empty() => Ok(()),
            (Some(_), Some(_)) => Err("give either `file` or `dataset_id`, not both".into()),
            _ => Err("one of `file` or `dataset_id` is required".into()),
        }
    }
}

/// The `--rubric` / `--rubric-id` contract, shared by every judging kind. Exactly one is required;
/// [`JudgeSpec::validate`] says so at enqueue time rather than at the first paid call.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct JudgeSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
}

impl JudgeSpec {
    pub fn validate(&self) -> Result<(), String> {
        match (self.rubric.as_deref(), self.rubric_id.as_deref()) {
            (Some(_), None) | (None, Some(_)) => Ok(()),
            (Some(_), Some(_)) => {
                Err("pass exactly one of 'rubric' or 'rubric_id', not both".into())
            }
            (None, None) => Err("one of 'rubric' or 'rubric_id' is required".into()),
        }
    }
}

/// Parse-and-check `payload` against what `kind` expects, returning a human reason on refusal.
///
/// Used by `POST /v1/jobs` and by the schedule sweep before it enqueues, so a malformed schedule is
/// caught when it is written rather than every interval forever.
pub fn validate_payload(kind: JobKind, payload: &Value) -> Result<(), String> {
    fn parse<T: for<'de> Deserialize<'de>>(p: &Value) -> Result<T, String> {
        serde_json::from_value(p.clone()).map_err(|e| e.to_string())
    }
    match kind {
        JobKind::BenchRun => parse::<BenchRunPayload>(payload).map(|_| ()),
        JobKind::ScoreEvents => {
            parse::<ScoreEventsPayload>(payload).and_then(|p| p.judge.validate())
        }
        JobKind::ScoreTraces => {
            parse::<ScoreTracesPayload>(payload).and_then(|p| p.judge.validate())
        }
        JobKind::DatasetSample => parse::<DatasetSamplePayload>(payload).map(|_| ()),
        JobKind::Calibrate => parse::<CalibratePayload>(payload)
            .and_then(|p| p.judge.validate().and_then(|()| p.validate_source())),
        JobKind::Contribute => parse::<ContributePayload>(payload).and_then(|p| p.validate()),
    }
    .map_err(|e| format!("invalid payload for job kind '{}': {e}", kind.as_str()))
}

fn one_u32() -> u32 {
    1
}
fn one_usize() -> usize {
    1
}
fn ten() -> usize {
    10
}
fn fifty() -> usize {
    50
}
fn hundred() -> usize {
    100
}
fn settle_secs() -> i64 {
    120
}
fn online() -> String {
    "online".to_string()
}
fn seven_tenths() -> f64 {
    0.7
}
fn six_tenths() -> f64 {
    0.6
}
fn fifteen_hundredths() -> f64 {
    0.15
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_minimal_payload_fills_in_the_rest() {
        let p: BenchRunPayload =
            serde_json::from_value(json!({ "benchmark_id": "b1" })).expect("minimal bench payload");
        assert_eq!(p.samples, 1);
        assert_eq!(p.gen_samples, 1);
        assert!(!p.heal);
        assert!(p.jobs.is_none());
    }

    #[test]
    fn validation_catches_at_the_door_what_would_otherwise_dead_letter() {
        // Missing the one field that has no defensible default.
        let e = validate_payload(JobKind::BenchRun, &json!({})).expect_err("must refuse");
        assert!(e.contains("bench_run"), "{e}");
        // A judging kind with neither half of the rubric contract is refused, not queued.
        let e = validate_payload(JobKind::ScoreEvents, &json!({ "project": "p" }))
            .expect_err("must refuse");
        assert!(e.contains("rubric"), "{e}");
        // …and with both halves, which is ambiguous rather than merely incomplete.
        let e = validate_payload(
            JobKind::ScoreEvents,
            &json!({ "rubric": "be good", "rubric_id": "r1" }),
        )
        .expect_err("must refuse");
        assert!(e.contains("not both"), "{e}");
        // A well-formed one passes.
        validate_payload(JobKind::ScoreEvents, &json!({ "rubric": "be good" })).expect("valid");
        validate_payload(
            JobKind::DatasetSample,
            &json!({ "project": "p", "n": 5, "llm_scrub": true }),
        )
        .expect("valid");
    }

    /// A contribute schedule that names no hub would POST to nowhere on every interval forever;
    /// one that names a bare hostname would do the same with a friendlier-looking payload.
    #[test]
    fn a_contribution_must_name_an_absolute_hub() {
        let e = validate_payload(JobKind::Contribute, &json!({ "hub": "" })).expect_err("refuse");
        assert!(e.contains("hub"), "{e}");
        let e = validate_payload(JobKind::Contribute, &json!({ "hub": "hub.example" }))
            .expect_err("refuse");
        assert!(e.contains("absolute"), "{e}");
        validate_payload(
            JobKind::Contribute,
            &json!({ "hub": "https://hub.example/" }),
        )
        .expect("valid");
        // The key is referenced BY NAME. A payload carrying the key itself would put a hub
        // credential at rest in the queue every schedule tick writes to.
        let p: ContributePayload = serde_json::from_value(
            json!({ "hub": "https://hub.example", "hub_key_ref": "MY_HUB_KEY" }),
        )
        .expect("payload");
        assert_eq!(p.hub_key_ref.as_deref(), Some("MY_HUB_KEY"));
        assert!(p.min_cases.is_none());
    }

    #[test]
    fn the_judge_contract_flattens_into_every_judging_payload() {
        let p: ScoreTracesPayload =
            serde_json::from_value(json!({ "project": "p", "rubric_id": "r1" }))
                .expect("traces payload");
        assert_eq!(p.judge.rubric_id.as_deref(), Some("r1"));
        assert_eq!(p.sample_every, 1);
        assert_eq!(p.settle_secs, 120);
    }
}
