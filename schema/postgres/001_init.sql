-- LightTrack Postgres store. Ports schema/sqlite/001_init.sql:
--   no PRAGMA; INTEGER -> BIGINT; REAL -> DOUBLE PRECISION; reserved word "window" quoted.
-- Timestamps stay TEXT (fixed-width RFC3339(Nanos,Z)) so string range filters/ORDER BY match SQLite.
-- Booleans are stored as BIGINT 0/1 to match the app's `bool as i64` writes.

CREATE TABLE IF NOT EXISTS projects (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  enabled     BIGINT NOT NULL DEFAULT 1,
  redaction   TEXT NOT NULL DEFAULT 'none',
  -- Consent to include this project's benchmark runs in a collective-network digest. Default off.
  collective_opt_in BIGINT NOT NULL DEFAULT 0,
  created_at  TEXT NOT NULL,
  -- Set by DELETE /v1/projects/:id. Archive, never delete: the events and runs stay.
  archived_at TEXT
);
ALTER TABLE projects ADD COLUMN IF NOT EXISTS archived_at TEXT;
-- M11: the per-project judge-trust policy. OFF by default: turning it on retroactively would
-- block every existing deployment's gates on the day it upgraded, nothing having been calibrated.
ALTER TABLE projects ADD COLUMN IF NOT EXISTS require_trusted_judge BIGINT NOT NULL DEFAULT 0;

CREATE TABLE IF NOT EXISTS api_keys (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL,
  name         TEXT NOT NULL,
  prefix       TEXT NOT NULL,
  key_hash     TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  last_used_at TEXT,
  revoked      BIGINT NOT NULL DEFAULT 0,
  -- JSON array of ingest|read|manage. NULL on rows written before scopes existed, which read as
  -- the permissive back-compat default (core::decode_scopes).
  scopes       TEXT,
  -- Fixed-width RFC3339. Past it, the key authenticates as nothing.
  expires_at   TEXT
);
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS scopes TEXT;
ALTER TABLE api_keys ADD COLUMN IF NOT EXISTS expires_at TEXT;
CREATE INDEX IF NOT EXISTS idx_api_keys_prefix ON api_keys(prefix);

CREATE TABLE IF NOT EXISTS events (
  id                  TEXT PRIMARY KEY,
  project_id          TEXT NOT NULL,
  trace_id            TEXT,
  span_id             TEXT,
  parent_span_id      TEXT,
  ts                  TEXT NOT NULL,
  provider            TEXT NOT NULL,
  model               TEXT NOT NULL,
  operation           TEXT NOT NULL DEFAULT 'chat',
  input_tokens        BIGINT NOT NULL DEFAULT 0,
  output_tokens       BIGINT NOT NULL DEFAULT 0,
  cached_input_tokens BIGINT,
  reasoning_tokens    BIGINT,
  cost_usd            DOUBLE PRECISION,
  latency_ms          BIGINT,
  status              TEXT NOT NULL DEFAULT 'success',
  error               TEXT,
  input               TEXT,
  output              TEXT,
  tags                TEXT,
  source              TEXT,
  metadata            TEXT,
  name                TEXT,
  -- Server-stamped arrival time (fixed-width RFC3339 UTC, like `ts`). `ts` is CLIENT event time and
  -- may be skewed or deliberately backdated; every rolling-window accounting read (limit admission)
  -- keys on `received_at` so one wrong clock cannot move a budget window.
  received_at         TEXT
);
-- Existing deployments predate the `name` column (use-case attribution); idempotent on fresh DBs.
ALTER TABLE events ADD COLUMN IF NOT EXISTS name TEXT;
-- …and predate `received_at`. ADD COLUMN must come BEFORE any index over the column: an index on a
-- not-yet-added column fails the whole schema batch on every existing deployment.
ALTER TABLE events ADD COLUMN IF NOT EXISTS received_at TEXT;
-- Backfill arrival time to event time for pre-migration rows (the reads COALESCE too, so the two
-- agree either way). The partial index makes the backfill index-driven and, once it has run, empty —
-- so re-running the schema batch on every boot costs a lookup instead of a seq scan over `events`.
CREATE INDEX IF NOT EXISTS idx_events_received_backfill ON events(id) WHERE received_at IS NULL;
UPDATE events SET received_at = ts WHERE received_at IS NULL;
CREATE INDEX IF NOT EXISTS idx_events_project_ts ON events(project_id, ts);
-- Windowed accounting (admission's usage_since / usage_since_scoped) filters on server arrival time,
-- not client `ts`. It is indexed on the *expression the queries use* — a plain (project_id,
-- received_at) index cannot serve `COALESCE(received_at, ts) >= $2`, and admission runs this query
-- on every ingested event, inside the per-project admission lock.
CREATE INDEX IF NOT EXISTS idx_events_project_received
  ON events(project_id, COALESCE(received_at, ts));
CREATE INDEX IF NOT EXISTS idx_events_trace ON events(trace_id);
-- The trace surface reads a *project's* trace: `WHERE trace_id = $1 AND project_id = $2` (a trace id
-- is caller-supplied, so the project scope is part of the query, not a later check). Matching the
-- SQLite reference's idx_events_project_trace, this keeps the detail read proportional to the trace
-- instead of scanning the project's events; single-column idx_events_trace still serves the
-- project-agnostic (operator) read. Both columns predate every ADD COLUMN above.
CREATE INDEX IF NOT EXISTS idx_events_project_trace ON events(project_id, trace_id);

