use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

/// The structured verdict an LLM judge returns. Used as the `--json-schema` for `claude -p`
/// (lands in the `structured_output` field of the JSON envelope).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JudgeVerdict {
    pub score: f64,
    #[serde(default = "one")]
    pub max: f64,
    #[serde(default)]
    pub pass: bool,
    #[serde(default)]
    pub reasoning: String,
}

fn one() -> f64 {
    1.0
}

/// JSON Schema for [`JudgeVerdict`], to pass to `claude -p --json-schema`.
pub fn judge_verdict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "score":     { "type": "number", "description": "rubric score for this output" },
            "max":       { "type": "number", "description": "upper bound of the scale" },
            "pass":      { "type": "boolean", "description": "whether the output meets the bar" },
            "reasoning": { "type": "string", "description": "concise justification" }
        },
        "required": ["score", "max", "pass", "reasoning"],
        "additionalProperties": false
    })
}

/// Storage bounds for [`ScoreDetail`]. A score row is hot (listed, joined, alerted on), so the
/// provenance it carries is capped rather than unbounded: at most [`MAX_REASONING_CHARS`] per
/// reasoning string, [`MAX_REASONINGS_PER_DIM`] retained per dimension (the first k samples, in
/// sample order), [`MAX_DIMENSIONS`] dimensions and [`MAX_NOTES`] notes. Truncated strings end in `…`
/// so a reader can tell. Worst case ≈ 32 × 8 × 600 B ≈ 150 KB of JSON, and typical rubrics
/// (4 dims × 3 samples) land near 5 KB.
pub const MAX_REASONING_CHARS: usize = 600;
pub const MAX_REASONINGS_PER_DIM: usize = 8;
pub const MAX_DIMENSIONS: usize = 32;
pub const MAX_NOTES: usize = 8;

/// One dimension's contribution to a judged verdict, kept so a stored score can answer *why*.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreDim {
    pub key: String,
    /// The aggregated (mean-over-samples) dimension score.
    pub value: f64,
    pub weight: f64,
    /// The rubric's gating floor for this dimension, when it has one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor: Option<f64>,
    /// True when `value` fell below `floor` — the reason a high overall can still fail. On a
    /// merged cell this is the OR across the cells merged, so it can be true while the merged
    /// (mean) `value` clears the floor; read `floor_hits`/`floor_of` for the shape.
    #[serde(default)]
    pub floor_hit: bool,
    /// How many contributing observations fell below `floor`, and how many there were. One
    /// judged verdict reports its own; a merged cell reports the tally across what it merged.
    ///
    /// `floor_hit` alone cannot separate a dimension that crossed on **every** observation from
    /// one that crossed on **one of five**, because an OR reduces both to `true` — and that is
    /// exactly the difference between a boundary the candidate always hits and one it sometimes
    /// does. The count carries its own denominator so the two are never confused.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_hits: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floor_of: Option<u32>,
    /// The judge's reasoning, one entry per sample that parsed (sample order). Every sample is kept:
    /// its reasoning tokens were paid for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasoning: Vec<String>,
}

