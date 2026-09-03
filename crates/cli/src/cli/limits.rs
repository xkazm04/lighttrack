//! Usage governance verbs: the cap rules, and the standing margin policies that mint them.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum LimitsCmd {
    /// Add a usage cap to a project. Monitored ingest traffic only — the judge is never budgeted.
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
    /// A project's limit rules, enabled and disabled alike.
    List {
        #[arg(long)]
        project: String,
    },
    /// Where a project stands against its caps right now, and how much of that rests on weak cost
    /// evidence.
    Status {
        #[arg(long)]
        project: String,
    },
    /// Who is spending: rolling usage broken down by one scope dimension, with the rules that bind.
    Usage {
        #[arg(long)]
        project: Option<String>,
        /// The dimension to group by: `api_key` | `customer` | `model` | `provider` | `name`.
        #[arg(long, default_value = "api_key")]
        by: String,
        /// Rolling window: `hour` | `day` | `month`.
        #[arg(long, default_value = "day")]
        window: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
}

#[derive(Subcommand)]
pub(crate) enum MarginPoliciesCmd {
    /// Store a standing margin guardrail; the forecast sweep is what turns it into limit rules.
    Create {
        #[arg(long)]
        project: String,
        /// The margin condition that arms the policy, as JSON, e.g. '{"margin_pct_below":0}'.
        #[arg(long)]
        trigger: String,
        /// The limit rule it creates when armed, as JSON.
        #[arg(long)]
        action: String,
        /// Windowed cost a subject must exceed before the policy acts on it.
        #[arg(long = "min-cost-usd")]
        min_cost_usd: Option<f64>,
        /// Gap between actions on the same subject (default 3600).
        #[arg(long = "cooldown-secs")]
        cooldown_secs: Option<i64>,
        /// How long a rule this policy creates lives (default 86400).
        #[arg(long = "expiry-secs")]
        expiry_secs: Option<i64>,
        /// Store it disarmed; it mints nothing until enabled.
        #[arg(long)]
        disabled: bool,
    },
    /// A project's standing margin guardrails.
    List {
        #[arg(long)]
        project: String,
    },
    /// Remove a margin policy; the rules it already created are reaped by the sweep, not here.
    Delete {
        #[arg(long)]
        project: String,
        id: String,
    },
}
