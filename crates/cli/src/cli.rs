//! Command-line interface (clap).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "lt", about = "LightTrack operator CLI")]
pub(crate) struct Cli {
    #[arg(long, env = "LIGHTTRACK_URL", default_value = "http://127.0.0.1:8787")]
    pub(crate) base: String,
    #[arg(long, env = "LIGHTTRACK_KEY")]
    pub(crate) key: Option<String>,
    /// Print raw JSON instead of the rendered table view (also implied when stdout is piped).
    #[arg(long, global = true)]
    pub(crate) json: bool,
    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand)]
pub(crate) enum Cmd {
    /// Manage projects.
    Projects {
        #[command(subcommand)]
        action: ProjectsCmd,
    },
    /// Manage API keys.
    Keys {
        #[command(subcommand)]
        action: KeysCmd,
    },
    /// Manage and inspect limit rules.
    Limits {
        #[command(subcommand)]
        action: LimitsCmd,
    },
    /// Cost/usage rollup.
    Costs {
        #[arg(long)]
        project: Option<String>,
    },
    /// Recent events.
    Events {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Recent agent traces (events grouped by trace_id): end-to-end cost, latency, tokens, spans.
    Traces {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One trace by id: rolled-up totals, the span tree, and any scores within it.
    Trace {
        id: String,
    },
    /// Profit margin: revenue − LLM cost by customer or product (default window: last 30 days).
    Margin {
        #[arg(long, default_value = "customer")]
        by: String,
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default 30d ago).
        #[arg(long)]
        since: Option<String>,
        /// RFC3339 window end (default now).
        #[arg(long)]
        until: Option<String>,
    },
    /// Collective Model Intelligence: the shared real-world model leaderboard (network effect).
    Collective {
        #[command(subcommand)]
        action: CollectiveCmd,
    },
}

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
    Contribute {
        /// Base URL of the hub that accepts contributions (its API).
        #[arg(long)]
        hub: String,
        #[arg(long = "min-cases", default_value_t = 5)]
        min_cases: u32,
        /// Optional bearer key for the hub (if it runs in enforced auth mode).
        #[arg(long = "hub-key")]
        hub_key: Option<String>,
    },
    /// Withdraw everything this instance contributed to a hub (the right to leave the network).
    Withdraw {
        /// Base URL of the hub holding the contribution.
        #[arg(long)]
        hub: String,
        /// The contributor key this instance contributes with — the hub identifies the source by it.
        #[arg(long = "hub-key")]
        hub_key: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum ProjectsCmd {
    Create {
        #[arg(long)]
        name: String,
    },
    List,
}

#[derive(Subcommand)]
pub(crate) enum KeysCmd {
    Create {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "default")]
        name: String,
    },
}

#[derive(Subcommand)]
pub(crate) enum LimitsCmd {
    Set {
        #[arg(long)]
        project: String,
        #[arg(long)]
        metric: String,
        #[arg(long)]
        window: String,
        #[arg(long)]
        threshold: f64,
        #[arg(long, default_value = "alert")]
        action: String,
        /// Create the rule disabled (it won't enforce or alert until toggled on).
        #[arg(long)]
        disabled: bool,
        /// Soft-warning fraction in (0,1): alert when usage reaches this share of the threshold.
        #[arg(long = "warn-at")]
        warn_at: Option<f64>,
        /// Scope the cap to one provider (mutually exclusive with --scope-model/--scope-name).
        #[arg(long = "scope-provider", group = "scope")]
        scope_provider: Option<String>,
        /// Scope the cap to one model.
        #[arg(long = "scope-model", group = "scope")]
        scope_model: Option<String>,
        /// Scope the cap to one use-case (event `name`).
        #[arg(long = "scope-name", group = "scope")]
        scope_name: Option<String>,
    },
    /// Replace a rule's fields by id (also toggles enable/disable via --disabled).
    Update {
        id: String,
        #[arg(long)]
        metric: String,
        #[arg(long)]
        window: String,
        #[arg(long)]
        threshold: f64,
        #[arg(long, default_value = "alert")]
        action: String,
        #[arg(long)]
        disabled: bool,
        #[arg(long = "warn-at")]
        warn_at: Option<f64>,
        #[arg(long = "scope-provider", group = "scope")]
        scope_provider: Option<String>,
        #[arg(long = "scope-model", group = "scope")]
        scope_model: Option<String>,
        #[arg(long = "scope-name", group = "scope")]
        scope_name: Option<String>,
    },
    /// Delete a rule by id.
    Delete {
        id: String,
    },
    List {
        #[arg(long)]
        project: String,
    },
    Status {
        #[arg(long)]
        project: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    /// clap's own consistency check — catches a malformed `#[arg]`/`group` at test time rather than
    /// on the operator's first run.
    #[test]
    fn command_tree_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn defaults_match_the_documented_surface() {
        let cli = Cli::try_parse_from(["lt", "events"]).expect("parse");
        assert_eq!(cli.base, "http://127.0.0.1:8787");
        assert!(!cli.json);
        match cli.cmd {
            Cmd::Events { project, limit } => {
                assert_eq!(project, None);
                assert_eq!(limit, 20);
            }
            _ => panic!("wrong subcommand"),
        }
    }

    /// `--json` is `global = true`, so it must be accepted after the subcommand too — that is how
    /// operators actually type it.
    #[test]
    fn json_flag_is_global() {
        let cli = Cli::try_parse_from(["lt", "costs", "--json"]).expect("parse");
        assert!(cli.json);
    }

    /// The three `--scope-*` flags share one clap `group`, which is what makes them mutually
    /// exclusive; `scope_json` relies on at most one being set.
    #[test]
    fn scope_flags_are_mutually_exclusive() {
        let ok = Cli::try_parse_from([
            "lt", "limits", "set", "--project", "p", "--metric", "cost_usd", "--window", "day",
            "--threshold", "5", "--scope-model", "m",
        ]);
        assert!(ok.is_ok());
        let clash = Cli::try_parse_from([
            "lt", "limits", "set", "--project", "p", "--metric", "cost_usd", "--window", "day",
            "--threshold", "5", "--scope-model", "m", "--scope-provider", "openai",
        ]);
        assert!(clash.is_err());
    }
}