/// Structured provenance for a judged verdict: the per-dimension breakdown plus the reliability
/// signals (agreement, sample accounting, bias/injection flags) that produced the scalar `value` on
/// [`Score`]. Additive and nullable — a score posted without it is still a valid score.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ScoreDetail {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dimensions: Vec<ScoreDim>,
    /// Cross-sample agreement on the overall score (1.0 = the samples agreed exactly).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agreement: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_requested: Option<u32>,
    /// How many of the requested samples yielded a usable verdict.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub samples_parsed: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse_failures: Option<u32>,
    /// Pairwise only: swapping A/B flipped the winner, so the verdict collapsed to a tie.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position_bias: Option<bool>,
    /// The judged content imitated a prompt boundary and was neutralized (see engine `fence`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub injection_suspected: Option<bool>,
    /// `"exact"` when every sampling control the judge provider exposes was pinned (temperature 0 +
    /// a fixed seed); `"best-effort"` when the path exposed no seed, no knobs at all, or rejected
    /// the ones we asked for. On a best-effort verdict, cross-sample agreement partly measures
    /// sampling noise rather than genuine ambiguity — so the number means less.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub determinism: Option<String>,
    /// Judge text not tied to a dimension (a freeform verdict's rationale, a pairwise rationale).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// For a **whole-trace** verdict: what it judged (span count + the fingerprint of the judged
    /// root exchange). Stamped by the API on `POST /v1/traces/:id/score`; a trace read compares it
    /// against the trace as it now stands so a verdict that stopped describing its trace says so
    /// instead of aging silently. `None` on every non-trace score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coverage: Option<crate::trace::TraceCoverage>,
    /// How many PII spans the ingest scrub had already replaced in the evidence this verdict was
    /// computed from ([`crate::LlmEvent::redaction`]'s `spans`, copied at judge time).
    ///
    /// The judge reads the *stored* text, so a scrub that mangled a payload silently changes what
    /// was judged — D14's un-observable defect, now observable at the verdict rather than only at
    /// the row. `Some(0)` is a scrubbed-and-untouched payload; `None` is a verdict whose event
    /// carried no stamp at all, which is a weaker statement and kept distinct from it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence_redacted_spans: Option<u32>,
}

/// Truncate on a char boundary, marking that it happened.
fn cap_str(s: &str) -> String {
    if s.chars().count() <= MAX_REASONING_CHARS {
        return s.to_string();
    }
    let mut out: String = s.chars().take(MAX_REASONING_CHARS - 1).collect();
    out.push('…');
    out
}

impl ScoreDetail {
    /// Enforce the storage bounds. Applied by the API on every insert, so a client that posts an
    /// unbounded detail cannot balloon a hot score row.
    pub fn capped(mut self) -> Self {
        self.dimensions.truncate(MAX_DIMENSIONS);
        for d in &mut self.dimensions {
            d.reasoning.truncate(MAX_REASONINGS_PER_DIM);
            for r in &mut d.reasoning {
                *r = cap_str(r);
            }
        }
        self.notes.truncate(MAX_NOTES);
        for n in &mut self.notes {
            *n = cap_str(n);
        }
        self
    }

    /// True when nothing meaningful is recorded (so callers can store `None` instead of `{}`).
    pub fn is_empty(&self) -> bool {
        *self == ScoreDetail::default()
    }
}

/// What kind of verdict a [`Score`] is.
///
/// `Score.rubric` is one free-text column carrying six different encodings — a bare rubric name,
/// `bench:{name}`, `{name}:{label}#case{i}`, a pairwise label, `lt:calibration:…`, a trace rubric —
/// so every consumer had to parse a string to find out what it was reading, and the alerting path
/// keyed its window on that string. A compare cell therefore minted a **unique key per case**, and a
/// window that never sees the same key twice never accumulates: `score_drop` could not fire on a
/// benchmark's case stream at all.
///
/// Typed here, defaulted to [`ScoreKind::Freeform`], and the legacy `rubric` string is kept verbatim
/// beside it — this classifies existing verdicts rather than replacing their identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScoreKind {
    /// An ad-hoc verdict with no rubric behind it (the online scorer's default judge).
    #[default]
    Freeform,
    /// Judged against a stored [`crate::Rubric`]; `rubric_id` names it.
    Rubric,
    /// One case of a benchmark run.
    BenchCase,
    /// One (target × case) cell of a comparison matrix.
    CompareCell,
    /// One game of a pairwise comparison.
    PairwiseGame,
    /// A judge-calibration probe (`lt:calibration:…`), not a product verdict.
    Calibration,
    /// A whole-trace verdict, anchored to the trace's root span.
    Trace,
    /// A kind this binary does not know — a newer writer's verdict read by an older reader. Kept as
    /// a variant rather than silently folded into `Freeform`, which would misfile it.
    #[serde(other)]
    Other,
}

