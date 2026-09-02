//! Recurring workloads and the background job queue.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum SchedulesCmd {
    /// List schedules — one project's, or every recurring workload in the deployment.
    List {
        #[arg(long)]
        project: Option<String>,
    },
    /// Store a new recurring workload.
    Create {
        #[arg(long)]
        project: String,
        /// bench_run | score_events | score_traces | dataset_sample | calibrate.
        #[arg(long = "type")]
        kind: String,
        /// How often it fires: `30m`, `6h`, `1d`, or bare seconds.
        #[arg(long)]
        every: String,
        /// The job payload as JSON, e.g. '{"benchmark_id":"b-1","samples":2}'.
        #[arg(long)]
        payload: Option<String>,
        /// Seconds until the first firing (default 0 = due at once).
        #[arg(long, default_value_t = 0)]
        start_in_secs: i64,
        /// Store it paused; it fires nothing until enabled.
        #[arg(long)]
        paused: bool,
    },
    /// Change a schedule. Omitted fields are left alone, so pausing cannot rewrite the payload.
    Set {
        id: String,
        #[arg(long)]
        every: Option<String>,
        #[arg(long)]
        payload: Option<String>,
        /// Resume a paused schedule.
        #[arg(long, conflicts_with = "disable")]
        enable: bool,
        /// Pause it. It stays listed — an operator has to be able to see what they paused.
        #[arg(long)]
        disable: bool,
    },
    /// Remove a schedule. The jobs it already produced are kept.
    Delete { id: String },
    /// The jobs one schedule has produced.
    Runs { id: String },
}

#[derive(Subcommand)]
pub(crate) enum JobsCmd {
    /// Recent jobs, newest first.
    List {
        /// queued | running | cancelling | done | failed | cancelled.
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One job: status, progress, error, result.
    Show { id: String },
    /// Enqueue one unit of work now (the one-shot counterpart to a schedule).
    Enqueue {
        /// bench_run | score_events | score_traces | dataset_sample | calibrate.
        #[arg(long = "type")]
        kind: String,
        /// The job payload as JSON.
        #[arg(long)]
        payload: Option<String>,
    },
    /// Ask a queued or running job to stop.
    Cancel { id: String },
}
