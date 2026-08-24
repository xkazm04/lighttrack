//! SQLite-backed [`Store`] — the local-development backend (bundled SQLite, no external service).
//!
//! `SqliteStore` delegates every method to a per-domain submodule of free functions over a
//! `&Connection` (`events`, `scores`, `projects`, `benchmarks`, `datasets`, `rubrics`, `prices`,
//! `jobs`). The timestamp/enum/JSON codecs are shared across all backends — see [`crate::codec`].
//!
//! **Concurrency.** Writes serialize behind one mutex-guarded connection; reads are served from a
//! pool of read-only connections ([`pool`]) that, under WAL, take a consistent snapshot without
//! blocking or being blocked by ingest. See [`SqliteStore::read`] vs [`SqliteStore::with`].

mod benchmarks;
mod collective;
mod datasets;
mod events;
mod forecast;
mod jobs;
mod limits;
mod pool;
mod prices;
mod projects;
mod prompts;
mod relay;
mod revenue;
mod rubrics;
mod schema;
mod scores;
mod usage_cache;

#[cfg(test)]
mod bench;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod tests_concurrency;

use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde_json::Value;

use lighttrack_core::{
    ApiKey, Benchmark, BenchmarkRun, CollectiveEntry, CostByDimension, Dataset, DatasetItem, Job,
    JobCancel, JobFinish, LimitRule, LimitScope, LlmEvent, ModelPriceRow, Project, Prompt,
    PromptVersion, RelayOutcome, RelayTask, RevenueEvent, Rubric, Score, TokensByDimension,
    TraceSummary,
};

use crate::{
    Admission, CostRow, CustomerCostRow, DailyDimCost, DailyUsage, EventFilter, EventPage, Result,
    ScopeUsage, Store, StoreError, TraceEvents, TraceFilter, TracePage, Usage, UseCaseCostRow,
};

/// A **sargable** project predicate for `?1`-bound project queries. When a project is given this is an
/// index-seekable equality (`project_id = ?1`), so a `WHERE project_pred(..) AND ts >= ?2 AND ts < ?3`
/// rides `idx_events_project_ts`; when it is absent it is a constant TRUE (the all-projects path is
/// inherently a scan). This replaces the `(?1 IS NULL OR project_id = ?1)` form, which the SQLite
/// planner cannot use the index for **even when a project IS given** — the `OR` with a non-column
/// condition forces a full table scan of `events` (across all projects and all time) under the global
/// connection mutex on every windowed cost/usage/forecast read. The `?1` binding is unchanged in both
/// arms, so callers bind exactly as before.
pub(super) fn project_pred(project: Option<&str>) -> &'static str {
    if project.is_some() {
        "project_id = ?1"
    } else {
        "?1 IS NULL"
    }
}

/// How a store is opened. Only [`SqliteStore::open`]'s defaults ship; the knobs exist so tests and
/// the throughput harness can construct the pre-WAL, single-connection store for comparison.
pub(crate) struct OpenOpts {
    /// Request WAL journalling. Ignored (and impossible) for in-memory databases.
    pub(crate) wal: bool,
    /// Read-only connections to pre-open. `0` routes reads back through the write connection.
    pub(crate) read_pool: usize,
}

impl Default for OpenOpts {
    fn default() -> Self {
        Self {
            wal: true,
            read_pool: pool::configured_size(),
        }
    }
}

/// SQLite store: one serialized write connection plus a pool of read-only connections.
pub struct SqliteStore {
    /// The **write** connection. Every mutation — and every admission decision — serializes here,
    /// so the check-then-insert critical section is exactly as atomic as it was when this mutex
    /// guarded reads too.
    conn: Mutex<Connection>,
    /// Read-only connections, used by every side-effect-free `Store` method. Empty when WAL is
    /// unavailable or an in-memory database is in play, in which case reads fall back to `conn`.
    readers: pool::ReadPool,
    /// Incremental rolling-usage totals for admission control, so a cap check costs `O(new events)`
    /// instead of re-aggregating the whole window. Locked *before* `conn` in the two admission
    /// methods, so the count-then-insert stays one atomic critical section. See [`usage_cache`].
    usage_cache: Mutex<usage_cache::UsageCache>,
}

