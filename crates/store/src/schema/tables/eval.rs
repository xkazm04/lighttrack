//! Verdicts, rubrics, benchmarks and the eval corpus.

use super::super::model::{Column as C, Index as I, Kind::*, Table};

pub static SCORES: Table = Table::new(
    "scores",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("event_id", Text),
        C::new("rubric", Text).nn(),
        C::new("value", Real).nn(),
        C::new("max", Real).nn().def("1.0").quoted_pg(),
        C::new("pass", Int),
        C::new("reasoning", Text),
        C::new("detail", Json).added("M9").doc(
            "Structured verdict provenance (core::ScoreDetail) as JSON: per-dimension {value, \
             weight, floor_hit, floor_hits/floor_of, reasoning[]}, agreement, sample accounting, \
             bias/injection flags. NULL for scores posted without it. Bounded by \
             ScoreDetail::capped() at the API boundary.",
        ),
        C::new("run_id", Text).added("M9").doc(
            "The benchmark run that produced this verdict (NULL for online/ad-hoc scores). With \
             `case_index` it makes \"every case result for run X\" a query instead of a \
             created_at guess.",
        ),
        C::new("case_index", Int)
            .added("M9")
            .doc("the 1-based case position within that run"),
        C::new("scored_by", Text).nn(),
        C::new("cost_usd", Real),
        C::new("created_at", Ts).nn(),
        C::new("rubric_id", Text).added("M9").doc(
            "Typed verdict identity. `scores.rubric` is one free-text column carrying six \
             encodings, so nothing downstream could tell a benchmark case from a calibration probe \
             without parsing a string — and the alerting window keyed on that string, which made \
             every compare cell a unique key that never accumulated. The legacy label stays \
             verbatim beside these.",
        ),
        C::new("kind", Text).added("M9"),
    ],
)
.indexes(&[
    I::new("idx_scores_project", "project_id, created_at"),
    I::new("idx_scores_event", "event_id").doc(
        "Probe scores by the event they judged: powers the trace-scores join and the online \
         scorer's unscored-events anti-join. Without it both full-scan `scores`.",
    ),
    I::new("idx_scores_run", "run_id, case_index, created_at")
        .doc("Run-scoped case results, already in the listing's order."),
    I::new("idx_scores_rubric_id", "rubric_id, created_at"),
    I::new("idx_scores_kind", "kind, created_at"),
    I::new("idx_scores_created", "created_at").doc(
        "The quality read joins scores to events by event_id and windows on the VERDICT's \
         created_at; without this the join degrades to a scan of the scores table per window.",
    ),
])
.bq("DATE(created_at)", "project_id, rubric");

pub static BENCHMARKS: Table = Table::new(
    "benchmarks",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("name", Text).nn(),
        C::new("rubric", Text).nn(),
        C::new("judge_model", Text).nn(),
        C::new("target", Json),
        C::new("dataset_ref", Text),
        C::new("dataset", Json).doc("JSON array of {input, expected?, output?}"),
        C::new("rubric_id", Text).doc("optional structured rubric for per-dimension judging"),
        C::new("baseline_score", Real),
        C::new("created_at", Ts).nn(),
    ],
);

pub static RUBRICS: Table = Table::new(
    "rubrics",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("name", Text).nn(),
        C::new("dimensions", Json)
            .nn()
            .doc("JSON array of {key, description, weight, anchors, floor?}"),
        C::new("threshold", Real).nn().def("0.7"),
        C::new("created_at", Ts).nn(),
        C::new("version", Int32).nn().def("1").added("M9").doc(
            "A rubric edit changes what a score *means*, and nothing recorded that one had \
             happened. A new version is a new row linked to the old one, never a mutation of it.",
        ),
        C::new("supersedes", Text).added("M9"),
    ],
)
.doc("Weighted, anchored rubrics.");

pub static BENCHMARK_RUNS: Table = Table::new(
    "benchmark_runs",
    &[
        C::new("id", Text).pk(),
        C::new("benchmark_id", Text).nn().refs("benchmarks(id)"),
        C::new("started_at", Ts).nn(),
        C::new("finished_at", Ts),
        C::new("n_cases", Int).nn().def("0"),
        C::new("mean_score", Real),
        C::new("pass_rate", Real),
        C::new("cost_usd", Real),
        C::new("status", Text).nn().def("'running'"),
        C::new("p50_latency_ms", Int),
        C::new("p95_latency_ms", Int),
        C::new("total_tokens", Int),
        C::new("report", Json),
    ],
);

