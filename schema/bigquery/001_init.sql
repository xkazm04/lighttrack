-- LightTrack cloud store (BigQuery). Mirrors schema/sqlite/001_init.sql.
-- Replace ${DATASET} (e.g. `your-project.lighttrack`). Hot config (projects, api_keys,
-- limit_rules) lives in Firestore in the cloud; only analytical tables are listed here,
-- but the full set is provided so BigQuery-only deployments work too.
-- Partition events/scores by day for cheap time-range queries within the 1 TB/mo free tier.

CREATE TABLE IF NOT EXISTS `${DATASET}.events` (
  id                  STRING NOT NULL,
  project_id          STRING NOT NULL,
  trace_id            STRING,
  span_id             STRING,
  parent_span_id      STRING,
  ts                  TIMESTAMP NOT NULL,
  provider            STRING NOT NULL,
  model               STRING NOT NULL,
  operation           STRING,
  input_tokens        INT64,
  output_tokens       INT64,
  cached_input_tokens INT64,
  reasoning_tokens    INT64,
  cost_usd            FLOAT64,
  latency_ms          INT64,
  status              STRING,
  error               STRING,
  input               JSON,
  output              JSON,
  tags                ARRAY<STRING>,
  source              STRING,
  metadata            JSON
)
PARTITION BY DATE(ts)
CLUSTER BY project_id, provider, model;

CREATE TABLE IF NOT EXISTS `${DATASET}.scores` (
  id          STRING NOT NULL,
  project_id  STRING NOT NULL,
  event_id    STRING,
  rubric      STRING NOT NULL,
  value       FLOAT64 NOT NULL,
  max         FLOAT64,
  pass        BOOL,
  reasoning   STRING,
  scored_by   STRING NOT NULL,
  cost_usd    FLOAT64,
  created_at  TIMESTAMP NOT NULL
)
PARTITION BY DATE(created_at)
CLUSTER BY project_id, rubric;

CREATE TABLE IF NOT EXISTS `${DATASET}.benchmarks` (
  id             STRING NOT NULL,
  project_id     STRING NOT NULL,
  name           STRING NOT NULL,
  rubric         STRING NOT NULL,
  judge_model    STRING NOT NULL,
  target         JSON,
  dataset_ref    STRING,
  dataset        JSON,         -- array of {input, expected?, output?}
  rubric_id      STRING,
  baseline_score FLOAT64,
  created_at     TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS `${DATASET}.rubrics` (
  id          STRING NOT NULL,
  project_id  STRING NOT NULL,
  name        STRING NOT NULL,
  dimensions  JSON NOT NULL,
  threshold   FLOAT64,
  created_at  TIMESTAMP NOT NULL
);

-- Background job queue (Phase 3.6d). On GCP this is typically replaced by Pub/Sub.
CREATE TABLE IF NOT EXISTS `${DATASET}.jobs` (
  id           STRING NOT NULL,
  type         STRING NOT NULL,
  payload      JSON,
  status       STRING NOT NULL,
  attempts     INT64,
  max_attempts INT64,
  progress     STRING,
  error        STRING,
  result       JSON,
  claimed_at   TIMESTAMP,
  created_at   TIMESTAMP NOT NULL,
  updated_at   TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS `${DATASET}.benchmark_runs` (
  id             STRING NOT NULL,
  benchmark_id   STRING NOT NULL,
  started_at     TIMESTAMP NOT NULL,
  finished_at    TIMESTAMP,
  n_cases        INT64,
  mean_score     FLOAT64,
  pass_rate      FLOAT64,
  cost_usd       FLOAT64,
  status         STRING,
  p50_latency_ms INT64,
  p95_latency_ms INT64,
  total_tokens   INT64,
  report         JSON
);

CREATE TABLE IF NOT EXISTS `${DATASET}.model_prices` (
  provider              STRING NOT NULL,
  model                 STRING NOT NULL,
  input_per_mtok        FLOAT64 NOT NULL,
  output_per_mtok       FLOAT64 NOT NULL,
  cached_input_per_mtok FLOAT64,
  effective_date        TIMESTAMP NOT NULL,
  source_url            STRING
);

CREATE TABLE IF NOT EXISTS `${DATASET}.datasets` (
  id          STRING NOT NULL,
  project_id  STRING NOT NULL,
  name        STRING NOT NULL,
  version     INT64,
  frozen      BOOL,
  source      STRING,
  created_at  TIMESTAMP NOT NULL
);

CREATE TABLE IF NOT EXISTS `${DATASET}.dataset_items` (
  id              STRING NOT NULL,
  dataset_id      STRING NOT NULL,
  input           STRING NOT NULL,
  output          STRING,
  expected        STRING,
  context         STRING,
  tags            ARRAY<STRING>,
  source_event_id STRING,
  anonymization   JSON
);

-- Example free-tier-friendly rollup the notifier / Looker Studio can use:
-- SELECT project_id, DATE(ts) d, SUM(cost_usd) cost, COUNT(*) calls
-- FROM `${DATASET}.events` WHERE ts > TIMESTAMP_SUB(CURRENT_TIMESTAMP(), INTERVAL 30 DAY)
-- GROUP BY project_id, d ORDER BY d DESC;
