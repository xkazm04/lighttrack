-- LightTrack local store (SQLite). Mirrors schema/bigquery/001_init.sql.
PRAGMA journal_mode = WAL;

CREATE TABLE IF NOT EXISTS projects (
  id          TEXT PRIMARY KEY,
  name        TEXT NOT NULL,
  enabled     INTEGER NOT NULL DEFAULT 1,
  redaction   TEXT NOT NULL DEFAULT 'none',   -- none | hash | drop
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS api_keys (
  id           TEXT PRIMARY KEY,
  project_id   TEXT NOT NULL REFERENCES projects(id),
  name         TEXT NOT NULL,
  prefix       TEXT NOT NULL,
  key_hash     TEXT NOT NULL,
  created_at   TEXT NOT NULL,
  last_used_at TEXT,
  revoked      INTEGER NOT NULL DEFAULT 0
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
  input_tokens        INTEGER NOT NULL DEFAULT 0,
  output_tokens       INTEGER NOT NULL DEFAULT 0,
  cached_input_tokens INTEGER,
  reasoning_tokens    INTEGER,
  cost_usd            REAL,
  latency_ms          INTEGER,
  status              TEXT NOT NULL DEFAULT 'success',
  error               TEXT,
  input               TEXT,        -- JSON
  output              TEXT,        -- JSON
  tags                TEXT,        -- JSON array
  source              TEXT,
  metadata            TEXT         -- JSON
);
CREATE INDEX IF NOT EXISTS idx_events_project_ts ON events(project_id, ts);
CREATE INDEX IF NOT EXISTS idx_events_trace ON events(trace_id);

CREATE TABLE IF NOT EXISTS limit_rules (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  metric      TEXT NOT NULL,   -- cost_usd | calls | tokens
  window      TEXT NOT NULL,   -- hour | day | month
  threshold   REAL NOT NULL,
  action      TEXT NOT NULL,   -- alert | throttle | block
  enabled     INTEGER NOT NULL DEFAULT 1
);

CREATE TABLE IF NOT EXISTS scores (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  event_id    TEXT,
  rubric      TEXT NOT NULL,
  value       REAL NOT NULL,
  max         REAL NOT NULL DEFAULT 1.0,
  pass        INTEGER,
  reasoning   TEXT,
  scored_by   TEXT NOT NULL,
  cost_usd    REAL,
  created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_scores_project ON scores(project_id, created_at);

CREATE TABLE IF NOT EXISTS benchmarks (
  id             TEXT PRIMARY KEY,
  project_id     TEXT NOT NULL,
  name           TEXT NOT NULL,
  rubric         TEXT NOT NULL,
  judge_model    TEXT NOT NULL,
  target         TEXT,         -- JSON
  dataset_ref    TEXT,
  dataset        TEXT,         -- JSON array of {input, expected?, output?}
  rubric_id      TEXT,         -- optional structured rubric for per-dimension judging
  baseline_score REAL,
  created_at     TEXT NOT NULL
);

-- Weighted, anchored rubrics (Phase 3.6c).
CREATE TABLE IF NOT EXISTS rubrics (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  name        TEXT NOT NULL,
  dimensions  TEXT NOT NULL,   -- JSON array of {key, description, weight, anchors, floor?}
  threshold   REAL NOT NULL DEFAULT 0.7,
  created_at  TEXT NOT NULL
);

-- Background job queue (Phase 3.6d): enqueue returns immediately; lt-runner serve executes.
CREATE TABLE IF NOT EXISTS jobs (
  id           TEXT PRIMARY KEY,
  type         TEXT NOT NULL,
  payload      TEXT,           -- JSON
  status       TEXT NOT NULL DEFAULT 'queued',
  attempts     INTEGER NOT NULL DEFAULT 0,
  max_attempts INTEGER NOT NULL DEFAULT 3,
  progress     TEXT,
  error        TEXT,
  result       TEXT,           -- JSON
  claimed_at   TEXT,
  created_at   TEXT NOT NULL,
  updated_at   TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_jobs_status ON jobs(status, created_at);

CREATE TABLE IF NOT EXISTS benchmark_runs (
  id              TEXT PRIMARY KEY,
  benchmark_id    TEXT NOT NULL REFERENCES benchmarks(id),
  started_at      TEXT NOT NULL,
  finished_at     TEXT,
  n_cases         INTEGER NOT NULL DEFAULT 0,
  mean_score      REAL,
  pass_rate       REAL,
  cost_usd        REAL,
  status          TEXT NOT NULL DEFAULT 'running',
  p50_latency_ms  INTEGER,
  p95_latency_ms  INTEGER,
  total_tokens    INTEGER,
  report          TEXT
);

-- DB-backed price book (source of truth; config/pricing.json is the seed).
CREATE TABLE IF NOT EXISTS model_prices (
  provider              TEXT NOT NULL,
  model                 TEXT NOT NULL,
  input_per_mtok        REAL NOT NULL,
  output_per_mtok       REAL NOT NULL,
  cached_input_per_mtok REAL,
  effective_date        TEXT NOT NULL,
  source_url            TEXT,
  PRIMARY KEY (provider, model)
);

-- Versioned evaluation datasets (Phase 3.6b), built by hand or sampled from real events.
CREATE TABLE IF NOT EXISTS datasets (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  name        TEXT NOT NULL,
  version     INTEGER NOT NULL DEFAULT 1,
  frozen      INTEGER NOT NULL DEFAULT 0,
  source      TEXT,
  created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS dataset_items (
  id              TEXT PRIMARY KEY,
  dataset_id      TEXT NOT NULL REFERENCES datasets(id),
  input           TEXT NOT NULL,
  output          TEXT,
  expected        TEXT,
  context         TEXT,
  tags            TEXT,        -- JSON array
  source_event_id TEXT,
  anonymization   TEXT         -- JSON {method, redactions}
);
CREATE INDEX IF NOT EXISTS idx_dataset_items_ds ON dataset_items(dataset_id);
