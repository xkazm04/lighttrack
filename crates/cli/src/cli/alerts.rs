//! The alert ledger's verbs, and the routing config behind it.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum AlertsCmd {
    /// Recent alerts, newest first.
    List {
        #[arg(long)]
        project: Option<String>,
        /// limit_breach | limit_warning | forecast_alert | relay_task_dead | error_spike |
        /// score_drop | bench_run | ingest_rejected.
        #[arg(long)]
        kind: Option<String>,
        /// Window start: an RFC3339 instant, or a relative `30m` / `24h` / `7d`.
        #[arg(long)]
        since: Option<String>,
        /// Only alerts someone has acknowledged.
        #[arg(long, conflicts_with = "open")]
        acked: bool,
        /// Only alerts nobody has acknowledged yet — the on-call view.
        #[arg(long)]
        open: bool,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Keyset cursor from a previous page's `next_cursor`. Without it you only ever see page one.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Acknowledge one alert: record that a human saw it.
    Ack {
        id: String,
        /// Who saw it — an on-call handle, an email, a runbook link. Defaults server-side to the
        /// calling key's label.
        #[arg(long)]
        by: Option<String>,
    },
    /// Where a project's alerts actually go.
    Channels {
        #[command(subcommand)]
        action: AlertChannelsCmd,
    },
}

#[derive(Subcommand)]
pub(crate) enum AlertChannelsCmd {
    /// The channels this project's alerts effectively reach: its own plus the inherited globals.
    List {
        #[arg(long)]
        project: String,
    },
    /// Add a routing destination. With `--signed` the signing secret is printed ONCE and is stored
    /// only as a digest — a lost one is replaced, never recovered.
    Set {
        #[arg(long)]
        project: String,
        /// The destination's transport: `webhook` | `ntfy` | `email`.
        #[arg(long = "type")]
        kind: String,
        /// The URL (webhook/ntfy) or address (email).
        #[arg(long)]
        target: String,
        /// Severity floor for this channel: `info` | `warning` | `critical` (default info).
        #[arg(long = "min-severity")]
        min_severity: Option<String>,
        /// An alert kind this channel wants. Repeatable; omit for every kind.
        #[arg(long = "kind")]
        kinds: Vec<String>,
        /// Store it muted: it is listed, and nothing is delivered to it.
        #[arg(long)]
        disabled: bool,
        /// Sign this channel's deliveries, minting the shared secret.
        #[arg(long)]
        signed: bool,
    },
    /// Remove one stored routing destination.
    Delete {
        #[arg(long)]
        project: String,
        id: String,
    },
    /// Send a REAL, signed test alert down one channel with this deployment's own credentials, and
    /// report whether it landed. Nothing is simulated: the destination receives it.
    Test { id: String },
}
