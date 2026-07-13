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
    /// Max concurrent judge/generation calls for `bench` / `compare` / `score` / `calibrate`. The
    /// judge is unbudgeted, so bounded parallelism just cuts wall-clock; `1` = fully sequential and
    /// byte-identical output. Defaults to 4.
    #[arg(long, default_value_t = 4)]
    pub(crate) jobs: usize,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Score recent events (those with both input and output) for a project. Skips events that
    /// already have a score, so it's safe to re-run; `--interval` turns it into an online loop.
    Score {
        #[arg(long)]
        rubric: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Run continuously, scoring newly-arrived (unscored) events every N seconds. 0 = one-shot.
        #[arg(long, default_value_t = 0)]
        interval: u64,
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
        /// Judge self-consistency: judge each candidate this many times and average (rubric mode).
        #[arg(long, default_value_t = 1)]
        samples: u32,
        /// Generation self-consistency (compare mode): generate this many candidates per case and
        /// average their scores, to average out generation variance.
        #[arg(long, default_value_t = 1)]
        gen_samples: u32,
        /// Add an LLM-generated recommendations/"healing" paragraph to the report (rubric mode).
        #[arg(long)]
        heal: bool,
        /// CI gate: exit non-zero on a regressed verdict (code 3) or no baseline (code 4), so a
        /// pipeline step fails the build. Without this flag the exit code is unchanged (0 on success).
        #[arg(long)]
        gate: bool,
        /// Compare mode only: also run order-debiased round-robin A-vs-B pairwise judging across the
        /// targets and print a win/loss/tie matrix + win-rate ranking (alongside the per-target table).
        #[arg(long)]
        pairwise: bool,
    },
    /// Build a dataset by sampling real events and anonymizing them.
    Dataset {
        #[command(subcommand)]
        action: DatasetCmd,
    },
    /// Sync revenue from a billing provider (Stripe) into LightTrack, for profit tracking.
    Billing {
        #[command(subcommand)]
        action: BillingCmd,
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
        /// Drift sentinel: re-judge the golden set on a schedule, persist κ history via /v1/scores
        /// under `lt:calibration:<judge>`, and alert on trust degradation. Daemon unless `--once`.
        #[arg(long)]
        watch: bool,
        /// Run a single watch cycle and exit (for cron / Cloud Scheduler); implies `--watch`. Exits
        /// non-zero when the cycle ends untrusted (κ < --kappa-bar).
        #[arg(long)]
        once: bool,
        /// Seconds between watch cycles (daemon mode).
        #[arg(long, default_value_t = 3600)]
        interval: u64,
        /// Watch mode: warn when κ falls by more than this vs the previous run, even if still trusted.
        #[arg(long, default_value_t = 0.15)]
        drift_threshold: f64,
        /// Watch mode: project id to attach the persisted calibration scores to (else derived from
        /// the API key). Also scopes the history read used for drift detection.
        #[arg(long)]
        project: Option<String>,
    },
    /// Periodically sample live events into frozen datasets (online sampling). Daemon by default;
    /// `--once` runs a single cycle (for OS cron / Cloud Scheduler / a systemd timer).
    Schedule {
        #[arg(long)]
        project: String,
        /// Seconds between sampling cycles (daemon mode).
        #[arg(long, default_value_t = 3600)]
        interval: u64,
        /// Run a single cycle and exit (for an external scheduler).
        #[arg(long)]
        once: bool,
        /// Events to sample per cycle (most recent).
        #[arg(long, default_value_t = 50)]
        n: usize,
        /// Dataset name prefix; each cycle creates `<prefix>-<UTC timestamp>`.
        #[arg(long, default_value = "online")]
        name_prefix: String,
        /// Add an LLM (claude -p) anonymization pass for names/free-text PII the regex misses.
        #[arg(long)]
        llm_scrub: bool,
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
        /// Seconds between benchmark-recurrence sweeps. Each sweep enqueues a `bench_run` for any
        /// benchmark whose opt-in `schedule_interval_secs` is due (continuous quality monitoring).
        /// `0` disables recurrence. With `--once`, one sweep always runs (so OS cron can drive it).
        #[arg(long, default_value_t = 60)]
        recur_interval: u64,
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

#[derive(Subcommand)]
pub(crate) enum BillingCmd {
    /// Pull paid invoices since a cutoff and post them as revenue (needs `STRIPE_API_KEY`).
    Sync {
        #[arg(long, default_value = "stripe")]
        provider: String,
        #[arg(long)]
        project: String,
        /// Look back this many days.
        #[arg(long, default_value_t = 30)]
        days: i64,
    },
}