impl ScoreKind {
    /// The stable wire string.
    pub fn as_str(self) -> &'static str {
        match self {
            ScoreKind::Freeform => "freeform",
            ScoreKind::Rubric => "rubric",
            ScoreKind::BenchCase => "bench_case",
            ScoreKind::CompareCell => "compare_cell",
            ScoreKind::PairwiseGame => "pairwise_game",
            ScoreKind::Calibration => "calibration",
            ScoreKind::Trace => "trace",
            ScoreKind::Other => "other",
        }
    }

    /// Parse a wire string; `None` for anything outside the vocabulary, so a filter can 400 on a
    /// typo instead of answering with an empty page.
    pub fn parse(s: &str) -> Option<Self> {
        ScoreKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s.trim().to_ascii_lowercase())
    }

    /// Every kind, so a filter validator derives its accepted set from the enum.
    pub const ALL: [ScoreKind; 8] = [
        ScoreKind::Freeform,
        ScoreKind::Rubric,
        ScoreKind::BenchCase,
        ScoreKind::CompareCell,
        ScoreKind::PairwiseGame,
        ScoreKind::Calibration,
        ScoreKind::Trace,
        ScoreKind::Other,
    ];

    /// Whether this kind is **per-case** work inside a larger run.
    ///
    /// These are the kinds whose legacy `rubric` string embeds a case index, which is what made
    /// every one of them a unique alert key. An alert window rolls them up under the run's benchmark
    /// instead, so a benchmark's case stream is one accumulating series again.
    pub fn is_run_case(self) -> bool {
        matches!(
            self,
            ScoreKind::BenchCase | ScoreKind::CompareCell | ScoreKind::PairwiseGame
        )
    }
}

/// A stored judge result, optionally tied to the event it scored.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Score {
    #[serde(default = "crate::new_id")]
    pub id: String,
    /// Defaulted so a keyed poster can omit it (the API derives it from the API key).
    #[serde(default)]
    pub project_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_id: Option<String>,
    /// The verdict's **human-readable label**, kept verbatim for every existing consumer: a bare
    /// rubric name, `bench:{name}`, `{name}:{label}#case{i}`, `lt:calibration:…`. Six encodings in
    /// one column, which is why it is no longer the identity — [`Score::rubric_id`] and
    /// [`Score::kind`] are.
    pub rubric: String,
    /// The [`crate::Rubric`] this verdict was judged against, when there was one. `None` for a
    /// freeform verdict, and for a verdict written before the column existed.
    ///
    /// This is the join the label could never be: a rubric renamed between two runs used to make
    /// them two unrelated series, and two rubrics that happened to share a name used to look like
    /// one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
    /// What sort of verdict this is. Defaults to [`ScoreKind::Freeform`] so every stored score
    /// deserializes; the runner stamps the real kind at every producer.
    #[serde(default)]
    pub kind: ScoreKind,
    pub value: f64,
    #[serde(default = "one")]
    pub max: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    /// Structured provenance for this verdict (per-dimension breakdown, agreement, sample
    /// accounting, bias flags). Additive and nullable: pre-existing clients that never send or read
    /// it keep working. Persisted by the SQLite backend; other backends default it to `None`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<ScoreDetail>,
    /// The [`BenchmarkRun`] this verdict belongs to, when it came from a benchmark. Without it, two
    /// runs of the same benchmark are distinguishable only by `created_at` ordering, so "why did run
    /// 47 fail?" has no query. The runner mints the run id **before** judging and stamps it on every
    /// case it posts, so the run's cases exist even if the run row is never written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// 1-based position of the case within the run's dataset (pairwise: the case a game was played
    /// on, so it repeats across that case's games). Ordering key for the run's case listing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_index: Option<u32>,
    /// Judge model, e.g. `claude-haiku-4-5`.
    pub scored_by: String,
    /// Cost of the judge call. Recorded for visibility (Agent SDK credit burn); never throttled.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl Score {
    /// The **series key** an alert window accumulates this verdict under.
    ///
    /// Two failures it fixes, both from keying on the free-text label:
    ///
    /// 1. A rubric renamed between runs split one series into two, and two rubrics sharing a name
    ///    merged into one. `rubric_id` is stable across a rename and unique across a collision, so
    ///    it wins whenever the row carries it.
    /// 2. A per-case label (`{name}:{label}#case7`, `bench:x#case7`) is unique per case, so the
    ///    window never saw the same key twice and `score_drop` could not fire on a benchmark's case
    ///    stream at all. Run cases therefore roll up under the run's benchmark — the label with the
    ///    `#case…` suffix removed, which is exactly the benchmark-scoped prefix the producers build.
    ///
    /// Falls back to the label for a pre-typing row, so an existing alert window keeps its history
    /// rather than resetting on upgrade.
    pub fn alert_key(&self) -> String {
        if self.kind.is_run_case() {
            if let Some(id) = &self.rubric_id {
                return id.clone();
            }
            if let Some((prefix, _)) = self.rubric.split_once("#case") {
                return prefix.to_string();
            }
            return self.rubric.clone();
        }
        self.rubric_id
            .clone()
            .unwrap_or_else(|| self.rubric.clone())
    }
}

