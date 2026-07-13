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
  name                TEXT,        -- optional use-case / call-site label (rollup key)
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
-- Composite for the project-scoped trace rollup (list_trace_summaries): filter project_id + group
-- by trace_id without a full scan. Single-column idx_events_trace still serves the project-agnostic
-- per-trace fetch (list_by_trace: WHERE trace_id = ?).
CREATE INDEX IF NOT EXISTS idx_events_project_trace ON events(project_id, trace_id);
CREATE INDEX IF NOT EXISTS idx_events_project_name_ts ON events(project_id, name, ts);

CREATE TABLE IF NOT EXISTS limit_rules (
  id          TEXT PRIMARY KEY,
  project_id  TEXT NOT NULL,
  metric      TEXT NOT NULL,   -- cost_usd | calls | tokens
  window      TEXT NOT NULL,   -- hour | day | month
  threshold   REAL NOT NULL,
  action      TEXT NOT NULL,   -- alert | throttle | block
  enabled     INTEGER NOT NULL DEFAULT 1,
  warn_at     REAL,            -- optional soft-warning fraction in (0,1); NULL = no pre-warning
  scope_kind  TEXT,            -- provider | model | name; NULL = project-wide (unscoped)
  scope_value TEXT             -- the scoped dimension value; NULL when unscoped
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

-- Prompt registry: named, versioned prompts fetched at runtime by label (e.g. production | staging).
-- Cutting a new version auto-enqueues the linked benchmark; promoting a label is blocked when that
-- benchmark's score regresses against its baseline. Reuses the benchmark + job-queue machinery.
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

-- Normalized revenue (Phase 1 profit tracking): the revenue analog of events' cost. Synced from a
-- billing provider (Stripe/Polar) or posted by hand; netted against LLM cost per customer/product.
CREATE TABLE IF NOT EXISTS revenue_events (
  id            TEXT PRIMARY KEY,
  project_id    TEXT NOT NULL,
  source        TEXT NOT NULL DEFAULT 'manual',  -- stripe | polar | manual
  external_id   TEXT,                            -- provider invoice/charge/order id (idempotency)
  customer_id   TEXT,
  product_id    TEXT,
  amount_usd    REAL NOT NULL,                   -- non-negative magnitude; sign derived from kind
  currency      TEXT NOT NULL DEFAULT 'USD',
  kind          TEXT NOT NULL DEFAULT 'one_time',-- subscription | one_time | usage | refund
  period_start  TEXT,                            -- subscription recognition window (RFC3339)
  period_end    TEXT,
  ts            TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_revenue_project_ts ON revenue_events(project_id, ts);
CREATE INDEX IF NOT EXISTS idx_revenue_customer ON revenue_events(customer_id);

-- Collective Model Intelligence (network effect): privacy-safe, aggregate-only digest entries
-- contributed by other LightTrack instances. No raw text, no project/customer ids — only public model
-- identities + aggregate quality/cost/latency. The hub merges these into a public leaderboard.
-- PK (contributor_id, provider, model, task_type) so a re-contribution upserts in place.
CREATE TABLE IF NOT EXISTS collective_entries (
  contributor_id  TEXT NOT NULL,   -- opaque, non-reversible source id (a hash)
  provider        TEXT NOT NULL,
  model           TEXT NOT NULL,
  task_type       TEXT NOT NULL,   -- coarse bucket from a fixed vocabulary
  quality         REAL NOT NULL,   -- mean score 0..1
  pass_rate       REAL NOT NULL,
  avg_cost_usd    REAL NOT NULL,   -- per case
  p50_latency_ms  INTEGER,
  p95_latency_ms  INTEGER,
  n_runs          INTEGER NOT NULL DEFAULT 0,
  n_cases         INTEGER NOT NULL DEFAULT 0,
  received_at     TEXT NOT NULL,
  PRIMARY KEY (contributor_id, provider, model, task_type)
);
CREATE INDEX IF NOT EXISTS idx_collective_model ON collective_entries(provider, model, task_type);

-- Cloud→device relay queue (docs/RELAY.md): apps enqueue action_type + JSON params; the enrolled
-- local device leases due tasks over outbound HTTPS, runs them against its local action library
-- (Claude Code CLI), and settles each with succeeded | failed | deferred. Prompts/tools/credentials
-- live only on the device — the payload carries parameters, never instructions.
CREATE TABLE IF NOT EXISTS relay_tasks (
  id                  TEXT PRIMARY KEY,
  project_id          TEXT NOT NULL,
  source              TEXT,                            -- originator tag (which app enqueued it)
  action_type         TEXT NOT NULL,                   -- resolved against the device's library
  payload             TEXT,                            -- JSON params
  status              TEXT NOT NULL DEFAULT 'queued',  -- queued | leased | succeeded | dead
  attempts            INTEGER NOT NULL DEFAULT 0,      -- consumed on lease; Deferred hands one back
  max_attempts        INTEGER NOT NULL DEFAULT 4,
  retry_interval_secs INTEGER NOT NULL DEFAULT 18000,  -- 5h — one Claude subscription window
  idempotency_key     TEXT,
  device              TEXT,                            -- which device holds/held the lease
  lease_deadline      TEXT,                            -- expired lease => reclaimable (or dead)
  next_attempt_at     TEXT NOT NULL,                   -- not leasable before this (retry backoff)
  result              TEXT,                            -- JSON
  error               TEXT,
  created_at          TEXT NOT NULL,
  updated_at          TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_relay_due ON relay_tasks(status, next_attempt_at);
CREATE UNIQUE INDEX IF NOT EXISTS idx_relay_idem ON relay_tasks(project_id, idempotency_key)
  WHERE idempotency_key IS NOT NULL;
