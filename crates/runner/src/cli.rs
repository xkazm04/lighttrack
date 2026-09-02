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
    ///
    /// A trailing `@<effort>` (low|medium|high|xhigh|max) sets the CLI reasoning effort, e.g.
    /// `opus@xhigh`. The judge is unbudgeted by design: a cheap judge discriminates poorly — on a
    /// 12-item golden set haiku separated good from bad by only 0.45 where opus@xhigh managed 0.63,
    /// and it failed a genuinely good answer. Trade down deliberately, not by default.
    #[arg(long, env = "LIGHTTRACK_JUDGE_MODEL", default_value = "opus@xhigh")]
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
    /// Pairwise cost guard: refuse to start a `bench --pairwise` round-robin whose game count exceeds
    /// this before a single (paid) call goes out. Round-robin is O(targets² · cases) games at TWO
    /// judge calls each, so cost jumps super-linearly in targets. The abort message prints the exact
    /// value to pass to proceed. Defaults to 500 games (~1000 judge calls).
    #[arg(long, default_value_t = 500)]
    pub(crate) max_games: usize,
    /// Compare cost guard, in US dollars, applied per run. Two things at once: a PRE-FLIGHT abort
    /// when the estimated matrix cost (targets × cases × gen-samples × judge-samples) exceeds it,
    /// before a single paid call goes out; and a LIVE ceiling that halts the run at a case boundary
    /// if the real spend reaches it — whatever ran is kept and reported as `partial`, never as a
    /// finished run. `0` disables both. Defaults to $25.
    #[arg(long, default_value_t = 25.0)]
    pub(crate) max_cost: f64,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Score recent events (those with both input and output) for a project. Skips events that
    /// already have a score, so it's safe to re-run; `--interval` turns it into an online loop.
    Score {
        /// Freeform judge criteria (use this OR --rubric-id).
        #[arg(long)]
        rubric: Option<String>,
        /// Structured rubric id to fetch from the API and judge per-dimension (use this OR --rubric).
        #[arg(long)]
        rubric_id: Option<String>,
        #[arg(long)]
        project: Option<String>,
        /// Only judge events carrying this `metadata.prompt` tag ("<name>@v<version>", M23).
        ///
        /// Judge calls cost money and a freshly-promoted version has minutes of traffic against
        /// production'''s days, so an unprioritized scorer spends its budget re-judging the version
        /// nobody is asking a question about. Point this at the canary and the online quality read
        /// (`GET /v1/quality/prompts`) accumulates evidence where a decision is pending.
        #[arg(long)]
        prompt_tag: Option<String>,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// Run continuously, scoring newly-arrived (unscored) events every N seconds. 0 = one-shot.
        #[arg(long, default_value_t = 0)]
        interval: u64,
        /// Enqueue this work as a job and let a worker run it, instead of running it here.
        ///
        /// The same cycle either way — the queue adds a lease, a heartbeat, cancellation, retry
        /// accounting and a record that it ran. Recurrence belongs in a stored schedule
        /// (`POST /v1/projects/:id/schedules`); this is the one-shot equivalent.
        #[arg(long)]
        via_queue: bool,
    },
    /// Score an ad-hoc input/output pair (not tied to a stored event).
    ScoreText {
        /// Freeform judge criteria (use this OR --rubric-id).
        #[arg(long)]
        rubric: Option<String>,
        /// Structured rubric id to fetch from the API and judge per-dimension (use this OR --rubric).
        #[arg(long)]
        rubric_id: Option<String>,
        #[arg(long)]
        input: String,
        #[arg(long)]
        output: String,
        #[arg(long)]
        project: String,
    },
    /// Auto-score whole traces: sample recently-completed traces for a project, judge each one's
    /// root exchange, and post a whole-trace score. Idempotent — a trace already scored for this
    /// rubric is never re-judged, so repeated runs never double-score. `--interval` runs it as a
    /// daemon; `--once` runs a single cycle (for OS cron / Cloud Scheduler).
    ScoreTraces {
        #[arg(long)]
        project: String,
        /// Freeform judge criteria (use this OR --rubric-id).
        #[arg(long)]
        rubric: Option<String>,
        /// Structured rubric id to fetch from the API and judge per-dimension (use this OR --rubric).
        #[arg(long)]
        rubric_id: Option<String>,
        /// Sample rate: judge ~1 of every N settled traces, chosen by a stable hash of the trace id
        /// (order-independent, so the same ~1/N subset is picked each cycle). 1 = every trace.
        #[arg(long, default_value_t = 1)]
        sample_every: usize,
        /// Always judge error traces, regardless of the sample rate.
        #[arg(long)]
        errors_always: bool,
        /// A trace counts as "completed" once its newest event is older than this many seconds — the
        /// settle window (traces carry no explicit completion marker).
        #[arg(long, default_value_t = 120)]
        settle_secs: i64,
        /// Max settled traces to consider per cycle (walked newest-first via keyset pages).
        #[arg(long, default_value_t = 100)]
        limit: usize,
        /// Judge spec `[provider/]model` override for this run (else the global --model).
        #[arg(long)]
        judge: Option<String>,
        /// Run continuously, scoring newly-settled traces every N seconds. 0 = one-shot.
        #[arg(long, default_value_t = 0)]
        interval: u64,
        /// Run a single cycle and exit (for an external scheduler). Overrides --interval.
        #[arg(long)]
        once: bool,
        /// Enqueue this work as a job and let a worker run it, instead of running it here.
        ///
        /// The same cycle either way — the queue adds a lease, a heartbeat, cancellation, retry
        /// accounting and a record that it ran. Recurrence belongs in a stored schedule
        /// (`POST /v1/projects/:id/schedules`); this is the one-shot equivalent.
        #[arg(long)]
        via_queue: bool,
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
        /// Judge this many cases per provider call instead of one (rubric mode). Amortizes the
        /// per-call context — the bulk of a judge run's tokens — across the batch.
        ///
        /// This is a METHODOLOGY change, not just a speed knob: a judge that sees N cases at once
        /// may anchor on them, so batched scores are not interchangeable with unbatched ones and
        /// every verdict records the batch size it was produced under. Measure the effect on your
        /// own rubric with `calibrate --compare-batch` before trusting it, and do not compare a
        /// batched run against an unbatched baseline. Default 1 (each case judged alone).
        #[arg(long, default_value_t = 1)]
        batch: usize,
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
        ///
        /// The file path is now an *import* route rather than the only one: a set on one machine's
        /// disk cannot be listed, re-used by a second calibration or attributed to whoever graded
        /// it. Prefer `--dataset`; `lt-runner labels import <file>` moves an existing file across.
        #[arg(long)]
        file: Option<String>,
        /// Stored dataset to calibrate against: its items paired with the human labels on them.
        /// Use this OR --file.
        #[arg(long)]
        dataset: Option<String>,
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
        /// Measure what batched judging does to THIS rubric: judge every item both singly and in
        /// batches of N, then report the paired difference. Requires --rubric-id.
        ///
        /// Run this before trusting `bench --batch`. Batching amortizes the per-call context but
        /// lets the judge see several cases at once, and whether that shifts your scores depends on
        /// your rubric and judge model — so it is measured, not assumed.
        #[arg(long)]
        compare_batch: Option<usize>,
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
        /// Enqueue this work as a job and let a worker run it, instead of running it here.
        ///
        /// The same cycle either way — the queue adds a lease, a heartbeat, cancellation, retry
        /// accounting and a record that it ran. Recurrence belongs in a stored schedule
        /// (`POST /v1/projects/:id/schedules`); this is the one-shot equivalent.
        #[arg(long)]
        via_queue: bool,
    },
    /// Human verdicts: the ground truth a judge is calibrated against.
    Labels {
        #[command(subcommand)]
        action: LabelsCmd,
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
        /// Enqueue this work as a job and let a worker run it, instead of running it here.
        ///
        /// The same cycle either way — the queue adds a lease, a heartbeat, cancellation, retry
        /// accounting and a record that it ran. Recurrence belongs in a stored schedule
        /// (`POST /v1/projects/:id/schedules`); this is the one-shot equivalent.
        #[arg(long)]
        via_queue: bool,
    },
    /// Run as a worker: poll the job queue and execute jobs of every declared kind.
    Serve {
        /// Process at most one cycle (claim+run one job, or exit if none) and stop.
        #[arg(long)]
        once: bool,
        /// Seconds to wait between polls when the queue is empty.
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Lease TTL: reclaim a job whose holder has not proved it is alive for this many seconds.
        ///
        /// This is **detection latency**, not job duration. It used to be 600 — sized to outlast the
        /// slowest benchmark, which meant a killed worker's job was untouchable for ten minutes.
        /// Now the holder renews on a timer (`--lease-renew-secs`), so a run may take hours while
        /// this stays small.
        #[arg(long, default_value_t = 120)]
        stale_secs: i64,
        /// Heartbeat cadence: renew the lease this often while a job runs. Default `0` = a third of
        /// `--stale-secs`, the conventional fraction: missing one or two renewals (a GC pause, a
        /// transient API error) must not forfeit a live worker's job, or every hiccup becomes a
        /// spurious takeover.
        #[arg(long, default_value_t = 0)]
        lease_renew_secs: u64,
        /// Job kinds this worker will claim, comma-separated
        /// (`bench_run,score_events,score_traces,dataset_sample,calibrate`). Default: all.
        ///
        /// A capability declaration, not a filter for convenience: the API applies it INSIDE the
        /// atomic claim, so a worker without a Claude install (or without a provider key) stops
        /// taking jobs it would only fail — which used to strand them off the queue under a lease
        /// while a capable worker idled beside them.
        #[arg(long, value_delimiter = ',')]
        kinds: Vec<String>,
        /// Model providers this worker holds credentials for. Default: derived from the API keys
        /// present in the environment.
        #[arg(long, value_delimiter = ',')]
        providers: Vec<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum LabelsCmd {
    /// Write a labelled file into the ledger through the API — the migration path off files.
    ///
    /// Accepts the same shape a calibration file already has (`human_score`, `note`) plus a
    /// `subject` (`event:<id>` / `dataset_item:<id>` / `score:<id>`), so an existing golden file
    /// becomes importable by adding one field rather than being rewritten.
    Import {
        /// JSONL or JSON-array file of label rows.
        #[arg(long)]
        file: String,
        /// Project to attach the labels to (else derived from the API key).
        #[arg(long)]
        project: Option<String>,
        /// Who graded these, when a row does not say. Defaults to `import:<file>` — an
        /// unattributable verdict is the one thing the ledger refuses.
        #[arg(long)]
        labeler: Option<String>,
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
