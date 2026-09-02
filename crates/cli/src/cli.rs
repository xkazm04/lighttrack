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
    /// Manage rubrics — the weighted, anchored contract the LLM judge scores against.
    Rubrics {
        #[command(subcommand)]
        action: RubricsCmd,
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
    Trace { id: String },
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
    /// Collective Model Intelligence: the shared real-world model leaderboard (network effect).
    Collective {
        #[command(subcommand)]
        action: CollectiveCmd,
    },
}

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
        /// Choose the project id (1–64 chars: letter/digit first, then letters, digits, `-`, `_`,
        /// `.`). This is the id you put in `LIGHTTRACK_PROJECT` and in URLs. Omit it for a UUID.
        #[arg(long)]
        id: Option<String>,
    },
    List,
}

#[derive(Subcommand)]
pub(crate) enum RubricsCmd {
    /// Create a rubric from a JSON file: either the whole body
    /// (`{"name","threshold","dimensions"}`) or a bare array of dimensions plus `--name`.
    Create {
        #[arg(long)]
        project: String,
        /// Path to the rubric JSON.
        #[arg(long)]
        file: String,
        /// Rubric name — supplies or overrides `name` in the file.
        #[arg(long)]
        name: Option<String>,
        /// Overall pass threshold 0–1 — supplies or overrides `threshold` (API default 0.7).
        #[arg(long)]
        threshold: Option<f64>,
    },
    /// List a project's rubrics (name, dimension count, threshold, id).
    List {
        #[arg(long)]
        project: String,
    },
    /// Show one rubric by id: its dimensions, weights and gating floors.
    Show { id: String },
}

#[derive(Subcommand)]
pub(crate) enum KeysCmd {
    Create {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "default")]
        name: String,
        /// What the key may do: `ingest`, `read`, `manage`. Repeatable. Omitted ⇒ the server's
        /// back-compat default (`ingest` + `read`); a key shipped inside a client app should be
        /// `--scope ingest` so it cannot read the project's stored prompts back.
        #[arg(long = "scope")]
        scopes: Vec<String>,
        /// Hard expiry, RFC3339 (e.g. `2027-01-01T00:00:00Z`). Past it the key stops working.
        #[arg(long)]
        expires: Option<String>,
    },
    /// List a project's keys with their scopes, expiry, last use and revocation state.
    List {
        #[arg(long)]
        project: String,
    },
    /// Mint a successor with the same name and scopes, and give this key a deadline instead of
    /// killing it — so a fleet still holding the old secret has a window to redeploy.
    Rotate {
        #[arg(long)]
        project: String,
        /// The key id to rotate (from `lt keys list`).
        id: String,
        /// How long the old key keeps working. `0` retires it at once.
        #[arg(long = "grace-secs")]
        grace_secs: Option<i64>,
    },
    /// Revoke a key immediately (soft — the row is kept for audit).
    Revoke {
        #[arg(long)]
        project: String,
        id: String,
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
    Delete { id: String },
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
}
