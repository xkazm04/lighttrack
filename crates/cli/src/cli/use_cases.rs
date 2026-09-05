//! The use-case registry — the declared inventory of where this project calls an LLM.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum UseCasesCmd {
    /// Register a call site, or correct one already registered.
    ///
    /// Idempotent on `(project, key)`: registering the same key again is a correction, not a
    /// conflict, so fixing a description never means deleting the row the events attribute to.
    Register {
        #[arg(long)]
        project: String,
        /// Stable identifier the SDK puts in `events.name`. Compared exactly, so no spaces.
        #[arg(long)]
        key: String,
        /// Human title for a dashboard row.
        #[arg(long)]
        name: String,
        /// What this call site is for, in a sentence.
        #[arg(long)]
        description: Option<String>,
        /// generation | classification | extraction | summarization | judge | agent | embedding |
        /// rerank | other. Splits calls that are not comparable on latency, price or quality.
        #[arg(long)]
        kind: Option<String>,
        /// active | planned | deprecated. Decides whether silence or traffic is the finding.
        #[arg(long)]
        status: Option<String>,
        /// Where in the application this call site lives (a module, route or service).
        #[arg(long)]
        component: Option<String>,
        /// A model this call site is MEANT to run, repeatable. Declaring none is not the same as
        /// "any model is fine": with none declared, the coverage report reports no drift rather
        /// than inventing a violation out of an absence.
        #[arg(long = "expected-model")]
        expected_models: Vec<String>,
        #[arg(long)]
        owner: Option<String>,
    },
    /// List the call sites this project has declared.
    List {
        #[arg(long)]
        project: String,
    },
    /// Show one declared call site.
    Show {
        #[arg(long)]
        project: String,
        key: String,
    },
    /// What was DECLARED against what the events actually did.
    ///
    /// The read the registry exists for: shadow usage (traffic nobody declared, or a typo splitting
    /// one use case's cost in two), quiet use cases, models running that nobody chose, and the
    /// calls carrying no use case at all.
    Coverage {
        #[arg(long)]
        project: String,
        /// RFC3339 lower bound on the event timestamp. Omit for all history.
        #[arg(long)]
        since: Option<String>,
    },
    /// Remove a registration. The events that referenced it are untouched — they simply become
    /// shadow usage again, which is the honest state.
    Delete {
        #[arg(long)]
        project: String,
        key: String,
    },
}
