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
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL,
  name         TEXT NOT NULL,
  prefix       TEXT NOT NULL,
  key_hash     TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  last_used_at TEXT,
  revoked      BIGINT NOT NULL DEFAULT 0
);
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
