//! Evaluation verbs: rubrics, the dataset corpus and its lineage, human labels, and judge trust.

use clap::Subcommand;

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
pub(crate) enum DatasetsCmd {
    /// Every version of a dataset NAME, newest first — which corpus a past run was scored against.
    Versions {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
    },
    /// Fork a dataset into the next version of its name: items and their labels copied, unfrozen.
    ///
    /// The way a FROZEN golden set is extended. Writing to the frozen one would rewrite what a
    /// finished run was scored against; a fork leaves that run reproducible and moves `version`, so
    /// the paired-test guard can finally tell the two corpora apart.
    Fork {
        /// Dataset id to fork.
        id: String,
    },
    /// Mine stored rows into an unfrozen dataset by a declared sampling strategy.
    Import {
        /// Dataset id to import into (409 if it is frozen — fork it first).
        id: String,
        /// Where the cases come from: `events` (default) | `scores`.
        #[arg(long, default_value = "events")]
        from: String,
        /// How to choose them: `recent` | `random` | `stratified` | `errors`.
        #[arg(long, default_value = "recent")]
        strategy: String,
        #[arg(long, default_value_t = 50)]
        n: usize,
        /// With `--from scores`: only verdicts whose normalised value (`value/max`) is below this.
        /// Implies `--strategy errors` when no strategy was given.
        #[arg(long)]
        below: Option<f64>,
        /// Only events from this model.
        #[arg(long)]
        model: Option<String>,
        /// Only events with this outcome: `success` | `error` | `timeout`.
        #[arg(long)]
        status: Option<String>,
        /// Skip cases whose normalised input is already in the set.
        #[arg(long)]
        dedupe: bool,
    },
    /// Promote one labelled production event into a golden case, carrying the human verdict onto it.
    Promote {
        /// Dataset id to promote into (409 if it is frozen).
        id: String,
        /// The label to promote; its subject must be an event.
        #[arg(long = "label-id")]
        label_id: String,
    },
    /// Every human verdict on this set's items — the join `lt-runner calibrate --dataset` reads.
    Labels { id: String },
}

#[derive(Subcommand)]
pub(crate) enum LabelsCmd {
    /// List human verdicts, newest first.
    List {
        #[arg(long)]
        project: Option<String>,
        /// Narrow to one subject: `event:<id>` / `dataset_item:<id>` / `score:<id>`.
        #[arg(long)]
        subject: Option<String>,
        #[arg(long)]
        rubric_id: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Keyset cursor from a previous page's `next_cursor`. Without it you only ever see page one.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Record one verdict. `--labeler` is required: a human verdict with no attribution cannot be
    /// audited, which is how a calibration result becomes a number nobody can defend.
    Add {
        /// `event:<id>` / `dataset_item:<id>` / `score:<id>`.
        #[arg(long)]
        subject: String,
        /// Overall quality 0-1, on the same scale a judge verdict normalizes to.
        #[arg(long)]
        value: f64,
        /// Who said so.
        #[arg(long)]
        labeler: String,
        #[arg(long)]
        project: Option<String>,
        /// An explicit pass/fail call; omit to derive it from `--value`.
        #[arg(long)]
        pass: Option<bool>,
        /// The rubric this opinion was formed under, if any.
        #[arg(long)]
        rubric_id: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum JudgesCmd {
    /// Is this judge trusted for this rubric? `trusted` | `untrusted` | `unknown` — and `unknown`
    /// is a real answer, not a missing one: nobody has measured this pair.
    Trust {
        /// The judge model, e.g. `anthropic/claude-haiku-4-5`.
        judge: String,
        #[arg(long)]
        project: Option<String>,
        /// Omit for the freeform judge; a rubric never inherits that trust.
        #[arg(long)]
        rubric_id: Option<String>,
    },
    /// The project's calibration history, newest first — the series a drift check reads.
    History {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        /// Keyset cursor from a previous page's `next_cursor`. Without it you only ever see page one.
        #[arg(long)]
        cursor: Option<String>,
    },
    /// Record a finished judge-human calibration — the row a trust verdict is decided from.
    Calibrate {
        /// Path to the CalibrationRecord JSON (`judge`, `kappa`, `pearson`, `mae`, `rmse`, `n`,
        /// `kappa_bar`, `trusted`, and optionally `rubric_id` / `dataset_id`).
        #[arg(long)]
        file: String,
        /// Project id — required with an admin key, which cannot derive one.
        #[arg(long)]
        project: Option<String>,
    },
}
