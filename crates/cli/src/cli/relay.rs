//! The cloud→device relay's verbs: the fleet, the task queue, and the action fingerprint ledger.

use clap::{Args, Subcommand};

#[derive(Subcommand)]
pub(crate) enum RelayCmd {
    /// The enrolled device fleet.
    Devices {
        #[command(subcommand)]
        action: RelayDevicesCmd,
    },
    /// The work handed to devices: what is queued, leased, done or dead.
    Tasks {
        #[command(subcommand)]
        action: RelayTasksCmd,
    },
    /// Which prompt text each action has actually been running, and when.
    Actions {
        #[command(flatten)]
        args: RelayActionsArgs,
        #[command(subcommand)]
        action: Option<RelayActionsCmd>,
    },
}

/// The ledger read's own filters, kept flattened so `lt relay actions --limit 5000` still works
/// beside the `snapshot` subcommand.
#[derive(Args)]
pub(crate) struct RelayActionsArgs {
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// Settle events to walk (default 1000, cap 20000).
    #[arg(long, default_value_t = 1000)]
    pub(crate) limit: usize,
}

#[derive(Subcommand)]
pub(crate) enum RelayActionsCmd {
    /// Snapshot an action's succeeded runs into a dataset, so a benchmark can gate its prompt.
    Snapshot {
        /// The namespaced action type, e.g. `xprice/reprice-summary`.
        action_type: String,
        /// The project whose succeeded runs are snapshotted.
        #[arg(long)]
        project: String,
        /// Dataset name (default `relay:<action_type>`).
        #[arg(long)]
        name: Option<String>,
        /// Succeeded tasks to snapshot (default 200, cap 1000).
        #[arg(long)]
        limit: Option<usize>,
    },
}

#[derive(Subcommand)]
pub(crate) enum RelayTasksCmd {
    /// Hand one unit of work to the fleet; an action nothing advertises is refused, not queued.
    Enqueue {
        /// The action the device is to run, e.g. `xprice/reprice-summary`.
        #[arg(long = "type")]
        action_type: String,
        /// Parameters for the action as JSON — never prompts and never credentials.
        #[arg(long)]
        payload: Option<String>,
        /// Admin/dev only; a project key forces its own project.
        #[arg(long)]
        project: Option<String>,
        /// Who enqueued it.
        #[arg(long)]
        source: Option<String>,
        /// Re-enqueueing with the same (project, key) returns the existing task.
        #[arg(long = "idempotency-key")]
        idempotency_key: Option<String>,
        /// Attempts before the task dead-letters.
        #[arg(long = "max-attempts")]
        max_attempts: Option<i64>,
        /// Wait between attempts.
        #[arg(long = "retry-interval-secs")]
        retry_interval_secs: Option<i64>,
    },
    /// Relay tasks, newest first — did the work a device was handed actually run?
    List {
        #[arg(long)]
        project: Option<String>,
        /// queued | leased | succeeded | dead | cancelling | cancelled.
        #[arg(long)]
        status: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    /// Stop a queued or leased task; a finished one is a 409, never a silent success.
    Cancel { id: String },
}

#[derive(Subcommand)]
pub(crate) enum RelayDevicesCmd {
    /// List enrolled devices: advertised capabilities, liveness, agent version, revocation.
    List {
        /// One project's devices; operator-wide devices are always included.
        #[arg(long)]
        project: Option<String>,
    },
    /// Enrol a device. Prints its key ONCE — only a salted digest is stored, so a lost key is
    /// re-enrolled, never recovered.
    Add {
        /// Human name for the machine, e.g. `studio-laptop`.
        #[arg(long)]
        name: String,
        /// Scope it to one project; omit for an operator-wide device serving every project.
        #[arg(long)]
        project: Option<String>,
        /// What it may run — an exact action type or a `ns/*` namespace. Repeatable. Omit for
        /// "everything"; the device's own action inventory narrows this at its first lease.
        #[arg(long = "capability")]
        capability: Vec<String>,
    },
    /// Revoke a device: it authenticates nothing and is eligible for nothing. A flag, not a delete,
    /// so tasks it already ran keep naming a device that still resolves.
    Revoke { id: String },
}
