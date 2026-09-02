//! A frozen snapshot of the pre-M14 SQLite apply path.
//!
//! `schema_equivalence.rs` applies this to one fresh database and the model-rendered
//! plan to another, then compares `PRAGMA table_info` and `sqlite_master` between them.
//! It is deliberately a COPY: the point is to compare the new path against what shipped,
//! so this file must never be regenerated from the model it is checking.

pub const LEGACY_SCHEMA: &str = include_str!("sqlite_001_init_pre_m14.sql");

pub const LEGACY_ADDED_COLUMNS: &[&str] = &[
    // DBs created before `events.name` existed.
    "ALTER TABLE events ADD COLUMN name TEXT",
    // Additive migrations for limit rules created before the soft-warning tier and dimension
    // scoping existed.
    "ALTER TABLE limit_rules ADD COLUMN warn_at REAL",
    "ALTER TABLE limit_rules ADD COLUMN scope_kind TEXT",
    "ALTER TABLE limit_rules ADD COLUMN scope_value TEXT",
    // Collective digest v2: per-bucket quality variance (for merged CIs), plus the coarse judge
    // family and rubric-shape fingerprint that scored the bucket.
    "ALTER TABLE collective_entries ADD COLUMN quality_variance REAL",
    "ALTER TABLE collective_entries ADD COLUMN judge_provider TEXT",
    "ALTER TABLE collective_entries ADD COLUMN rubric_fingerprint TEXT",
    // Collective digest v3: benchmark rigor — how reproducible the contributing runs were, whether
    // their cases came from one frozen pin, and whether their verdicts were significance-tested.
    "ALTER TABLE collective_entries ADD COLUMN determinism TEXT",
    "ALTER TABLE collective_entries ADD COLUMN frozen_dataset TEXT",
    "ALTER TABLE collective_entries ADD COLUMN significance_tested TEXT",
    // Collective consent: per-project opt-in to digest contribution (default off).
    "ALTER TABLE projects ADD COLUMN collective_opt_in INTEGER NOT NULL DEFAULT 0",
    // Verdict provenance: structured judge detail (core::ScoreDetail) as JSON.
    "ALTER TABLE scores ADD COLUMN detail TEXT",
    // Run-scoped case results: which benchmark run produced this verdict, and where in its dataset.
    // `idx_scores_run` in [`SCHEMA`] indexes both, so these ALTERs MUST run before the batch —
    // see the ordering note in [`apply`].
    "ALTER TABLE scores ADD COLUMN run_id TEXT",
    "ALTER TABLE scores ADD COLUMN case_index INTEGER",
    // Honest failure accounting on the job queue: `failures` (runs that actually failed — the retry
    // budget) apart from `attempts` (claims, which a crash also burns), and `stale_reclaims` (worker
    // deaths). Both are read by the claim/finish statements, so a database created before they
    // existed must be widened here, before the batch.
    "ALTER TABLE jobs ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE jobs ADD COLUMN stale_reclaims INTEGER NOT NULL DEFAULT 0",
    // Tenancy lifecycle: what a key may do, when it stops working, and when a project was archived.
    // All three are nullable — an existing row carries no opinion, and `core::decode_scopes` reads
    // that as the permissive back-compat default rather than locking a live key out on upgrade.
    "ALTER TABLE api_keys ADD COLUMN scopes TEXT",
    "ALTER TABLE api_keys ADD COLUMN expires_at TEXT",
    "ALTER TABLE projects ADD COLUMN archived_at TEXT",
    // FX provenance on a revenue row (M9-B). `amount_usd` is derived and a wrong rate makes it
    // wrong; the provider's minor-unit figure never needs restating, so keeping it — with the rate,
    // the book version that produced it, and whether a real conversion happened — is what makes a
    // rate correction a reprice instead of a re-ingest. All nullable: a pre-M9 row carries no
    // opinion, and `RevenueEvent::is_converted` reads that as "base currency converted, others
    // unknown" rather than flagging every historical USD invoice as approximate.
    "ALTER TABLE revenue_events ADD COLUMN amount_minor INTEGER",
    "ALTER TABLE revenue_events ADD COLUMN fx_rate REAL",
    "ALTER TABLE revenue_events ADD COLUMN fx_book_version TEXT",
    "ALTER TABLE revenue_events ADD COLUMN converted INTEGER",
    // C. Typed verdict identity (M9-C). `scores.rubric` is one free-text column carrying six
    // encodings, so nothing downstream could tell a benchmark case from a calibration probe without
    // parsing a string — and the alerting window keyed on that string, which made every compare cell
    // a unique key that never accumulated. The legacy label stays verbatim beside these.
    "ALTER TABLE scores ADD COLUMN rubric_id TEXT",
    "ALTER TABLE scores ADD COLUMN kind TEXT",
    "CREATE INDEX IF NOT EXISTS idx_scores_rubric_id ON scores(rubric_id, created_at)",
    "CREATE INDEX IF NOT EXISTS idx_scores_kind ON scores(kind, created_at)",
    // A rubric edit changes what a score *means*, and nothing recorded that one had happened.
    // A new version is a new row linked to the old one, never a mutation of it.
    "ALTER TABLE rubrics ADD COLUMN version INTEGER NOT NULL DEFAULT 1",
    "ALTER TABLE rubrics ADD COLUMN supersedes TEXT",
];

