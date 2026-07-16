-- LightTrack Postgres store. Ports schema/sqlite/001_init.sql:
--   no PRAGMA; INTEGER -> BIGINT; REAL -> DOUBLE PRECISION; reserved word "window" quoted.
-- Timestamps stay TEXT (fixed-width RFC3339(Nanos,Z)) so string range filters/ORDER BY match SQLite.
-- Booleans are stored as BIGINT 0/1 to match the app's `bool as i64` writes.

CREATE TABLE IF NOT EXISTS projects (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  enabled     BIGINT NOT NULL DEFAULT 1,
  redaction   TEXT NOT NULL DEFAULT 'none',
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
  metadata            TEXT
);
CREATE INDEX IF NOT EXISTS idx_events_project_ts ON events(project_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_trace ON events(trace_id);

CREATE TABLE IF NOT EXISTS limit_rules (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  metric      TEXT NOT NULL,
  "window"    TEXT NOT NULL,
  threshold   DOUBLE PRECISION NOT NULL,
  action      TEXT NOT NULL,
  enabled     BIGINT NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS scores (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  event_id    TEXT,
  rubric      TEXT NOT NULL,
  value       DOUBLE PRECISION NOT NULL,
  max         DOUBLE PRECISION NOT NULL DEFAULT 1.0,
  pass        BIGINT,
  reasoning   TEXT,
  scored_by   TEXT NOT NULL,
  cost_usd    DOUBLE PRECISION,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_scores_project ON scores(project_id, created_at);
-- Probe scores by the event they judged: powers the trace-scores join and the online scorer's
-- unscored-events anti-join (WHERE event_id IN / LEFT JOIN scores). Without it both full-scan scores.
CREATE INDEX IF NOT EXISTS idx_scores_event ON scores(event_id);

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
  attempts     BIGINT NOT NULL DEFAULT 0,
  max_attempts BIGINT NOT NULL DEFAULT 3,
  progress     TEXT,
  error        TEXT,
  result       TEXT,
  claimed_at   TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
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