CREATE TABLE IF NOT EXISTS limit_rules (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  metric      TEXT NOT NULL,
  "window"    TEXT NOT NULL,
  threshold   DOUBLE PRECISION NOT NULL,
  action      TEXT NOT NULL,
  enabled     BIGINT NOT NULL DEFAULT 1,
  warn_at     DOUBLE PRECISION,
  scope_kind  TEXT,
  scope_value TEXT
);
-- Existing deployments predate warn_at/scope (soft warnings + scoped caps); idempotent on fresh DBs.
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS warn_at DOUBLE PRECISION;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS scope_kind TEXT;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS scope_value TEXT;

CREATE TABLE IF NOT EXISTS scores (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  event_id    TEXT,
  rubric      TEXT NOT NULL,
  value       DOUBLE PRECISION NOT NULL,
  max         DOUBLE PRECISION NOT NULL DEFAULT 1.0,
  pass        BIGINT,
  reasoning   TEXT,
  -- Structured verdict provenance (core::ScoreDetail) as JSON; NULL when posted without it.
  detail      TEXT,
  -- The benchmark run that produced this verdict (NULL for online/ad-hoc scores) and the 1-based
  -- case position within it: "every case result for run X" as a query, not a created_at guess.
  run_id      TEXT,
  case_index  BIGINT,
  scored_by   TEXT NOT NULL,
  cost_usd    DOUBLE PRECISION,
  created_at  TEXT NOT NULL
);
-- Existing deployments predate verdict provenance and run-scoped case results; idempotent on fresh DBs.
ALTER TABLE scores ADD COLUMN IF NOT EXISTS detail TEXT;
ALTER TABLE scores ADD COLUMN IF NOT EXISTS run_id TEXT;
ALTER TABLE scores ADD COLUMN IF NOT EXISTS case_index BIGINT;
CREATE INDEX IF NOT EXISTS idx_scores_project ON scores(project_id, created_at);
-- Probe scores by the event they judged: powers the trace-scores join and the online scorer's
-- unscored-events anti-join (WHERE event_id IN / LEFT JOIN scores). Without it both full-scan scores.
CREATE INDEX IF NOT EXISTS idx_scores_event ON scores(event_id);
-- Run-scoped case results, already in the listing's (case_index, created_at) order. Declared after
-- the ALTERs above so it never indexes a column an older deployment hasn't been widened with yet.
CREATE INDEX IF NOT EXISTS idx_scores_run ON scores(run_id, case_index, created_at);