impl SqliteStore {
    /// Open (creating parent dirs and the file if needed) and ensure the schema exists.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        Self::open_with(path, OpenOpts::default())
    }

    pub(crate) fn open_with<P: AsRef<Path>>(path: P, opts: OpenOpts) -> Result<Self> {
        let path = path.as_ref();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let conn = Connection::open(path)?;
        conn.busy_timeout(pool::BUSY_TIMEOUT)?;
        // Migrate on the write connection *before* opening readers: a read-only connection can't
        // apply an ALTER, and pooled readers must never observe a pre-migration schema.
        schema::apply(&conn)?;
        // After the schema batch, which requests WAL itself but swallows the answer — journal mode is
        // a property of the file, so an existing pre-WAL database is upgraded here on open.
        let wal = set_journal_mode(&conn, opts.wal)?;
        // A pool without WAL would be a pessimisation, not an optimisation: under the rollback
        // journal a reader's SHARED lock blocks the writer outright. Fall back to the pre-pool
        // behavior (all calls through the write connection) and say so.
        let readers = if wal && opts.read_pool > 0 {
            pool::ReadPool::open(path, opts.read_pool)?
        } else {
            if opts.wal && !wal {
                eprintln!(
                    "lighttrack-store: WAL journalling unavailable for {} — reads will serialize \
                     behind writes (is the database on a network filesystem?)",
                    path.display()
                );
            }
            pool::ReadPool::disabled()
        };
        Ok(Self {
            conn: Mutex::new(conn),
            readers,
            usage_cache: Mutex::new(usage_cache::UsageCache::default()),
        })
    }

    /// In-memory store, for tests. A `:memory:` database is private to its connection, so there is
    /// no read pool (and no WAL) — every call goes through the single connection, as before.
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        schema::apply(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            readers: pool::ReadPool::disabled(),
            usage_cache: Mutex::new(usage_cache::UsageCache::default()),
        })
    }

    /// Run a closure with the locked **write** connection. Anything that mutates goes through here.
    fn with<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        f(&self.conn.lock().unwrap())
    }

    /// Run a **read-only** closure on a pooled connection, concurrently with any in-flight write.
    /// Falls back to the write connection when the pool is disabled.
    fn read<R>(&self, f: impl FnOnce(&Connection) -> R) -> R {
        match self.readers.acquire() {
            Some(c) => f(&c),
            None => self.with(f),
        }
    }
}

/// Set the journal mode and report whether WAL actually took effect. `PRAGMA journal_mode` *returns
/// the resulting mode* rather than failing when it can't change it (network filesystems, in-memory
/// databases), so the answer has to be read back, not assumed — the read pool's correctness depends
/// on it. `wal == false` forces the rollback journal, which only the comparison harness asks for.
fn set_journal_mode(conn: &Connection, wal: bool) -> Result<bool> {
    let sql = if wal {
        "PRAGMA journal_mode=WAL"
    } else {
        "PRAGMA journal_mode=DELETE"
    };
    let mode: String = conn.query_row(sql, [], |r| r.get(0))?;
    Ok(mode.eq_ignore_ascii_case("wal"))
}

impl Store for SqliteStore {
    fn init_schema(&self) -> Result<()> {
        self.with(schema::apply)
    }

