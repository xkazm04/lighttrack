//! Shared application state + the blocking-DB call helper.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use lighttrack_billing::BillingRegistry;
use lighttrack_core::{PriceBook, Redaction};
use lighttrack_store::{Store, StoreError};

use crate::alerts::Alerter;
use crate::auth::AuthMode;
use crate::collective::Collective;
use crate::error::ApiError;
use crate::idempotency::SeenWebhooks;
use crate::redact::Redactor;
use crate::rejections::RejectionLedger;

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) store: Arc<dyn Store + Send + Sync>,
    /// DB-backed price book, hot-swappable via `PUT /v1/prices/:provider/:model`.
    pub(crate) prices: Arc<RwLock<PriceBook>>,
    pub(crate) auth_mode: AuthMode,
    pub(crate) admin_key: Option<String>,
    /// The enrolled local device's bearer key for the relay lease/result endpoints
    /// (`LIGHTTRACK_RELAY_DEVICE_KEY`). Unset ⇒ only admin/dev principals may drive them.
    pub(crate) relay_device_key: Option<String>,
    /// Fixed per-request cost stamped on relay-run events (`LIGHTTRACK_RELAY_FLAT_COST_USD`,
    /// default $1.00). Subscription runs have no metered price; a flat rate gives a solid usage
    /// overview until token-precise costing is worth wiring up (docs/RELAY.md).
    pub(crate) relay_flat_cost: f64,
    /// Best-effort breach-alert delivery (webhook / ntfy), configured from env.
    pub(crate) alerts: Arc<Alerter>,
    /// Optional PII redaction of captured input/output on ingest, configured from env.
    pub(crate) redact: Arc<Redactor>,
    /// Per-project payload-persistence policies (the stored `Project.redaction` field), cached so the
    /// ingest hot path doesn't pay a store read per event. See [`ProjectPolicyCache`] for the freshness
    /// contract — this used to be a plain map that never invalidated, which meant *tightening* a
    /// project's redaction for compliance did nothing until the process restarted.
    pub(crate) project_policies: Arc<ProjectPolicyCache>,
    /// Configured billing-webhook sources (Stripe/Polar), keyed by provider.
    pub(crate) billing: Arc<BillingRegistry>,
    /// In-process idempotency for webhook deliveries — collapses provider retries / duplicate
    /// deliveries of the same event so they aren't reprocessed (durable backstop: deterministic
    /// `revenue_events.id` upsert).
    pub(crate) seen_webhooks: Arc<SeenWebhooks>,
    /// Collective Model Intelligence config (opaque contributor id + hub accept flag), from env.
    pub(crate) collective: Arc<Collective>,
    /// Best-effort, process-local ledger of ingest attempts that limit rules rejected (429). Rejected
    /// events are deliberately never stored (they'd corrupt usage/cost), so this counts them out-of-band
    /// so history isn't blind exactly when a cap bites. Resets on restart; entries roll off after 24h.
    pub(crate) rejections: Arc<RejectionLedger>,
    /// Bounded-concurrency gate + deadline for the ingest routes, and the shed/timeout counters
    /// behind `GET /v1/ingest/status`. Admission control for *load*, orthogonal to the spend limits
    /// above — see [`crate::shed`].
    pub(crate) ingest_guard: Arc<crate::shed::IngestGuard>,
    /// Per-source budget for **failed** credential attempts. A third, independent axis: `ingest_guard`
    /// bounds concurrent load and the limit rules bound spend, but neither bounded how fast an
    /// attacker could guess the (operator-chosen, possibly weak) admin key — see
    /// [`crate::auth_throttle`].
    pub(crate) auth_throttle: Arc<crate::auth_throttle::AuthThrottle>,
    /// Live count of in-flight requests — the activity gauge the quiet-window maintenance sweep
    /// gates on. Fed by a middleware over the WHOLE router (not just ingest), because a long
    /// analytical read is exactly the foreground work a checkpoint must not compete with.
    /// See [`crate::storage`].
    pub(crate) activity: Arc<crate::storage::ActivityGauge>,
    /// The maintenance flight recorder: every pass, including the deferred ones, behind
    /// `GET /v1/storage/status`.
    pub(crate) maintenance: Arc<crate::storage::Maintenance>,
    /// The sweep's configuration as one readable line, so the status surface can say whether
    /// anything will ever checkpoint this database.
    pub(crate) maintenance_desc: String,
    /// Per-(policy, subject) last-applied instants for the margin guardrail pass. Process-local, in
    /// the same spirit as the alert cooldowns: it exists so a policy with a long cooldown does not
    /// rewrite its rule on every sweep tick.
    pub(crate) policy_cooldowns: Arc<crate::margin_guardrails::PolicyCooldowns>,
}

