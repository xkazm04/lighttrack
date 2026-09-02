//! Command-line interface (clap).
//!
//! `Cli` and the top-level `Cmd` live here; each domain's subcommand enums live in a sibling module
//! and are re-exported, so every handler still says `crate::cli::{Cli, XCmd}`. The split is what
//! keeps one file from owning the whole verb tree as the contract's endpoint table grows.

mod alerts;
mod collective;
mod evals;
mod limits;
mod projects;
mod prompts;
mod relay;
mod usage;
mod work;

pub(crate) use alerts::{AlertChannelsCmd, AlertsCmd};
pub(crate) use collective::CollectiveCmd;
pub(crate) use evals::{DatasetsCmd, JudgesCmd, LabelsCmd, RubricsCmd};
pub(crate) use limits::{LimitsCmd, MarginPoliciesCmd};
pub(crate) use projects::{KeysCmd, ProjectsCmd};
pub(crate) use prompts::PromptsCmd;
pub(crate) use relay::{
    RelayActionsArgs, RelayActionsCmd, RelayCmd, RelayDevicesCmd, RelayTasksCmd,
};
pub(crate) use usage::{
    CostsArgs, CostsCmd, IngestCmd, MarginArgs, MarginCmd, PricesCmd, RevenueCmd, StorageCmd,
};
pub(crate) use work::{JobsCmd, SchedulesCmd};

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
    /// Liveness, and the store backend's declared surfaces. Needs no key.
    Health,
    /// This deployment's OpenAPI 3.1 description, generated from the endpoint contract.
    Openapi,
    /// What this deployment's store backend serves, and what it answers 501 `unsupported` for.
    Capabilities,
    /// The ingest doors' load-shedding view.
    Ingest {
        #[command(subcommand)]
        action: IngestCmd,
    },
    /// What the store is costing on disk, and what maintenance has actually run.
    Storage {
        #[command(subcommand)]
        action: StorageCmd,
    },
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
    /// Standing margin guardrails: the policies that turn an eroding customer into a limit rule.
    #[command(name = "margin-policies")]
    MarginPolicies {
        #[command(subcommand)]
        action: MarginPoliciesCmd,
    },
    /// The fired-alert ledger: what fired, whether it was delivered, and who acknowledged it.
    Alerts {
        #[command(subcommand)]
        action: AlertsCmd,
    },
    /// Manage rubrics — the weighted, anchored contract the LLM judge scores against.
    Rubrics {
        #[command(subcommand)]
        action: RubricsCmd,
    },
    /// Eval corpus lineage (M24) — a dataset name's versions, forking one, mining rows into one.
    Datasets {
        #[command(subcommand)]
        action: DatasetsCmd,
    },
    /// Human verdicts (M11) — the ground truth a judge is calibrated against.
    Labels {
        #[command(subcommand)]
        action: LabelsCmd,
    },
    /// Judge trust — may the judge behind a green badge be believed for this rubric?
    Judges {
        #[command(subcommand)]
        action: JudgesCmd,
    },
    /// Cost/usage rollup.
    Costs {
        #[command(flatten)]
        args: CostsArgs,
        #[command(subcommand)]
        action: Option<CostsCmd>,
    },
    /// The grouped cost/usage primitive every fixed cost surface is one grouping of.
    Rollup {
        #[arg(long)]
        project: Option<String>,
        /// 1–3 comma-separated dimensions (default provider,model):
        /// project|provider|model|name|api_key|customer|product|prompt|day.
        #[arg(long, default_value = "provider,model")]
        by: String,
        /// RFC3339 window start (default 30d ago).
        #[arg(long)]
        since: Option<String>,
        /// RFC3339 window end (exclusive).
        #[arg(long)]
        until: Option<String>,
        /// Which timestamp the window and `day` bucket read: `ts` (default) or `received_at`.
        #[arg(long)]
        time: Option<String>,
        /// Comma-separated `dimension:value` equality predicates, e.g. `customer:acme,model:gpt-5.4`.
        #[arg(long)]
        filter: Option<String>,
    },
    /// Where spend and margin are heading: budget-breach ETAs and margin-erosion crossovers.
    ///
    /// A projection under the evidence floor is withheld and named in `refused[]`, so an empty
    /// `alerts` beside a non-empty `refused` means "not enough history", not "all is well".
    Forecast {
        /// Required with an admin key; a project key derives it.
        #[arg(long)]
        project: Option<String>,
        /// Margin dimension: `customer` (default) | `product`.
        #[arg(long, default_value = "customer")]
        by: String,
        /// Days to project ahead (default 14, 1..=90).
        #[arg(long)]
        horizon: Option<i64>,
        /// Trailing days of history to fit (default 14, clamped to 4..=90).
        #[arg(long)]
        lookback: Option<i64>,
    },
    /// The prompt registry, and how the versions it serves are actually scoring.
    Prompts {
        #[command(subcommand)]
        action: PromptsCmd,
    },
    /// The model price book: what is priced, what is not, and what a rate used to be.
    Prices {
        #[command(subcommand)]
        action: PricesCmd,
    },
    /// Recent events.
    Events {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Keyset cursor from a previous page's `X-Next-Cursor`. Without it you only ever see
        /// page one.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Recent agent traces (events grouped by trace_id): end-to-end cost, latency, tokens, spans.
    Traces {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        /// Keyset cursor from a previous page's `X-Next-Cursor`. Without it you only ever see
        /// page one.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// One trace by id: rolled-up totals, the span tree, and any scores within it.
    Trace { id: String },
    /// Profit margin: revenue − LLM cost by customer or product (default window: last 30 days).
    Margin {
        #[command(flatten)]
        args: MarginArgs,
        #[command(subcommand)]
        action: Option<MarginCmd>,
    },
    /// Recognized revenue — the half of the margin subtraction LightTrack does not observe.
    Revenue {
        #[command(subcommand)]
        action: RevenueCmd,
    },
    /// Recurring workloads: what this instance runs on a schedule, and how often.
    Schedules {
        #[command(subcommand)]
        action: SchedulesCmd,
    },
    /// The background job queue: what is queued, running, or finished.
    Jobs {
        #[command(subcommand)]
        action: JobsCmd,
    },
    /// Restate revenue stored at the 1:1 FX fallback, once a missing rate has been added.
    ///
    /// Previews by default. Adding a rate to config/fx_rates.json fixes future syncs only; the rows
    /// already stored at 1:1 stay wrong until this runs.
    Reprice {
        /// ISO-4217 code to restate, e.g. GBP.
        #[arg(long)]
        currency: String,
        #[arg(long)]
        project: Option<String>,
        /// USD per one major unit. Defaults to the server's current FX book.
        #[arg(long)]
        rate: Option<f64>,
        /// Actually write. Without it this reports what would change and touches nothing.
        #[arg(long)]
        apply: bool,
    },
    /// Collective Model Intelligence: the shared real-world model leaderboard (network effect).
    Collective {
        #[command(subcommand)]
        action: CollectiveCmd,
    },
    /// The cloud→device relay: which devices are enrolled, and what each can run.
    Relay {
        #[command(subcommand)]
        action: RelayCmd,
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
            Cmd::Events {
                project,
                limit,
                cursor,
            } => {
                assert_eq!(project, None);
                assert_eq!(limit, 20);
                assert_eq!(cursor, None);
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
            "lt",
            "limits",
            "set",
            "--project",
            "p",
            "--metric",
            "cost_usd",
            "--window",
            "day",
            "--threshold",
            "5",
            "--scope-model",
            "m",
        ]);
        assert!(ok.is_ok());
        let clash = Cli::try_parse_from([
            "lt",
            "limits",
            "set",
            "--project",
            "p",
            "--metric",
            "cost_usd",
            "--window",
            "day",
            "--threshold",
            "5",
            "--scope-model",
            "m",
            "--scope-provider",
            "openai",
        ]);
        assert!(clash.is_err());
    }

    /// `margin`, `costs` and `relay actions` each gained a subcommand alongside their own flags.
    /// The bare form is the one operators already have in scripts, so it must keep parsing exactly
    /// as it did — and reach the parent, not a subcommand.
    #[test]
    fn a_parent_with_subcommands_still_parses_its_own_flags() {
        let cli = Cli::try_parse_from(["lt", "margin", "--by", "product"]).expect("parse");
        match cli.cmd {
            Cmd::Margin { args, action } => {
                assert_eq!(args.by, "product");
                assert!(action.is_none());
            }
            _ => panic!("wrong subcommand"),
        }
        let cli = Cli::try_parse_from(["lt", "margin", "trend", "--days", "7"]).expect("parse");
        match cli.cmd {
            Cmd::Margin { action, .. } => match action.expect("a subcommand") {
                MarginCmd::Trend { days, .. } => assert_eq!(days, Some(7)),
                _ => panic!("wrong margin subcommand"),
            },
            _ => panic!("wrong subcommand"),
        }
        assert!(Cli::try_parse_from(["lt", "costs", "--project", "p"]).is_ok());
        assert!(Cli::try_parse_from(["lt", "costs", "prompts", "--project", "p"]).is_ok());
        assert!(Cli::try_parse_from(["lt", "relay", "actions", "--limit", "50"]).is_ok());
    }
}