pub const LEGACY_ADDED_COLUMNS_LATE: &[&str] = &[
    // Measure-to-act guardrails (M4): a threshold that is not a bare number (`threshold_json`), a
    // forecast-driven action override and its deadline, what created the rule, and when a
    // policy-created rule lapses. All nullable — an existing row reads back as exactly the fixed,
    // human-made, never-expiring rule it always was.
    "ALTER TABLE limit_rules ADD COLUMN threshold_json TEXT",
    "ALTER TABLE limit_rules ADD COLUMN escalation_json TEXT",
    "ALTER TABLE limit_rules ADD COLUMN escalated_until TEXT",
    "ALTER TABLE limit_rules ADD COLUMN origin TEXT",
    "ALTER TABLE limit_rules ADD COLUMN expires_at TEXT",
    // M7 — the relay's fenced, renewable lease: `failures` is the retry budget, `stale_reclaims`
    // counts device deaths (kept apart so a sleeping laptop does not burn a chance), `lease_fence` is
    // the holding device's identity compared exactly on settle/renew/progress, `progress` its liveness.
    "ALTER TABLE relay_tasks ADD COLUMN failures INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN stale_reclaims INTEGER NOT NULL DEFAULT 0",
    "ALTER TABLE relay_tasks ADD COLUMN lease_fence TEXT",
    "ALTER TABLE relay_tasks ADD COLUMN progress TEXT",
    // M23 — the served-version quality loop on the prompt row: the canary policy (JSON, NULL = the
    // registry still stops observing a version at promotion) and the append-only label ledger
    // (JSON array, NULL = a prompt whose labels have not moved since the column existed). Both
    // nullable, so an existing registry row reads back as exactly the prompt it always was.
    "ALTER TABLE prompts ADD COLUMN canary TEXT",
    "ALTER TABLE prompts ADD COLUMN label_history TEXT",
    // The quality read joins scores to events by event_id and windows on the VERDICT's created_at;
    // without this the join degrades to a scan of the scores table per window.
    "CREATE INDEX IF NOT EXISTS idx_scores_created ON scores(created_at)",
    // M11 — the per-project judge-trust policy. Nullable-with-a-default and OFF: turning it on
    // retroactively would block every existing deployment's gates on the day it upgraded, because
    // nothing has been calibrated yet.
    "ALTER TABLE projects ADD COLUMN require_trusted_judge INTEGER NOT NULL DEFAULT 0",
    // M24 — eval corpus lineage. `parent_id` is the link that makes `version` mean something: a v2
    // with no parent is just another row that shares a name. `input_hash` is the normalised-input
    // fingerprint near-duplicate collapse looks up instead of scanning every stored case's text.
    // Both nullable — a pre-M24 dataset is a v1 with no parent, and its items are simply not known
    // to be duplicates of anything (which is why dedupe treats NULL as "no match", never as one).
    "ALTER TABLE datasets ADD COLUMN parent_id TEXT",
    "ALTER TABLE dataset_items ADD COLUMN input_hash TEXT",
    // The version walk (`GET /v1/projects/:id/datasets/:name/versions`) and the fork's "what is the
    // highest version this name already has" read.
    "CREATE INDEX IF NOT EXISTS idx_datasets_name_version ON datasets(project_id, name, version)",
    // Dedupe's lookup: the fingerprints already in the target set.
    "CREATE INDEX IF NOT EXISTS idx_dataset_items_hash ON dataset_items(dataset_id, input_hash)",
    // M17 — the job queue's missing tenant. Without it a project key reading `GET /v1/jobs` saw
    // every project's payloads. Nullable: NULL is an operator/legacy job (a sweep, or anything
    // enqueued before this column existed), which `Scope::Operator` sees and no project scope does.
    "ALTER TABLE jobs ADD COLUMN project_id TEXT",
    "CREATE INDEX IF NOT EXISTS idx_jobs_project_created ON jobs(project_id, created_at DESC)",
];

pub const LEGACY_ADD_RECEIVED_AT: &str = "ALTER TABLE events ADD COLUMN received_at TEXT";

/// The M26 rebuild exactly as the pre-M14 backend ran it, for the legacy replica.
pub const LEGACY_M26_REBUILD: &str = "BEGIN;
         CREATE TABLE model_prices_m26 (
           provider              TEXT NOT NULL,
           model                 TEXT NOT NULL,
           input_per_mtok        REAL NOT NULL,
           output_per_mtok       REAL NOT NULL,
           cached_input_per_mtok REAL,
           effective_from        TEXT NOT NULL,
           source_url            TEXT,
           verified_at           TEXT,
           note                  TEXT,
           PRIMARY KEY (provider, model, effective_from)
         );
         INSERT OR IGNORE INTO model_prices_m26
           (provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok,
            effective_from, source_url, verified_at, note)
           SELECT provider, model, input_per_mtok, output_per_mtok, cached_input_per_mtok,
                  effective_date, source_url, NULL, NULL
             FROM model_prices;
         DROP TABLE model_prices;
         ALTER TABLE model_prices_m26 RENAME TO model_prices;
         COMMIT;";