    // --- events ---
    fn insert_event(&self, ev: &LlmEvent) -> Result<()> {
        self.with(|c| events::insert(c, ev))
    }
    fn admission_is_atomic(&self) -> bool {
        // One locked write connection (+ the usage-cache lock) spans check-count-insert, within a
        // single process. The multi-process caveat is documented in docs/ARCHITECTURE.md.
        true
    }
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        // Lock the usage cache *before* the connection (consistent order in both admission methods,
        // so no deadlock) and hold both across the check-count-insert — one atomic critical section.
        let mut cache = self.usage_cache.lock().unwrap();
        self.with(|c| events::insert_checked(c, &mut cache, ev))
    }
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        // One critical section for the whole batch: the cache + connection locks are held across every
        // item, so each accepted insert is already visible to the next item's usage read (no cap
        // bypass), and the check-then-insert stays atomic against concurrent ingest.
        //
        // One TRANSACTION for the whole batch too: without it every item is its own autocommit —
        // one fsync per event, so a 500-item batch held the global connection lock for ~500 fsyncs
        // (~0.5-2.5s of cluster-wide ingest stall) and the "efficient" batch path was worse than 500
        // interleaved single posts. A per-item failure (e.g. duplicate id) does not poison the
        // transaction — the survivors still commit, preserving the "previously-accepted items stay
        // committed" contract the batch response is built on. `unchecked_transaction` is sound here
        // because we already hold the connection mutex (same justification as `revenue::insert_batch`).
        //
        // Limit rules are also hoisted: fetched once per distinct project (a batch is single-project
        // by construction today) instead of re-queried per item.
        let mut cache = self.usage_cache.lock().unwrap();
        self.with(|c| {
            let tx = match c.unchecked_transaction() {
                Ok(tx) => tx,
                Err(e) => {
                    let msg = format!("batch begin failed: {e}");
                    return evs
                        .iter()
                        .map(|_| Err(StoreError::Other(msg.clone())))
                        .collect();
                }
            };
            let mut rules_by_project: std::collections::HashMap<&str, Vec<LimitRule>> =
                std::collections::HashMap::new();
            let mut out: Vec<Result<Admission>> = Vec::with_capacity(evs.len());
            for e in evs {
                let rules = match rules_by_project.entry(e.project_id.as_str()) {
                    std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
                    std::collections::hash_map::Entry::Vacant(v) => {
                        match limits::list(&tx, &e.project_id, true) {
                            Ok(r) => v.insert(r),
                            Err(err) => {
                                out.push(Err(err));
                                continue;
                            }
                        }
                    }
                };
                out.push(events::insert_checked_with_rules(&tx, &mut cache, e, rules));
            }
            if let Err(e) = tx.commit() {
                // The whole batch is lost (all-or-nothing beats a torn batch a client can't detect).
                // The in-memory usage cache may now over-count the uncommitted events — the safe
                // direction (over-enforcement), and it re-syncs from the table on its next rebuild.
                let msg = format!("batch commit failed: {e}");
                return evs
                    .iter()
                    .map(|_| Err(StoreError::Other(msg.clone())))
                    .collect();
            }
            out
        })
    }
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        self.read(|c| events::list(c, project, limit))
    }
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        self.read(|c| events::list_filtered(c, project, filter, limit))
    }
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>> {
        self.read(|c| events::cost_summary(c, project))
    }
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        self.read(|c| events::cost_summary_windowed(c, project, since, until))
    }
    fn usecase_costs(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        self.read(|c| events::usecase_costs(c, project, since))
    }
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage> {
        self.read(|c| events::usage_since(c, project, since))
    }
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        scope: &LimitScope,
    ) -> Result<Usage> {
        self.read(|c| events::usage_since_scoped(c, project, since, scope))
    }
    fn usage_by_scope(
        &self,
        project: &str,
        since: DateTime<Utc>,
        kind: &str,
    ) -> Result<Vec<ScopeUsage>> {
        self.read(|c| events::usage_by_scope(c, project, since, kind))
    }
    fn daily_usage(
        &self,
        project: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyUsage>> {
        self.read(|c| forecast::daily_usage(c, project, since, until))
    }
    fn daily_cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyDimCost>> {
        self.read(|c| forecast::daily_cost_by_dimension(c, project, dim, since, until))
    }
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>> {
        self.read(|c| events::get(c, id))
    }

    // --- traces ---
    fn serves_traces(&self) -> bool {
        true
    }
    fn list_traces(&self, project: Option<&str>, limit: usize) -> Result<Vec<TraceSummary>> {
        self.read(|c| events::list_trace_summaries(c, project, limit))
    }
    fn list_traces_filtered(
        &self,
        project: Option<&str>,
        filter: &TraceFilter,
        limit: usize,
    ) -> Result<TracePage> {
        self.read(|c| events::list_trace_summaries_filtered(c, project, filter, limit))
    }
    fn list_trace_events(
        &self,
        project: Option<&str>,
        trace_id: &str,
        max_spans: usize,
    ) -> Result<TraceEvents> {
        self.read(|c| events::list_by_trace(c, project, trace_id, max_spans))
    }
    fn list_trace_scores(&self, project: Option<&str>, trace_id: &str) -> Result<Vec<Score>> {
        self.read(|c| scores::list_by_trace(c, project, trace_id))
    }

    // --- scores ---
    fn insert_score(&self, s: &Score) -> Result<()> {
        self.with(|c| scores::insert(c, s))
    }
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>> {
        self.read(|c| scores::list(c, project, limit))
    }
    fn list_run_scores(
        &self,
        run_id: &str,
        project: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Score>> {
        self.read(|c| scores::list_by_run(c, run_id, project, limit))
    }
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>> {
        self.read(|c| scores::scored_event_ids(c, event_ids))
    }

    // --- projects / api keys / limits ---
    fn create_project(&self, p: &Project) -> Result<()> {
        self.with(|c| projects::create(c, p))
    }
    fn update_project(&self, p: &Project) -> Result<bool> {
        self.with(|c| projects::update(c, p))
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>> {
        self.read(|c| projects::get(c, id))
    }
    fn list_projects(&self) -> Result<Vec<Project>> {
        self.read(projects::list)
    }
    fn create_api_key(&self, k: &ApiKey) -> Result<()> {
        self.with(|c| projects::create_key(c, k))
    }
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>> {
        self.read(|c| projects::find_key_by_prefix(c, prefix))
    }
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()> {
        self.with(|c| projects::touch_key(c, id, when))
    }
    fn list_api_keys(&self, project: &str) -> Result<Vec<ApiKey>> {
        self.read(|c| projects::list_keys(c, project))
    }
    fn set_api_key_revoked(&self, id: &str, revoked: bool) -> Result<bool> {
        self.with(|c| projects::set_key_revoked(c, id, revoked))
    }
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()> {
        self.with(|c| limits::create(c, r))
    }
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>> {
        self.read(|c| limits::list(c, project, only_enabled))
    }
    fn get_limit_rule(&self, id: &str) -> Result<Option<LimitRule>> {
        self.read(|c| limits::get(c, id))
    }
    fn update_limit_rule(&self, r: &LimitRule) -> Result<bool> {
        self.with(|c| limits::update(c, r))
    }
    fn delete_limit_rule(&self, id: &str) -> Result<bool> {
        self.with(|c| limits::delete(c, id))
    }

    // --- benchmarks ---
    fn create_benchmark(&self, b: &Benchmark) -> Result<()> {
        self.with(|c| benchmarks::create(c, b))
    }
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>> {
        self.read(|c| benchmarks::get(c, id))
    }
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>> {
        self.read(|c| benchmarks::list(c, project))
    }
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()> {
        self.with(|c| benchmarks::create_run(c, r))
    }
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>> {
        self.read(|c| benchmarks::list_runs(c, benchmark_id))
    }

    // --- prices ---
    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()> {
        self.with(|c| prices::upsert(c, p))
    }
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>> {
        self.read(prices::list)
    }

    // --- datasets ---
    fn create_dataset(&self, d: &Dataset) -> Result<()> {
        self.with(|c| datasets::create(c, d))
    }
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>> {
        self.read(|c| datasets::get(c, id))
    }
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>> {
        self.read(|c| datasets::list(c, project))
    }
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()> {
        self.with(|c| datasets::set_frozen(c, id, frozen))
    }
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()> {
        self.with(|c| datasets::create_item(c, item))
    }
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>> {
        self.read(|c| datasets::list_items(c, dataset_id))
    }

    // --- rubrics ---
    fn create_rubric(&self, r: &Rubric) -> Result<()> {
        self.with(|c| rubrics::create(c, r))
    }
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>> {
        self.read(|c| rubrics::get(c, id))
    }
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>> {
        self.read(|c| rubrics::list(c, project))
    }

    // --- jobs ---
    fn create_job(&self, j: &Job) -> Result<()> {
        self.with(|c| jobs::create(c, j))
    }
    fn claim_job(&self, stale_before: DateTime<Utc>) -> Result<Option<Job>> {
        self.with(|c| jobs::claim(c, stale_before))
    }
    fn cancel_job(&self, id: &str) -> Result<Option<JobCancel>> {
        self.with(|c| jobs::cancel(c, id))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()> {
        self.with(|c| jobs::update_progress(c, id, progress))
    }
    fn renew_job_lease(&self, id: &str, fence: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        self.with(|c| jobs::renew_lease(c, id, fence))
    }
    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: &Value,
        error: Option<&str>,
        fence: Option<DateTime<Utc>>,
    ) -> Result<JobFinish> {
        self.with(|c| jobs::finish(c, id, status, result, error, fence))
    }
    fn get_job(&self, id: &str) -> Result<Option<Job>> {
        self.read(|c| jobs::get(c, id))
    }
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>> {
        self.read(|c| jobs::list(c, status, limit))
    }

    // --- prompt registry ---
    fn create_prompt(&self, p: &Prompt) -> Result<()> {
        self.with(|c| prompts::create(c, p))
    }
    fn update_prompt(&self, p: &Prompt) -> Result<()> {
        self.with(|c| prompts::update(c, p))
    }
    fn get_prompt(&self, project: &str, name: &str) -> Result<Option<Prompt>> {
        self.read(|c| prompts::get(c, project, name))
    }
    fn get_prompt_by_id(&self, id: &str) -> Result<Option<Prompt>> {
        self.read(|c| prompts::get_by_id(c, id))
    }
    fn list_prompts(&self, project: &str) -> Result<Vec<Prompt>> {
        self.read(|c| prompts::list(c, project))
    }
    fn create_prompt_version(&self, v: &PromptVersion) -> Result<()> {
        self.with(|c| prompts::create_version(c, v))
    }
    fn get_prompt_version(&self, prompt_id: &str, version: u32) -> Result<Option<PromptVersion>> {
        self.read(|c| prompts::get_version(c, prompt_id, version))
    }
    fn list_prompt_versions(&self, prompt_id: &str) -> Result<Vec<PromptVersion>> {
        self.read(|c| prompts::list_versions(c, prompt_id))
    }

    // --- revenue + margin (Phase 1 profit tracking) ---
    fn insert_revenue_event(&self, ev: &RevenueEvent) -> Result<()> {
        self.with(|c| revenue::insert(c, ev))
    }
    fn insert_revenue_events(&self, evs: &[RevenueEvent]) -> Result<()> {
        self.with(|c| revenue::insert_batch(c, evs))
    }
    fn list_revenue_events(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        self.read(|c| revenue::list(c, project, since, until))
    }
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        self.read(|c| revenue::cost_by_dimension(c, project, dim, since, until))
    }
    fn tokens_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<TokensByDimension>> {
        self.read(|c| revenue::tokens_by_dimension(c, project, dim, since, until))
    }
    fn customer_cost_by_model(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        self.read(|c| revenue::customer_cost_by_model(c, project, customer, since, until))
    }
    fn customer_cost_by_name(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        self.read(|c| revenue::customer_cost_by_name(c, project, customer, since, until))
    }

    // --- cloud→device relay queue ---
    fn create_relay_task(&self, t: &RelayTask) -> Result<()> {
        self.with(|c| relay::create(c, t))
    }
    fn get_relay_task(&self, id: &str) -> Result<Option<RelayTask>> {
        self.read(|c| relay::get(c, id))
    }
    fn find_relay_task_by_key(&self, project: &str, key: &str) -> Result<Option<RelayTask>> {
        self.read(|c| relay::find_by_key(c, project, key))
    }
    fn list_relay_tasks(
        &self,
        project: Option<&str>,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<RelayTask>> {
        self.read(|c| relay::list(c, project, status, limit))
    }
    fn lease_relay_tasks(
        &self,
        device: &str,
        lease_secs: i64,
        max: usize,
    ) -> Result<Vec<RelayTask>> {
        self.with(|c| relay::lease(c, device, lease_secs, max))
    }
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        self.with(relay::sweep_dead)
    }
    fn settle_relay_task(&self, id: &str, outcome: &RelayOutcome) -> Result<Option<RelayTask>> {
        self.with(|c| relay::settle(c, id, outcome))
    }

    // --- collective model intelligence ---
    fn upsert_collective_entry(&self, e: &CollectiveEntry) -> Result<()> {
        self.with(|c| collective::upsert(c, e))
    }
    fn delete_collective_entries(&self, contributor_id: &str) -> Result<u64> {
        self.with(|c| collective::delete(c, contributor_id))
    }
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        self.read(collective::list)
    }
    fn purge_collective_entries_before(&self, cutoff: DateTime<Utc>) -> Result<u64> {
        self.with(|c| collective::purge_before(c, cutoff))
    }
}