pub static DATASETS: Table = Table::new(
    "datasets",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("name", Text).nn(),
        C::new("version", Int).nn().def("1"),
        C::new("frozen", Int).nn().def("0"),
        C::new("source", Text),
        C::new("created_at", Ts).nn(),
        C::new("parent_id", Text).added("M24").doc(
            "The link that makes `version` mean something: a v2 with no parent is just another row \
             that shares a name. Nullable — a pre-M24 dataset is a v1 with no parent.",
        ),
    ],
)
.doc("Versioned evaluation datasets, built by hand or sampled from real events.")
.indexes(&[I::new("idx_datasets_name_version", "project_id, name, version").doc(
    "The version walk and the fork's \"what is the highest version this name already has\" read.",
)]);

pub static DATASET_ITEMS: Table = Table::new(
    "dataset_items",
    &[
        C::new("id", Text).pk(),
        C::new("dataset_id", Text).nn().refs("datasets(id)"),
        C::new("input", Text).nn(),
        C::new("output", Text),
        C::new("expected", Text),
        C::new("context", Text),
        C::new("tags", Json).doc("JSON array"),
        C::new("source_event_id", Text),
        C::new("anonymization", Json).doc("JSON {method, redactions}"),
        C::new("input_hash", Text).added("M24").doc(
            "The normalised-input fingerprint near-duplicate collapse looks up instead of scanning \
             every stored case's text. Nullable, and dedupe treats NULL as \"no match\".",
        ),
    ],
)
.indexes(&[
    I::new("idx_dataset_items_ds", "dataset_id"),
    I::new("idx_dataset_items_hash", "dataset_id, input_hash")
        .doc("Dedupe's lookup: the fingerprints already in the target set."),
]);

pub static LABELS: Table = Table::new(
    "labels",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("subject_kind", Text)
            .nn()
            .doc("event | dataset_item | score"),
        C::new("subject_id", Text).nn(),
        C::new("rubric_id", Text).doc("NULL = a general quality opinion"),
        C::new("value", Real)
            .nn()
            .doc("0..1, comparable with scores.value/max"),
        C::new("pass", Bool).doc("an explicit human call; NULL = derive from value"),
        C::new("dimensions", Json).doc("JSON array of ScoreDim; read whole, never filtered"),
        C::new("labeler", Text)
            .nn()
            .doc("who said so: what makes a result auditable"),
        C::new("note", Text),
        C::new("created_at", Ts).nn(),
    ],
)
.doc(
    "What a person said about one subject (M11). The subject is TWO columns rather than one \
     \"kind:id\" string, for the reason M9 split `scores.rubric`: a column carrying several \
     encodings is a column nothing can index or join on.",
)
.indexes(&[
    I::new("idx_labels_subject", "subject_kind, subject_id"),
    I::new("idx_labels_project", "project_id, created_at"),
    I::new("idx_labels_rubric", "rubric_id, created_at"),
]);

pub static CALIBRATIONS: Table = Table::new(
    "calibrations",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("judge", Text).nn().doc("canonical [provider/]model"),
        C::new("rubric_id", Text).doc("NULL = the freeform calibration; matched IS NULL"),
        C::new("dataset_id", Text),
        C::new("dataset_version", Int32),
        C::new("kappa", Real).nn(),
        C::new("pearson", Real).nn(),
        C::new("mae", Real).nn(),
        C::new("rmse", Real).nn(),
        C::new("n", Int32)
            .nn()
            .doc("so trust on 12 cases reads as trust on 12 cases"),
        C::new("kappa_bar", Real).nn().doc(
            "stored beside `kappa` so raising the bar later cannot silently re-verdict what was \
             already measured",
        ),
        C::new("trusted", Bool).nn(),
        C::new("created_at", Ts).nn(),
    ],
)
.doc("One completed judge↔human calibration (M11). Append-only: a re-measurement is a new row.")
.indexes(&[I::new(
    "idx_calibrations_key",
    "project_id, judge, rubric_id, created_at",
)
.doc("The lookup every gate makes: exactly one (project, judge, rubric) pair, newest first.")]);
