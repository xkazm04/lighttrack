//! The prompt registry's verbs: what is registered, what it is scoring, and how a version is cut.

use clap::Subcommand;

#[derive(Subcommand)]
pub(crate) enum PromptsCmd {
    /// Registry entries with their label→version pointers and linked benchmark.
    List {
        #[arg(long)]
        project: String,
    },
    /// Per-served-version quality: mean, pass rate, ~95% interval and n for every
    /// `metadata.prompt` tag. The quality half of `lt costs` — read `n` before the mean.
    Quality {
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 lower bound on the VERDICT time (default: 7 days ago).
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
        /// Narrow to one rubric — the only way two versions are compared on the same criteria.
        #[arg(long)]
        rubric_id: Option<String>,
    },
    /// Register a new prompt with its version 1; a name already in the registry is a 409.
    Create {
        #[arg(long)]
        project: String,
        /// Registry prompt name, unique within the project.
        #[arg(long)]
        name: String,
        /// Path to the file holding version 1's text.
        #[arg(long)]
        file: String,
        /// Structured config as JSON (model, params, variable schema).
        #[arg(long)]
        config: Option<String>,
        /// Change note for version 1.
        #[arg(long)]
        note: Option<String>,
        /// The benchmark whose regression check gates this prompt's promotions.
        #[arg(long = "benchmark-id")]
        benchmark_id: Option<String>,
    },
    /// Point a prompt at the benchmark whose regression check gates its promotions.
    Link {
        #[arg(long)]
        project: String,
        name: String,
        /// The gating benchmark. Omit to unlink, which reopens promotion to an ungated one.
        #[arg(long = "benchmark-id")]
        benchmark_id: Option<String>,
    },
    /// Every stored version of one registry prompt.
    Versions {
        #[arg(long)]
        project: String,
        name: String,
    },
    /// Set or clear the online canary policy; a policy that could never fire is refused.
    Canary {
        #[arg(long)]
        project: String,
        name: String,
        /// Path to the CanaryPolicy JSON. Omit with `--clear` to remove the policy entirely.
        #[arg(long, conflicts_with = "clear")]
        file: Option<String>,
        /// Remove the stored policy: every request goes back to the label's own version.
        #[arg(long)]
        clear: bool,
    },
}
