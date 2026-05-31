//! Command-line interface (clap).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lt-runner", about = "LightTrack scoring/benchmark worker")]
pub(crate) struct Cli {
    #[arg(long, env = "LIGHTTRACK_URL", default_value = "http://127.0.0.1:8787")]
    pub(crate) base: String,
    #[arg(long, env = "LIGHTTRACK_KEY")]
    pub(crate) key: Option<String>,
    /// Default judge spec `[provider/]model` for score/score-text (benchmarks use their own).
    #[arg(long, env = "LIGHTTRACK_JUDGE_MODEL", default_value = "haiku")]
    pub(crate) model: String,
    /// Path to the claude executable. On Windows the default auto-resolves the npm `claude.exe`
    /// (the `claude.cmd`/`.ps1` shims can't be invoked directly from a child process).
    #[arg(long, env = "LIGHTTRACK_CLAUDE_BIN", default_value = "claude")]
    pub(crate) claude_bin: String,
    /// Pass --bare to claude (cheap: skips ~40k token context load, but needs ANTHROPIC_API_KEY).
    #[arg(long)]
    pub(crate) bare: bool,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Score recent events (those with both input and output) for a project.
    Score {
        #[arg(long)]
        rubric: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Score an ad-hoc input/output pair (not tied to a stored event).
    ScoreText {
        #[arg(long)]
        rubric: String,
        #[arg(long)]
        input: String,
        #[arg(long)]
        output: String,
        #[arg(long)]
        project: String,
    },
    /// Run a stored benchmark: judge each case, aggregate a scorecard, record a run.
    Bench {
        #[arg(long)]
        benchmark: String,
        /// Self-consistency: judge each case this many times and average (rubric mode).
        #[arg(long, default_value_t = 1)]
        samples: u32,
        /// Add an LLM-generated recommendations/"healing" paragraph to the report (rubric mode).
        #[arg(long)]
        heal: bool,
    },
    /// Build a dataset by sampling real events and anonymizing them.
    Dataset {
        #[command(subcommand)]
        action: DatasetCmd,
    },
    /// Measure judge↔human agreement on a labeled set (Cohen's κ, correlation) to validate a rubric.
    Calibrate {
        /// JSONL (one object per line) or JSON-array file of {input, output, human_score, ...}.
        #[arg(long)]
        file: String,
        /// Freeform criteria text for the judge (use this OR --rubric-id).
        #[arg(long)]
        rubric: Option<String>,
        /// Structured rubric id to fetch from the API and judge per-dimension (use this OR --rubric).
        #[arg(long)]
        rubric_id: Option<String>,
        /// Pass/fail cutoff for binarizing scores (drives κ + agreement rate).
        #[arg(long, default_value_t = 0.7)]
        threshold: f64,
        /// Minimum Cohen's κ for the rubric to be considered "trusted".
        #[arg(long, default_value_t = 0.6)]
        kappa_bar: f64,
        /// Self-consistency: judge each item this many times and average (rubric mode).
        #[arg(long, default_value_t = 1)]
        samples: u32,
        /// Optional path to write the full JSON report.
        #[arg(long)]
        report: Option<String>,
    },
    /// Run as a worker: poll the job queue and execute jobs (e.g. bench_run).
    Serve {
        /// Process at most one cycle (claim+run one job, or exit if none) and stop.
        #[arg(long)]
        once: bool,
        /// Seconds to wait between polls when the queue is empty.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Reclaim jobs stuck in `running` longer than this many seconds.
        #[arg(long, default_value_t = 600)]
        stale_secs: i64,
    },
}

#[derive(Subcommand)]
pub(crate) enum DatasetCmd {
    /// Sample N recent events for a project, scrub PII, and freeze a new dataset.
    Build {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
        #[arg(long, default_value_t = 50)]
        n: usize,
        /// Add an LLM (claude -p) anonymization pass for names/free-text PII the regex misses.
        #[arg(long)]
        llm_scrub: bool,
    },
}