/// One case in a benchmark dataset. `output` is the candidate to judge; `expected` is an optional
/// reference answer the judge can compare against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkCase {
    pub input: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Reserved key under a benchmark's free-form `target` object carrying its opt-in recurrence
/// interval in seconds. The API writes it (folding recurrence into `target` so the SQLite/Postgres
/// schema stays untouched) and the runner reads it to decide due-ness — two crates that must agree
/// on one spelling. Defined here as the single authority both import, so a rename can't leave one
/// side silently reading `None` for every benchmark and stopping recurrence with no error.
pub const RECURRENCE_KEY: &str = "schedule_interval_secs";

/// Reserved key under a benchmark's `target` object naming the dataset a failing online verdict
/// under this benchmark's rubric appends to (M24).
///
/// Folded into `target` for the same reason [`RECURRENCE_KEY`] is: a policy field that only some
/// benchmarks set does not earn a column in three backends' `benchmarks` table, and a reserved key
/// both the API and the runner import from one place cannot drift the way two spellings of a column
/// name would. The value is a dataset **name**, not an id — the whole point is that it survives
/// forking, and a fork mints a new id.
pub const REGRESSION_DATASET_KEY: &str = "regression_dataset";

/// A benchmark definition: a dataset + rubric + judge run repeatedly to track quality over time.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Benchmark {
    #[serde(default = "crate::new_id")]
    pub id: String,
    #[serde(default)]
    pub project_id: String,
    pub name: String,
    pub rubric: String,
    #[serde(default = "default_judge_model")]
    pub judge_model: String,
    /// How to produce outputs to judge (e.g. an endpoint, a model+prompt). Free-form for now.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub target: Value,
    /// Reference to an external case dataset (path/URI/table), if not inlined in `dataset`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dataset_ref: Option<String>,
    /// Optional structured rubric (id) for per-dimension judging; falls back to `rubric` text if unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rubric_id: Option<String>,
    /// Inline dataset of cases.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dataset: Vec<BenchmarkCase>,
    /// Baseline mean score to detect regressions against.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline_score: Option<f64>,
    #[serde(default = "Utc::now")]
    pub created_at: DateTime<Utc>,
}

impl Benchmark {
    /// The dataset name failing verdicts under this benchmark append to, if the policy is set
    /// ([`REGRESSION_DATASET_KEY`]). `None` — the default — means failures are not mined, which is
    /// what every benchmark did before M24.
    pub fn regression_dataset(&self) -> Option<&str> {
        self.target
            .get(REGRESSION_DATASET_KEY)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
    }
}

fn default_judge_model() -> String {
    "haiku".to_string()
}

/// One execution of a [`Benchmark`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkRun {
    #[serde(default = "crate::new_id")]
    pub id: String,
    pub benchmark_id: String,
    #[serde(default = "Utc::now")]
    pub started_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at: Option<DateTime<Utc>>,
    #[serde(default)]
    pub n_cases: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mean_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pass_rate: Option<f64>,
    #[serde(default)]
    pub cost_usd: f64,
    /// `running` | `passed` | `regressed` | `failed`.
    #[serde(default = "default_run_status")]
    pub status: String,
    // Phase 3.6a: response-time + token aggregates for the run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p50_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub p95_latency_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub total_tokens: Option<u64>,
    /// Phase 3.6c: per-dimension breakdown + recommendations/healing for this run.
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub report: Value,
}