CREATE TABLE IF NOT EXISTS benchmarks (
  id             TEXT PRIMARY KEY,
  project_id     TEXT NOT NULL,
  name           TEXT NOT NULL,
  rubric         TEXT NOT NULL,
  judge_model    TEXT NOT NULL,
  target         TEXT,
  dataset_ref    TEXT,
  dataset        TEXT,
  rubric_id      TEXT,
  baseline_score DOUBLE PRECISION,
  created_at     TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS rubrics (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  name        TEXT NOT NULL,
  dimensions  TEXT NOT NULL,
  threshold   DOUBLE PRECISION NOT NULL DEFAULT 0.7,
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
  id           TEXT PRIMARY KEY,
  type         TEXT NOT NULL,
  payload      TEXT,
  status       TEXT NOT NULL DEFAULT 'queued',
  attempts     BIGINT NOT NULL DEFAULT 0,   -- claims, including ones a crash ended
  max_attempts BIGINT NOT NULL DEFAULT 3,
  failures       BIGINT NOT NULL DEFAULT 0, -- runs that actually failed (the retry budget)
  stale_reclaims BIGINT NOT NULL DEFAULT 0, -- worker deaths (claim held past the stale window)
  progress     TEXT,
  error        TEXT,
  result       TEXT,
  claimed_at   TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
-- Databases created before honest failure accounting existed. ADD COLUMN IF NOT EXISTS must stay
-- above the index below (same rule as the scores columns): an index over a column an old table
-- does not have yet is a hard error.
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS failures BIGINT NOT NULL DEFAULT 0;
ALTER TABLE jobs ADD COLUMN IF NOT EXISTS stale_reclaims BIGINT NOT NULL DEFAULT 0;
CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status, created_at);

CREATE TABLE IF NOT EXISTS benchmark_runs (
  id              TEXT PRIMARY KEY,
  benchmark_id    TEXT NOT NULL,
  started_at      TEXT NOT NULL,
  finished_at     TEXT,
  n_cases         BIGINT NOT NULL DEFAULT 0,
  mean_score      DOUBLE PRECISION,
  pass_rate       DOUBLE PRECISION,
  cost_usd        DOUBLE PRECISION,
  status          TEXT NOT NULL DEFAULT 'running',
  p50_latency_ms  BIGINT,
  p95_latency_ms  BIGINT,
  total_tokens    BIGINT,
  report          TEXT
);

CREATE TABLE IF NOT EXISTS model_prices (
  provider              TEXT NOT NULL,
  model                 TEXT NOT NULL,
  input_per_mtok        DOUBLE PRECISION NOT NULL,
  output_per_mtok       DOUBLE PRECISION NOT NULL,
  cached_input_per_mtok DOUBLE PRECISION,
  effective_date        TEXT NOT NULL,
  source_url            TEXT,
  PRIMARY KEY (provider, model)
);

CREATE TABLE IF NOT EXISTS datasets (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  name        TEXT NOT NULL,
  version     BIGINT NOT NULL DEFAULT 1,
  frozen      BIGINT NOT NULL DEFAULT 0,
  source      TEXT,
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS dataset_items (
  id              TEXT PRIMARY KEY,
  dataset_id      TEXT NOT NULL,
  input           TEXT NOT NULL,
  output          TEXT,
  expected        TEXT,
  context         TEXT,
  tags            TEXT,
  source_event_id TEXT,
  anonymization   TEXT
);
CREATE INDEX IF NOT EXISTS idx_dataset_items_ds ON dataset_items(dataset_id);

-- Normalized revenue (profit tracking): the revenue analog of events' cost. Netted against LLM cost
-- per customer/product for margin. Mirrors schema/sqlite/001_init.sql.
CREATE TABLE IF NOT EXISTS revenue_events (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL,
  source        TEXT NOT NULL DEFAULT 'manual',
  external_id   TEXT,
  customer_id   TEXT,
  product_id    TEXT,
  amount_usd    DOUBLE PRECISION NOT NULL,
  currency      TEXT NOT NULL DEFAULT 'USD',
  kind          TEXT NOT NULL DEFAULT 'one_time',
  period_start  TEXT,
  period_end    TEXT,
  ts            TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_revenue_project_ts ON revenue_events(project_id, ts);
CREATE INDEX IF NOT EXISTS idx_revenue_customer ON revenue_events(customer_id);

-- Cloud→device relay queue (docs/RELAY.md). Mirrors schema/sqlite/001_init.sql: timestamps are
-- fixed-width RFC3339 TEXT (string range filters on next_attempt_at / lease_deadline are correct).
CREATE TABLE IF NOT EXISTS relay_tasks (
  id                  TEXT PRIMARY KEY,
  project_id          TEXT NOT NULL,
  source              TEXT,
  action_type         TEXT NOT NULL,
  payload             TEXT,
  status              TEXT NOT NULL DEFAULT 'queued',  -- queued | leased | succeeded | dead
  attempts            BIGINT NOT NULL DEFAULT 0,
  max_attempts        BIGINT NOT NULL DEFAULT 4,
  retry_interval_secs BIGINT NOT NULL DEFAULT 18000,
  idempotency_key     TEXT,
  device              TEXT,
  lease_deadline      TEXT,
  next_attempt_at     TEXT NOT NULL,
  result              TEXT,
  error               TEXT,
  created_at          TEXT NOT NULL,
  updated_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_relay_due ON relay_tasks(status, next_attempt_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_idem ON relay_tasks(project_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;

-- Collective Model Intelligence (the opt-in shared leaderboard). Mirrors
-- schema/sqlite/001_init.sql: pure aggregate rows, no text and no project/customer ids; the primary
-- key makes a re-contribution an upsert in place rather than a second vote from the same source.
-- Timestamps stay fixed-width RFC3339(Nanos,Z) TEXT so `received_at` range filters and ORDER BY
-- agree with SQLite exactly.
CREATE TABLE IF NOT EXISTS collective_entries (
  contributor_id      TEXT NOT NULL,   -- opaque, non-reversible source id (a hash)
  provider            TEXT NOT NULL,
  model               TEXT NOT NULL,
  task_type           TEXT NOT NULL,   -- coarse bucket from a fixed vocabulary
  quality             DOUBLE PRECISION NOT NULL,
  pass_rate           DOUBLE PRECISION NOT NULL,
  avg_cost_usd        DOUBLE PRECISION NOT NULL,
  p50_latency_ms      BIGINT,
  p95_latency_ms      BIGINT,
  n_runs              BIGINT NOT NULL DEFAULT 0,
  n_cases             BIGINT NOT NULL DEFAULT 0,
  quality_variance    DOUBLE PRECISION,
  judge_provider      TEXT,
  rubric_fingerprint  TEXT,
  determinism         TEXT,            -- rigor: weakest stamp, NULL = unrecorded
  frozen_dataset      TEXT,            -- rigor: coverage tag (all|mixed|none), NULL = unknown
  significance_tested TEXT,            -- rigor: coverage tag (all|mixed|none), NULL = unknown
  received_at         TEXT NOT NULL,
  PRIMARY KEY (contributor_id, provider, model, task_type)
);
CREATE INDEX IF NOT EXISTS idx_collective_model ON collective_entries(provider, model, task_type);
CREATE INDEX IF NOT EXISTS idx_collective_received ON collective_entries(received_at);

-- Standing margin guardrails (M4). Mirrors schema/sqlite/001_init.sql: `trigger`/`action` are JSON
-- text because both are open sum types, and this is sweep-time config rather than hot-path data.
CREATE TABLE IF NOT EXISTS margin_policies (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL,
  trigger_json  TEXT NOT NULL,
  min_cost_usd  DOUBLE PRECISION NOT NULL DEFAULT 0,
  action_json   TEXT NOT NULL,
  cooldown_secs BIGINT NOT NULL DEFAULT 3600,
  expiry_secs   BIGINT NOT NULL DEFAULT 86400,
  enabled       BIGINT NOT NULL DEFAULT 1
);
CREATE INDEX IF NOT EXISTS idx_margin_policies_project ON margin_policies(project_id);

-- Derived thresholds, escalation and rule provenance (M4), additive on limit_rules. All nullable:
-- an existing rule carries no opinion, which reads back as the fixed threshold it always had.
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS threshold_json TEXT;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS escalation_json TEXT;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS escalated_until TEXT;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS origin TEXT;
ALTER TABLE limit_rules ADD COLUMN IF NOT EXISTS expires_at TEXT;
CREATE INDEX IF NOT EXISTS idx_limit_rules_origin ON limit_rules(origin) WHERE origin IS NOT NULL;

-- ===========================================================================================
-- M7: stored schedules + the fenced, renewable relay lease. Self-contained block, appended.
-- Mirrors schema/sqlite/001_init.sql; timestamps stay fixed-width RFC3339 TEXT.
-- ===========================================================================================

CREATE TABLE IF NOT EXISTS schedules (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL,
  kind          TEXT NOT NULL,
  payload       TEXT,
  interval_secs BIGINT NOT NULL,
  next_due      TEXT NOT NULL,
  last_job_id   TEXT,
  enabled       BOOLEAN NOT NULL DEFAULT TRUE,
  created_at    TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_schedules_due ON schedules(enabled, next_due);
CREATE INDEX IF NOT EXISTS idx_schedules_project ON schedules(project_id);

ALTER TABLE relay_tasks ADD COLUMN IF NOT EXISTS failures BIGINT NOT NULL DEFAULT 0;
ALTER TABLE relay_tasks ADD COLUMN IF NOT EXISTS stale_reclaims BIGINT NOT NULL DEFAULT 0;
ALTER TABLE relay_tasks ADD COLUMN IF NOT EXISTS lease_fence TEXT;
ALTER TABLE relay_tasks ADD COLUMN IF NOT EXISTS progress TEXT;
CREATE INDEX IF NOT EXISTS idx_relay_lease ON relay_tasks(status, lease_deadline);

-- ---------------------------------------------------------------------------
-- M9 — provenance on the row. Additive columns, applied after every CREATE
-- above so this block is self-contained and order-independent: a fresh database
-- gets them here, an existing one is widened here, and re-running the file is a
-- no-op. Every column is nullable — a pre-M9 row carries no opinion, and the
-- readers are written to treat absence as "unknown" rather than as a value.
-- ---------------------------------------------------------------------------

-- B. FX provenance: `amount_usd` is derived, and a wrong rate makes it wrong.
-- The provider's own minor-unit figure never needs restating, so keeping it —
-- with the rate, the book version behind it, and whether a real conversion
-- happened — turns a rate correction into a reprice instead of a re-ingest.
ALTER TABLE revenue_events ADD COLUMN IF NOT EXISTS amount_minor BIGINT;
ALTER TABLE revenue_events ADD COLUMN IF NOT EXISTS fx_rate DOUBLE PRECISION;
ALTER TABLE revenue_events ADD COLUMN IF NOT EXISTS fx_book_version TEXT;
ALTER TABLE revenue_events ADD COLUMN IF NOT EXISTS converted BOOLEAN;

-- C. Typed verdict identity: `scores.rubric` is one free-text column carrying six encodings, so a
-- reader had to parse a string to learn what it was reading, and the alert window keyed on that
-- string — which made every per-case verdict a unique key that never accumulated. The legacy label
-- stays verbatim beside these.
ALTER TABLE scores ADD COLUMN IF NOT EXISTS rubric_id TEXT;
ALTER TABLE scores ADD COLUMN IF NOT EXISTS kind TEXT;
CREATE INDEX IF NOT EXISTS idx_scores_rubric_id ON scores(rubric_id, created_at);
CREATE INDEX IF NOT EXISTS idx_scores_kind ON scores(kind, created_at);

-- A rubric edit changes what a score means, and nothing recorded that one had happened. A new
-- version is a new row linked to the old one, never a mutation of it.
ALTER TABLE rubrics ADD COLUMN IF NOT EXISTS version INTEGER NOT NULL DEFAULT 1;
ALTER TABLE rubrics ADD COLUMN IF NOT EXISTS supersedes TEXT;

-- ---------------------------------------------------------------------------
-- M10 — the prompt registry. Self-contained and appended at the end of the file
-- so it is order-independent: both tables are created here, and re-running the
-- file is a no-op. The registry used to be SQLite-only, which meant the
-- promotion gate — the one place a prompt edit becomes a measurable quality
-- step — returned 501 on every managed Postgres deployment.
-- ---------------------------------------------------------------------------

-- Named, versioned prompts fetched at runtime by label (e.g. production | staging).
-- Cutting a new version auto-enqueues the linked benchmark; promoting a label is blocked when that
-- benchmark's run did not generate with the version being promoted, or regressed against baseline.
CREATE TABLE IF NOT EXISTS prompts (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL,
  name          TEXT NOT NULL,
  benchmark_id  TEXT,                         -- linked benchmark; its regression check gates promotion
  labels        TEXT NOT NULL DEFAULT '{}',   -- JSON object: label -> version (e.g. {"production": 3})
  created_at    TEXT NOT NULL,
  updated_at    TEXT NOT NULL,
  UNIQUE (project_id, name)
);
CREATE INDEX IF NOT EXISTS idx_prompts_project ON prompts(project_id, name);

-- Immutable prompt versions (one row per cut). `version` is monotonic per prompt.
CREATE TABLE IF NOT EXISTS prompt_versions (
  id          TEXT PRIMARY KEY,
  prompt_id   TEXT NOT NULL REFERENCES prompts(id),
  version     INTEGER NOT NULL,
  content     TEXT NOT NULL,
  config      TEXT,           -- JSON (model, params, variable schema)
  note        TEXT,           -- change note / "commit message"
  created_at  TEXT NOT NULL,
  UNIQUE (prompt_id, version)
);
CREATE INDEX IF NOT EXISTS idx_prompt_versions_pid ON prompt_versions(prompt_id, version);

-- M26 — the price book becomes a dated, append-only timeline. Self-contained
-- and idempotent like the M9 block above: a fresh database gets the new shape
-- here, an existing one is migrated here, and re-running the file is a no-op.
--
-- Why this is not just an ADD COLUMN: the identity of a rate has to become
-- (provider, model, effective_from), or a correction overwrites the row that
-- priced last quarter's traffic and no June cost number can ever be defended.
-- ---------------------------------------------------------------------------

-- The pre-M26 date column becomes the key's date. Renamed rather than added
-- beside it, so a row carries exactly one date and nothing has to decide which
-- of two spellings wins.
DO $m26$
BEGIN
  IF EXISTS (SELECT 1 FROM information_schema.columns
              WHERE table_name = 'model_prices' AND column_name = 'effective_date')
     AND NOT EXISTS (SELECT 1 FROM information_schema.columns
              WHERE table_name = 'model_prices' AND column_name = 'effective_from')
  THEN
    ALTER TABLE model_prices RENAME COLUMN effective_date TO effective_from;
  END IF;
END
$m26$;

-- Both nullable: nobody vouched for a pre-M26 rate, and stamping "verified
-- today" onto rows nobody checked would make the staleness warning repeat a lie.
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS effective_from TEXT;
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS verified_at TEXT;
ALTER TABLE model_prices ADD COLUMN IF NOT EXISTS note TEXT;
UPDATE model_prices SET effective_from = '1970-01-01T00:00:00.000000000Z'
 WHERE effective_from IS NULL;
ALTER TABLE model_prices ALTER COLUMN effective_from SET NOT NULL;

-- Widen the primary key in place, guarded on the key's own column list so this
-- runs once and is a no-op on every subsequent apply.
DO $m26pk$
BEGIN
  IF NOT EXISTS (
    SELECT 1 FROM pg_index i
      JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
     WHERE i.indrelid = 'model_prices'::regclass AND i.indisprimary
       AND a.attname = 'effective_from')
  THEN
    ALTER TABLE model_prices DROP CONSTRAINT IF EXISTS model_prices_pkey;
    ALTER TABLE model_prices
      ADD CONSTRAINT model_prices_pkey PRIMARY KEY (provider, model, effective_from);
  END IF;
END
$m26pk$;

-- M18 - the enrolled relay device fleet. Self-contained block, appended.
-- ---------------------------------------------------------------------------

-- Who may lease relay tasks, and what each one can actually run. The relay used
-- to have exactly one anonymous device: a shared LIGHTTRACK_RELAY_DEVICE_KEY
-- authorized every lease and the `device` written onto a task was whatever the
-- client asserted, so identity was un-revocable and routing was blind - a task
-- went to whoever asked first, including a device whose action library lacked
-- the action, which burned a real attempt and then a five-hour retry interval to
-- do it again. `key_hash` is the api_keys scheme verbatim ("<salt>:<sha256hex>");
-- the raw `ltd_<prefix>_<secret>` is shown once and never stored. `capabilities`
-- is a JSON array of action types / "<ns>/*" prefixes - EMPTY means "everything",
-- the back-compat answer a pre-M18 agent gives. `relay_tasks.device` keeps its
-- column and now carries this table's `id`.
CREATE TABLE IF NOT EXISTS devices (
  id            TEXT PRIMARY KEY,
  project_id    TEXT,
  name          TEXT NOT NULL,
  key_prefix    TEXT NOT NULL,
  key_hash      TEXT NOT NULL,
  capabilities  TEXT NOT NULL DEFAULT '[]',
  last_seen_at  TEXT,
  agent_version TEXT,
  created_at    TEXT NOT NULL,
  revoked       BOOLEAN NOT NULL DEFAULT FALSE
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_devices_prefix ON devices(key_prefix);
CREATE INDEX IF NOT EXISTS idx_devices_project ON devices(project_id);

-- ===========================================================================================
-- M3: the persisted alert ledger + per-project routing. Self-contained block, appended.
-- Mirrors schema/sqlite/001_init.sql; timestamps stay fixed-width RFC3339 TEXT.
-- ===========================================================================================

CREATE TABLE IF NOT EXISTS alerts (
  id          TEXT PRIMARY KEY,
  project_id  TEXT,
  kind        TEXT NOT NULL,
  dedup_key   TEXT NOT NULL,
  severity    TEXT NOT NULL,
  payload     TEXT,
  fired_at    TEXT NOT NULL,
  delivered   TEXT,
  acked_at    TEXT,
  acked_by    TEXT,
  resolution  TEXT
);
CREATE INDEX IF NOT EXISTS idx_alerts_dedup ON alerts(dedup_key, fired_at);
CREATE INDEX IF NOT EXISTS idx_alerts_fired ON alerts(fired_at);
CREATE INDEX IF NOT EXISTS idx_alerts_project ON alerts(project_id, fired_at);

CREATE TABLE IF NOT EXISTS alert_channels (
  id               TEXT PRIMARY KEY,
  project_id       TEXT,
  kind             TEXT NOT NULL,
  target           TEXT NOT NULL,
  secret_hash      TEXT,
  prev_secret_hash TEXT,
  min_severity     TEXT NOT NULL,
  kinds            TEXT,
  enabled          BOOLEAN NOT NULL DEFAULT TRUE,
  created_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_alert_channels_project ON alert_channels(project_id);

-- ===========================================================================================
-- M11: the human verdict ledger + the calibration history. Self-contained block, appended.
-- Mirrors schema/sqlite/001_init.sql; timestamps stay fixed-width RFC3339 TEXT.
-- ===========================================================================================

CREATE TABLE IF NOT EXISTS labels (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL,
  subject_kind TEXT NOT NULL,
  subject_id   TEXT NOT NULL,
  rubric_id    TEXT,
  value        DOUBLE PRECISION NOT NULL,
  pass         BOOLEAN,
  dimensions   TEXT,
  labeler      TEXT NOT NULL,
  note         TEXT,
  created_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_labels_subject ON labels(subject_kind, subject_id);
CREATE INDEX IF NOT EXISTS idx_labels_project ON labels(project_id, created_at);
CREATE INDEX IF NOT EXISTS idx_labels_rubric ON labels(rubric_id, created_at);

CREATE TABLE IF NOT EXISTS calibrations (
  id              TEXT PRIMARY KEY,
  project_id      TEXT NOT NULL,
  judge           TEXT NOT NULL,
  rubric_id       TEXT,
  dataset_id      TEXT,
  dataset_version INTEGER,
  kappa           DOUBLE PRECISION NOT NULL,
  pearson         DOUBLE PRECISION NOT NULL,
  mae             DOUBLE PRECISION NOT NULL,
  rmse            DOUBLE PRECISION NOT NULL,
  n               INTEGER NOT NULL,
  kappa_bar       DOUBLE PRECISION NOT NULL,
  trusted         BOOLEAN NOT NULL,
  created_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_calibrations_key
  ON calibrations(project_id, judge, rubric_id, created_at);
