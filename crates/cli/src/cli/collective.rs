//! Collective Model Intelligence: the shared, opt-in real-world model leaderboard.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum CollectiveCmd {
    /// Show the merged public leaderboard (quality × cost × latency across contributors).
    Leaderboard {
        /// Filter to one task-type bucket (e.g. qa, summarization, coding).
        #[arg(long = "task-type")]
        task_type: Option<String>,
        #[arg(long)]
        provider: Option<String>,
        /// Filter to rows scored by one judge family (anthropic|openai|google|unknown).
        #[arg(long)]
        judge: Option<String>,
        /// Rigor filter: keep only rows where EVERY source ran at this determinism level
        /// (exact|best-effort|sampled).
        #[arg(long)]
        determinism: Option<String>,
        /// Rigor filter: keep only rows where every source ran against a frozen, single-version dataset.
        #[arg(long)]
        frozen: bool,
        /// Rigor filter: keep only rows where every source's verdict was significance-tested.
        #[arg(long)]
        tested: bool,
    },
    /// Preview this instance's privacy-safe digest — what `contribute` would publish (admin key).
    Digest {
        /// k-anonymity floor: only publish (model, task) buckets with at least this many cases.
        #[arg(long = "min-cases", default_value_t = 5)]
        min_cases: u32,
    },
    /// Build this instance's digest and contribute it to a leaderboard hub (opt-in).
    ///
    /// Goes through the API, which records the attempt in the contribution ledger and skips a push
    /// whose digest is unchanged since the last one that hub acked. `--direct` keeps the old
    /// two-hop client-side push, for an air-gapped hub the API itself cannot reach.
    Contribute {
        /// Base URL of the hub that accepts contributions (its API).
        #[arg(long)]
        hub: String,
        #[arg(long = "min-cases", default_value_t = 5)]
        min_cases: u32,
        /// Bearer key for the hub. Used by `--direct` only: on the ledgered path the API resolves
        /// its own key from `--hub-key-ref`, so a hub credential never travels through a request
        /// body or sits at rest in a schedule row.
        #[arg(long = "hub-key")]
        hub_key: Option<String>,
        /// The NAME of the server-side environment variable holding the hub key. Defaults to
        /// `LIGHTTRACK_COLLECTIVE_HUB_KEY`.
        #[arg(long = "hub-key-ref")]
        hub_key_ref: Option<String>,
        /// Push even when the digest is unchanged — for a hub that lost its database.
        #[arg(long)]
        force: bool,
        /// Push from this machine instead of from the API: `GET /digest` here, `POST /ingest`
        /// there. Nothing is recorded and nothing is hash-gated; for an air-gapped hub only.
        #[arg(long)]
        direct: bool,
    },
    /// What this instance has contributed: to which hub, when, and what the hub said back.
    History {
        /// Max rows (newest first).
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Keyset cursor from a previous page's `X-Next-Cursor`.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Withdraw everything this instance contributed to a hub (the right to leave the network).
    Withdraw {
        /// Base URL of the hub holding the contribution. Required unless `--all`.
        #[arg(long)]
        hub: Option<String>,
        /// The contributor key this instance contributes with — the hub identifies the source by it.
        #[arg(long = "hub-key")]
        hub_key: Option<String>,
        /// Withdraw from EVERY hub the contribution ledger says holds our data. Repeat `--hub` to
        /// name a hub the ledger only knows by hash; any it cannot name is reported, not skipped.
        #[arg(long)]
        all: bool,
        /// With `--all`: the NAME of the server-side env var holding the hub key.
        #[arg(long = "hub-key-ref")]
        hub_key_ref: Option<String>,
    },
}
