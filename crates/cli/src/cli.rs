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
    /// Cost/usage rollup.
    Costs {
        #[arg(long)]
        project: Option<String>,
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

#[derive(Subcommand)]
pub(crate) enum RelayCmd {
    /// The enrolled device fleet.
    Devices {
        #[command(subcommand)]
        action: RelayDevicesCmd,
    },
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

#[derive(Subcommand)]
pub(crate) enum PricesCmd {
    /// The rates in force today, one row per model.
    List,
    /// Models carrying traffic the price book could not cost, loudest first.
    ///
    /// While this list is non-empty, every cost, margin, forecast and limit number over the window
    /// is a FLOOR — those calls are stored with no cost at all, never a zero. Close a row with
    /// `PUT /v1/prices/<provider>/<model>?fill_unpriced=1`.
    Unpriced {
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default: 30 days ago).
        #[arg(long)]
        since: Option<String>,
    },
    /// Every stored rate for one model, newest first — what a call in a past window really cost.
    History { provider: String, model: String },
}

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
    },
    /// Acknowledge one alert: record that a human saw it.
    Ack {
        id: String,
        /// Who saw it — an on-call handle, an email, a runbook link. Defaults server-side to the
        /// calling key's label.
        #[arg(long)]
        by: Option<String>,
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
    /// Mint the next generation of a rubric: a copy-with-changes under a NEW id, linked to the old.
    ///
    /// Not an edit. Verdicts already stored cite the old rubric's id, and rewriting that row would
    /// silently change what those verdicts claim to have measured. Omit `--file` to carry the
    /// dimensions forward unchanged (e.g. to move only the threshold).
    Version {
        /// The rubric to supersede.
        id: String,
        /// Path to the new dimensions JSON (whole body or a bare array). Omitted ⇒ unchanged.
        #[arg(long)]
        file: Option<String>,
        /// New pass threshold 0–1. Omitted ⇒ carried forward from the superseded rubric.
        #[arg(long)]
        threshold: Option<f64>,
    },
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