/// Env: how long a cached redaction policy may be served before it is re-read from the store.
/// `0` disables caching entirely (every ingest re-reads the project). Default [`DEFAULT_POLICY_TTL`].
const ENV_POLICY_TTL: &str = "LIGHTTRACK_REDACTION_CACHE_TTL_SECS";
const DEFAULT_POLICY_TTL: Duration = Duration::from_secs(60);

/// The slice of a project row the ingest path enforces on every event: the payload-persistence
/// policy, and whether the project is accepting events at all. Cached together because they are
/// read together, on the same hot path, from the same row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ProjectPolicy {
    pub(crate) redaction: Redaction,
    /// `Project.enabled`. A disabled project's keys still authenticate (its operators keep reading),
    /// but neither ingest door records an event for it — the switch the projects API accepts is a
    /// switch, not a label.
    pub(crate) enabled: bool,
}

impl Default for ProjectPolicy {
    /// What ingest assumes when there is no project row yet (keyless dev traffic before the default
    /// project is bootstrapped): store as sent, accept.
    fn default() -> Self {
        Self {
            redaction: Redaction::default(),
            enabled: true,
        }
    }
}

impl From<&lighttrack_core::Project> for ProjectPolicy {
    fn from(p: &lighttrack_core::Project) -> Self {
        Self {
            redaction: p.redaction,
            enabled: p.enabled,
        }
    }
}

/// Read-through cache of per-project ingest policies, with two independent freshness guarantees — a
/// redaction policy is a *compliance* control and `enabled` is a kill switch, so "eventually, after
/// a restart" is not an acceptable propagation story:
///
/// 1. **Explicit invalidation.** `PUT /v1/projects/:id` drops the entry, so a tightening made through
///    this instance takes effect on the very next event (see [`ProjectPolicyCache::invalidate`]).
/// 2. **A TTL bound.** Entries expire after [`ENV_POLICY_TTL`], which bounds staleness for changes
///    this instance did *not* make — another replica's write, or a direct DB edit. This is the slice:
///    a bounded window, not a cross-replica invalidation bus.
pub(crate) struct ProjectPolicyCache {
    entries: RwLock<HashMap<String, (ProjectPolicy, Instant)>>,
    ttl: Duration,
}

impl ProjectPolicyCache {
    /// Build a cache pre-warmed with the policies read at startup, with the TTL resolved from env.
    pub(crate) fn new(warm: HashMap<String, ProjectPolicy>) -> Self {
        let ttl = std::env::var(ENV_POLICY_TTL)
            .ok()
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_POLICY_TTL);
        let now = Instant::now();
        let entries = warm.into_iter().map(|(k, v)| (k, (v, now))).collect();
        Self {
            entries: RwLock::new(entries),
            ttl,
        }
    }

    /// The cached policy for `pid`, or `None` when absent or expired.
    fn get_fresh(&self, pid: &str) -> Option<ProjectPolicy> {
        if self.ttl.is_zero() {
            return None;
        }
        let entries = self.entries.read().ok()?;
        let (policy, stamped) = entries.get(pid)?;
        (stamped.elapsed() < self.ttl).then_some(*policy)
    }

    /// Remember `policy` for `pid`, restamping its freshness clock.
    pub(crate) fn put(&self, pid: &str, policy: ProjectPolicy) {
        if let Ok(mut e) = self.entries.write() {
            e.insert(pid.to_string(), (policy, Instant::now()));
        }
    }

    /// Forget `pid` so the next ingest re-reads it from the store. Called when a project is updated
    /// through this instance — the path that makes a tightening effective *immediately*.
    pub(crate) fn invalidate(&self, pid: &str) {
        if let Ok(mut e) = self.entries.write() {
            e.remove(pid);
        }
    }
}

/// The ingest policy for `pid`, from the cache — falling back to one store read when the cache has
/// no fresh answer (then remembered, including the "no project row" default, so a hot project costs
/// at most one read per TTL). This is what makes the stored `Project.redaction` and `Project.enabled`
/// fields *enforced* on ingest instead of decorative columns.
pub(crate) async fn project_policy_for(
    st: &AppState,
    pid: &str,
) -> Result<ProjectPolicy, ApiError> {
    if let Some(p) = st.project_policies.get_fresh(pid) {
        return Ok(p);
    }
    let store = st.store.clone();
    let id = pid.to_string();
    let policy = spawn_db(move || store.get_project(&id))
        .await?
        .as_ref()
        .map(ProjectPolicy::from)
        .unwrap_or_default();
    st.project_policies.put(pid, policy);
    Ok(policy)
}

/// Run a blocking store call on the blocking pool and flatten the two error layers.
pub(crate) async fn spawn_db<T, F>(f: F) -> Result<T, ApiError>
where
    F: FnOnce() -> Result<T, StoreError> + Send + 'static,
    T: Send + 'static,
{
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| ApiError::internal(format!("task join error: {e}")))?
        .map_err(ApiError::from)
}