fn default_run_status() -> String {
    "completed".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn score(kind: ScoreKind, rubric: &str, rubric_id: Option<&str>) -> Score {
        Score {
            id: "s".into(),
            project_id: "p".into(),
            event_id: None,
            rubric: rubric.into(),
            rubric_id: rubric_id.map(str::to_string),
            kind,
            value: 0.5,
            max: 1.0,
            pass: None,
            reasoning: None,
            detail: None,
            run_id: None,
            case_index: None,
            scored_by: "judge".into(),
            cost_usd: None,
            created_at: Utc::now(),
        }
    }

    #[test]
    fn every_kind_round_trips_through_its_wire_string() {
        for k in ScoreKind::ALL {
            assert_eq!(ScoreKind::parse(k.as_str()), Some(k), "{k:?}");
        }
        assert_eq!(ScoreKind::parse("BENCH_CASE"), Some(ScoreKind::BenchCase));
        assert_eq!(ScoreKind::parse("nonsense"), None, "a typo is not a kind");
    }

    /// A verdict written before typing existed must read as a freeform score with no rubric id,
    /// not fail to deserialize.
    #[test]
    fn a_pre_typing_score_reads_as_freeform() {
        let legacy: Score = serde_json::from_value(json!({
            "id": "s1", "project_id": "p1", "rubric": "quality",
            "value": 0.8, "max": 1.0, "scored_by": "judge",
            "created_at": "2026-01-01T00:00:00Z"
        }))
        .expect("legacy score");
        assert_eq!(legacy.kind, ScoreKind::Freeform);
        assert!(legacy.rubric_id.is_none());
        // …and its alert key is the label it always had, so an existing window keeps its history.
        assert_eq!(legacy.alert_key(), "quality");
    }

    /// A kind a newer writer introduced must not be silently misfiled as `freeform`.
    #[test]
    fn an_unknown_kind_reads_as_other_not_as_freeform() {
        let s: Score = serde_json::from_value(json!({
            "id": "s1", "project_id": "p1", "rubric": "x", "kind": "some_future_kind",
            "value": 0.8, "max": 1.0, "scored_by": "j", "created_at": "2026-01-01T00:00:00Z"
        }))
        .expect("forward-compatible");
        assert_eq!(s.kind, ScoreKind::Other);
    }

    /// The defect this closes: a per-case label is unique per case, so a window keyed on it never
    /// sees the same key twice, and a drop alert can never accumulate over a benchmark's cases.
    #[test]
    fn run_cases_roll_up_under_their_benchmark_instead_of_one_key_per_case() {
        let keys: Vec<String> = (1..=3)
            .map(|i| {
                score(
                    ScoreKind::CompareCell,
                    &format!("quality:gpt-5.4#case{i}"),
                    None,
                )
                .alert_key()
            })
            .collect();
        assert_eq!(
            keys,
            vec!["quality:gpt-5.4"; 3],
            "three cases, one accumulating series"
        );
        assert_eq!(
            score(ScoreKind::BenchCase, "bench:quality#case9", None).alert_key(),
            "bench:quality"
        );
    }

    /// `rubric_id` takes precedence where the row carries one: a rubric renamed between two runs
    /// used to split one series into two, and two rubrics sharing a name used to merge into one.
    #[test]
    fn the_rubric_id_is_the_key_when_the_row_carries_one() {
        assert_eq!(
            score(ScoreKind::Rubric, "renamed-since", Some("rub-1")).alert_key(),
            "rub-1"
        );
        assert_eq!(
            score(ScoreKind::BenchCase, "bench:x#case4", Some("rub-1")).alert_key(),
            "rub-1"
        );
    }

    #[test]
    fn only_the_per_case_kinds_are_run_cases() {
        for k in ScoreKind::ALL {
            let expect = matches!(
                k,
                ScoreKind::BenchCase | ScoreKind::CompareCell | ScoreKind::PairwiseGame
            );
            assert_eq!(k.is_run_case(), expect, "{k:?}");
        }
    }
}
