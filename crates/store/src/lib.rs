//! LightTrack persistence layer.
//!
//! [`Store`] is the backend-agnostic interface used by `api` (and later `mcp`/`cli`). The local
//! implementation is [`sqlite::SqliteStore`]; cloud backends slot in behind the same trait, selected
//! by `LIGHTTRACK_DATABASE_URL`: `lighttrack-store-pg` (Postgres, the cross-cloud default) and
//! `lighttrack-store-firestore` (GCP-native). See `docs/PACKAGING.md`.
//!
//! Methods are synchronous (SQLite is blocking). Async callers wrap them in `spawn_blocking`.

pub mod capabilities;
pub mod codec;
pub mod collective;
pub mod conformance;
pub mod pricing;
mod rollup_compat;
pub mod sqlite;
pub mod threshold;

use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::Value;
use thiserror::Error;

use lighttrack_core::{
    scope_matches, Alert, AlertChannel, AlertKind, ApiKey, Benchmark, BenchmarkRun,
    CollectiveEntry, ContributionRecord, CostByDimension, CostEvidence, Dataset, DatasetItem,
    Delivery, Device, DeviceEligibility, Job, JobCancel, JobFinish, LeaseHeld, LimitMetric,
    LimitRule, LimitScope, LimitStatus, LimitWindow, LlmEvent, MarginPolicy, ModelPriceRow,
    Project, Prompt, PromptVersion, RedactionStamp, RelayCancel, RelayOutcome, RelaySettle,
    RelayTask, RevenueEvent, RollupQuery, RollupRow, Rubric, Schedule, Score, ThresholdBasis,
    TokensByDimension, Trace, TraceSummary, UnpricedRow,
};

pub use capabilities::{Capabilities, Surface};
pub use collective::{replace_collective_contribution_nonatomic, CollectiveFilter, ReplaceAck};
pub use sqlite::SqliteStore;
pub use threshold::{
    needs_revenue, resolve_all as resolve_thresholds, resolve_from_windows, resolver,
    revenue_subject, window_key, RevenueWindows,
};

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    /// A write violated a uniqueness/primary-key constraint (e.g. a duplicate event `id`). Distinct
    /// from `Other` so the API can map it to a 409 Conflict instead of an opaque 500. Backends that
    /// don't classify constraint violations simply never produce it (their duplicate writes surface
    /// as `Sqlite`/`Other`, i.e. current behavior) — SQLite detects and raises it.
    #[error("conflict: {0}")]
    Conflict(String),
    /// The backend has not ported this capability. Distinct from `Other` so the API can answer 501
    /// (`unsupported`) instead of an opaque 500 — and so a permanent capability gap is never
    /// mistaken for a transient outage (or, worse, for "no data": trait defaults used to return
    /// empty results here, which read as authoritative zeros on unported backends). The payload
    /// names the capability; the full message is stable enough to log but not to parse.
    #[error("{0} is not supported by this store backend")]
    Unsupported(&'static str),
    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, StoreError>;

/// A cost/usage rollup row (grouped by project + provider + model).
#[derive(Debug, Clone, Serialize)]
pub struct CostRow {
    pub project_id: String,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    /// How many of `calls` had no price on the row and so contributed `$0.00` to `cost_usd` — the
    /// disclosure that keeps a floor from reading as a total. Additive: absent on older payloads.
    #[serde(default)]
    pub unpriced_calls: i64,
}

/// Optional filters + keyset cursor for [`Store::list_events_filtered`]. All fields are additive and
/// AND-combined; `None` fields don't constrain. `cursor` is an opaque token minted by a previous page
/// ([`EventPage::next_cursor`]) — the backend decodes it into a `(ts, id)` keyset position.
#[derive(Debug, Clone, Default)]
pub struct EventFilter {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub trace_id: Option<String>,
    pub name: Option<String>,
    /// Call outcome: `success` | `error` | `timeout`. The first question asked when debugging.
    pub status: Option<String>,
    /// Match events carrying this tag (membership in the `tags` array, not a substring).
    pub tag: Option<String>,
    /// Match events whose `metadata` has this key. Combined with [`EventFilter::metadata_value`] it
    /// becomes an equality test — which is how you ask "everything for this customer", since the
    /// billing linkage rides in metadata rather than a column.
    pub metadata_key: Option<String>,
    pub metadata_value: Option<String>,
    /// Minimum resolved `cost_usd` (inclusive). Trace listing already had this; the flat event list
    /// did not, so "which individual calls are expensive" had no answer.
    pub min_cost: Option<f64>,
    /// Match events stamped by this scrubber rule set (`metadata.redaction.rules`) — the query that
    /// separates rows scrubbed by the current rules from rows scrubbed by a previous generation,
    /// which is the first thing anyone needs after a rule change.
    pub redaction_rules: Option<String>,
    /// Match events whose scrub replaced at least this many spans (inclusive). `Some(1)` is
    /// "everything the scrubber actually rewrote" — the candidate set for "did we mangle the
    /// evidence a judge read".
    pub min_redacted_spans: Option<u32>,
    /// Also compute the total number of matching events (ignoring the cursor and page limit), so a
    /// client can render "n of N" without paging the whole result set to count it. Opt-in: it costs a
    /// second aggregate query, which a plain "give me the latest 50" should not pay for.
    pub with_total: bool,
    pub cursor: Option<String>,
}

impl EventFilter {
    /// The first predicate set here that a backend without the extended event-query support can't
    /// honor, or `None` when this filter only uses the original fields.
    ///
    /// Backends call this and return [`StoreError::Unsupported`] → HTTP 501 instead of quietly
    /// ignoring the predicate. Silently ignoring it is the dangerous option: an operator asking
    /// "show me the errored calls" would get a page of successful ones and believe it, and a
    /// filter that returns *more* than asked reads as authoritative rather than broken.
    pub fn unsupported_extension(&self) -> Option<&'static str> {
        if self.status.is_some() {
            return Some("the `status` event filter");
        }
        if self.tag.is_some() {
            return Some("the `tag` event filter");
        }
        if self.metadata_key.is_some() {
            return Some("the metadata event filter");
        }
        if self.min_cost.is_some() {
            return Some("the `min_cost` event filter");
        }
        if self.redaction_rules.is_some() {
            return Some("the `redaction_rules` event filter");
        }
        if self.min_redacted_spans.is_some() {
            return Some("the `min_redacted_spans` event filter");
        }
        if self.with_total {
            return Some("the event total count");
        }
        None
    }
}

/// One redaction posture: a distinct stamp (or its absence) and how many events carry it.
///
/// A tuple would have served the store, but this report is also the body of
/// `GET /v1/projects/:id/redaction`, and an operator reading `[[null, 4210], [{...}, 17]]` cannot
/// tell which half is which. Named fields make the payload self-describing where it is read.
#[derive(Debug, Clone, Serialize)]
pub struct RedactionPostureRow {
    /// `None` for rows carrying no stamp at all — "we do not know what happened to these", never
    /// folded in with rows that recorded a deliberate no-scrub.
    pub stamp: Option<RedactionStamp>,
    pub events: u64,
}

/// Predicates over a verdict's **typed identity** (M9-C).
///
/// `kind` is carried as the wire string rather than a [`ScoreKind`] so a filter can name a kind this
/// binary does not know — a verdict written by a newer producer must be findable by an older reader
/// instead of being invisible to it. The API validates the spelling against `ScoreKind::ALL` before
/// it gets here, so a typo is a 400 rather than an empty page.
#[derive(Debug, Clone, Default)]
pub struct ScoreFilter {
    /// The rubric a verdict was judged against — the join the free-text label could never be.
    pub rubric_id: Option<String>,
    pub kind: Option<String>,
}

impl ScoreFilter {
    /// Whether this filter asks for anything at all. A backend can answer an empty filter with its
    /// plain listing, which is why the unfiltered path stays on `list_scores`.
    pub fn is_empty(&self) -> bool {
        self.rubric_id.is_none() && self.kind.is_none()
    }
}

/// What one [`Store::reprice_revenue`] pass did, or would do.
///
/// `matched` and `changed` are reported separately because they differ for a reason the caller must
/// see: a row that took the 1:1 fallback but carries no `amount_minor` (a manual post, a row written
/// before FX provenance existed) **matches** the correction and cannot **take** it — there is no
/// original figure to re-multiply, and deriving one from the bad `amount_usd` would launder the
/// error into a confident-looking number. A single count would hide those rows entirely.
#[derive(Debug, Clone, Serialize)]
pub struct RepriceReport {
    pub currency: String,
    pub rate: f64,
    /// The FX book version stamped onto the rows this pass changed.
    pub book_version: String,
    /// Unconverted rows in this currency.
    pub matched: u64,
    /// …of which this many were (or would be) actually restated.
    pub changed: u64,
    /// True when nothing was written. A dry run's `changed` is the count of rows a real run would
    /// move, so the two runs are directly comparable.
    pub dry_run: bool,
}

/// One page of events plus the cursor to fetch the next page (newest-first). `next_cursor` is `Some`
/// only when more rows exist beyond this page; pass it back as [`EventFilter::cursor`] to continue.
#[derive(Debug, Clone)]
pub struct EventPage {
    pub events: Vec<LlmEvent>,
    pub next_cursor: Option<String>,
    /// Total matching events, ignoring the cursor and page limit — `Some` only when the filter asked
    /// for it ([`EventFilter::with_total`]).
    pub total: Option<u64>,
}

/// Optional filters + keyset cursor for [`Store::list_traces_filtered`]. All fields AND-combine;
/// `None` fields don't constrain. `since`/`until` bound the trace's `ended` (its newest event); of
/// these `since` is pushed to the event scan (index-served) while `until`, `status`, and `min_cost`
/// are aggregate-level (applied after grouping). `cursor` is an opaque token minted by a previous
/// page ([`TracePage::next_cursor`]) that the backend decodes into an `(ended, trace_id)` keyset
/// position. `status` is `"success"` or `"error"` (a trace is `error` iff any span errored).
#[derive(Debug, Clone, Default)]
pub struct TraceFilter {
    pub since: Option<DateTime<Utc>>,
    pub until: Option<DateTime<Utc>>,
    pub status: Option<String>,
    pub min_cost: Option<f64>,
    pub cursor: Option<String>,
}

/// One page of trace summaries plus the cursor to fetch the next page (newest-ended first).
/// `next_cursor` is `Some` only when more traces remain beyond this page; pass it back as
/// [`TraceFilter::cursor`] to continue.
#[derive(Debug, Clone)]
pub struct TracePage {
    pub traces: Vec<TraceSummary>,
    pub next_cursor: Option<String>,
}

/// A bounded window of one trace's events, plus how many it really has.
///
/// The detail path fetches at most [`MAX_TRACE_SPANS`] spans; `total` is the trace's true span count,
/// so a clipped read is reported as clipped ([`Trace::spans_truncated`]) instead of passing for a
/// whole trace. `total == events.len()` on the untruncated (normal) case.
#[derive(Debug, Clone)]
pub struct TraceEvents {
    pub events: Vec<LlmEvent>,
    pub total: usize,
}

/// Ceiling on how many spans one trace-detail read materializes. A runaway agent loop can put an
/// unbounded number of spans behind a single `trace_id`, and the detail path — unlike the paginated
/// listing — had no cap, so every fetch and every whole-trace scoring cycle grew with it.
pub const MAX_TRACE_SPANS: usize = 5_000;

/// One cost/usage bucket in a single customer's margin breakdown — grouped by model (`provider/model`)
/// or by use-case `name`. `key` is that bucket label (`unattributed` / `(unnamed)` for the null group).
#[derive(Debug, Clone, Serialize)]
pub struct CustomerCostRow {
    pub key: String,
    pub calls: i64,
    pub cost_usd: f64,
}

/// A use-case cost/usage rollup row — grouped by (name, provider, model), optionally windowed by a
/// `since` cutoff. `name` is `None` for calls that carry no use-case name; the consumer rolls those
/// up under their model. Powers the Personas "LLM Overview" table.
#[derive(Debug, Clone, Serialize)]
pub struct UseCaseCostRow {
    pub name: Option<String>,
    pub provider: String,
    pub model: String,
    pub calls: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cost_usd: f64,
    /// See [`CostRow::unpriced_calls`].
    #[serde(default)]
    pub unpriced_calls: i64,
}

/// One UTC calendar day's aggregated usage for a project — a point in the dense daily series that
/// trend forecasting fits. `day` is the `YYYY-MM-DD` prefix of the (fixed-width, UTC) event `ts`.
/// Days with no traffic are simply absent; the caller densifies the gaps to zero.
#[derive(Debug, Clone, Serialize)]
pub struct DailyUsage {
    pub day: String,
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
}

/// One UTC day's aggregated LLM cost for a single billing-dimension value (customer/product), for
/// margin-trend forecasting. `key` is `None` for untagged (unattributed) cost.
#[derive(Debug, Clone, Serialize)]
pub struct DailyDimCost {
    pub day: String,
    pub key: Option<String>,
    pub cost_usd: f64,
    pub calls: i64,
}

/// Aggregate usage for a project over a time window — used to evaluate limits.
///
/// `cost_usd` is the **stored** sum and stays exactly that: what a `SUM(cost_usd)` sees, with
/// unpriced events contributing nothing. `unpriced_calls` and `client_cost_usd` qualify it, and the
/// limit path reads [`Usage::cost_for_limits`] (stored + imputed) rather than the bare sum.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct Usage {
    pub cost_usd: f64,
    pub calls: i64,
    pub tokens: i64,
    /// Calls in this snapshot whose model was absent from the price book, so `cost_usd` is `NULL` on
    /// the row and they add `$0.00` to `cost_usd`. Counted (never priced) on the event itself; the
    /// limit path charges them via imputation instead.
    #[serde(default)]
    pub unpriced_calls: i64,
    /// The part of `cost_usd` that came from a client-supplied number (`metadata.cost_source =
    /// "client"`) rather than our own price-book arithmetic.
    #[serde(default)]
    pub client_cost_usd: f64,
}

impl Usage {
    /// The value of `metric` in this snapshot, as the comparable `f64` limits evaluate against. For
    /// `cost_usd` this is [`Usage::cost_for_limits`] — the stored sum **plus** the imputed cost of
    /// unpriced traffic — so a model missing from the price book can't be spent for free.
    pub fn metric_value(&self, metric: LimitMetric) -> f64 {
        match metric {
            LimitMetric::CostUsd => self.cost_for_limits(),
            LimitMetric::Calls => self.calls as f64,
            LimitMetric::Tokens => self.tokens as f64,
        }
    }

    /// Provenance of this snapshot's cost: priced vs unpriced calls, the imputed charge for the
    /// unpriced ones, the client-self-reported share, and whether the window is unpriceable.
    ///
    /// **Imputation rule:** each unpriced call is charged the mean cost of a *priced* call in the
    /// same window (`cost_usd / priced_calls`). It uses only evidence already inside the window —
    /// no provider lookups, no writes to the event row — and it self-corrects: as an operator adds
    /// the missing price, newly-priced traffic moves the mean the estimate is drawn from. With no
    /// priced call in the window there is nothing to learn from, and the snapshot is *unpriceable*.
    pub fn cost_evidence(&self) -> CostEvidence {
        let priced = self.calls - self.unpriced_calls;
        let imputed = if priced > 0 && self.unpriced_calls > 0 {
            (self.cost_usd / priced as f64) * self.unpriced_calls as f64
        } else {
            0.0
        };
        CostEvidence {
            priced_calls: priced.max(0),
            unpriced_calls: self.unpriced_calls,
            imputed_cost_usd: imputed,
            client_reported_cost_usd: self.client_cost_usd,
            unpriceable: self.unpriced_calls > 0 && priced <= 0,
        }
    }

    /// The cost figure a `cost_usd` limit is evaluated against: stored cost plus the imputed charge
    /// for unpriced traffic.
    pub fn cost_for_limits(&self) -> f64 {
        self.cost_usd + self.cost_evidence().imputed_cost_usd
    }

    /// Sum two usage snapshots (e.g. rolling usage plus one candidate event's contribution).
    pub fn plus(self, other: Usage) -> Usage {
        Usage {
            cost_usd: self.cost_usd + other.cost_usd,
            calls: self.calls + other.calls,
            tokens: self.tokens + other.tokens,
            unpriced_calls: self.unpriced_calls + other.unpriced_calls,
            client_cost_usd: self.client_cost_usd + other.client_cost_usd,
        }
    }

    /// Subtract one snapshot from another — the inverse of [`Usage::plus`], used to remove an event's
    /// contribution from a running rolling total when it ages out of the window (see the SQLite
    /// backend's `usage_cache`).
    pub fn minus(self, other: Usage) -> Usage {
        Usage {
            cost_usd: self.cost_usd - other.cost_usd,
            calls: self.calls - other.calls,
            tokens: self.tokens - other.tokens,
            unpriced_calls: self.unpriced_calls - other.unpriced_calls,
            client_cost_usd: self.client_cost_usd - other.client_cost_usd,
        }
    }
}

/// Rolling usage for one *value* of a scope dimension — one API key, one customer, one model…
///
/// This is the pre-breach view of a scoped cap: it answers "how much has each key spent so far",
/// which is exactly the question `/v1/limits/status` could not answer until a rule already existed
/// and had already tripped.
#[derive(Debug, Clone, Serialize)]
pub struct ScopeUsage {
    /// The dimension value, or `None` for traffic that carries no value on this dimension (unnamed
    /// calls, untagged customers, events written before key attribution existed).
    pub value: Option<String>,
    #[serde(flatten)]
    pub usage: Usage,
}

/// Evaluate one rule against a usage snapshot — the single place a [`LimitStatus`] is built from a
/// [`Usage`], shared by ingest admission (`evaluate_admission`) and the read-only
/// `/v1/limits/status` surface so the two can never disagree about what a cap currently says.
///
/// `cost_usd` rules carry their [`CostEvidence`]; `calls`/`tokens` rules don't (there is nothing to
/// qualify — a call is a call).
pub fn evaluate_rule(rule: &LimitRule, usage: &Usage) -> LimitStatus {
    let (threshold, basis) = rule.threshold.resolve(None);
    evaluate_rule_resolved(rule, usage, threshold, basis)
}

/// [`evaluate_rule`] against an already-resolved threshold — what a caller that has read revenue for
/// a [`Threshold::RevenueShare`](lighttrack_core::Threshold) rule uses (see [`threshold`]). Keeping
/// this the only other door means the enforced number and the reported number are the same value,
/// not two computations that could drift.
pub fn evaluate_rule_resolved(
    rule: &LimitRule,
    usage: &Usage,
    threshold: f64,
    basis: ThresholdBasis,
) -> LimitStatus {
    let evidence = matches!(rule.metric, LimitMetric::CostUsd).then(|| usage.cost_evidence());
    rule.evaluate_resolved(usage.metric_value(rule.metric), threshold, basis, evidence)
}

/// Outcome of an admission-controlled ingest ([`Store::insert_event_checked`]).
#[derive(Debug, Clone)]
pub struct Admission {
    /// Whether the event was persisted. `false` means a limit turned it away — either a hard stop
    /// (a breached `Throttle`/`Block` rule, or an unpriceable cost cap) or a graduated `Throttle`
    /// shed. The API surfaces both as HTTP 429; [`Admission::shed`] tells them apart.
    pub admitted: bool,
    /// Limit statuses evaluated against rolling usage *including* the candidate event. On a shed,
    /// the rule(s) that shed this event carry `shedding = true`.
    pub statuses: Vec<LimitStatus>,
    /// `true` when the rejection was graduated back-pressure (a `Throttle` rule shedding below its
    /// threshold) rather than a hard stop. Nothing is over budget yet: the same event may be
    /// accepted on a later attempt with a fresh id, and other traffic is still flowing.
    pub shed: bool,
    /// How long the client should wait before retrying, when rejected. Short for a shed, window-
    /// scaled for a hard stop. `None` when admitted.
    pub retry_after_secs: Option<u64>,
}

impl Admission {
    /// Decide the outcome from the evaluated statuses. Rejects on a hard stop
    /// ([`LimitStatus::rejects_ingest`]) or a graduated shed, and derives the retry hint from
    /// whichever rejection is in force — a hard stop outranks a shed, since it is the longer wait.
    pub(crate) fn from_statuses(statuses: Vec<LimitStatus>) -> Self {
        let hard = statuses.iter().any(|s| s.rejects_ingest());
        let shed = !hard && statuses.iter().any(|s| s.shedding);
        let admitted = !hard && !shed;
        let retry_after_secs = statuses
            .iter()
            .filter(|s| s.rejects_ingest())
            .map(|s| s.retry_after_secs())
            .max()
            .or_else(|| {
                statuses
                    .iter()
                    .filter(|s| s.shedding)
                    .map(|s| s.retry_after_secs())
                    .max()
            });
        Admission {
            admitted,
            statuses,
            shed,
            retry_after_secs,
        }
    }
}

/// One event's contribution to rolling usage: one call, its cost, and its prompt+completion tokens
/// (matching `usage_since`, which sums `input + output`).
///
/// An event with no resolved cost still contributes `$0.00` of *stored* cost — the price book had no
/// entry and we refuse to invent one on the row — but it is now counted in `unpriced_calls`, so the
/// limit path can charge it by imputation instead of letting it ride for free.
pub fn event_contribution(ev: &LlmEvent) -> Usage {
    let cost = ev.cost_usd.unwrap_or(0.0);
    Usage {
        cost_usd: cost,
        calls: 1,
        tokens: (ev.usage.input + ev.usage.output) as i64,
        unpriced_calls: i64::from(ev.cost_usd.is_none()),
        client_cost_usd: if ev.cost_is_client_reported() {
            cost
        } else {
            0.0
        },
    }
}

/// Evaluate `rules` against rolling usage that *includes* `contribution` (the candidate event `ev`),
/// looking up each distinct `(window, scope)`'s current usage via `current_usage`. Shared by the
/// trait's default (non-atomic) admission path and backends' transactional overrides so they agree
/// on semantics.
///
/// **Scope semantics:** a scoped rule only *applies* to an event whose dimensions match its scope —
/// non-matching scoped rules are skipped entirely (never evaluated, never able to reject this
/// event), so a "cap gpt-4o" rule can't turn away a gpt-4o-mini call. Every rule that *does* apply
/// (unscoped, or scoped-and-matching) folds the candidate into its own `(window, scope)` usage, then
/// breaches when that usage reaches its threshold; the event is admitted only if no applied enforcing
/// rule breaches.
///
/// **Unpriced traffic:** a `cost_usd` rule evaluates against [`Usage::cost_for_limits`], which charges
/// calls the price book couldn't price at the window's own mean priced cost. When the window has *no*
/// priced call at all the cap is unpriceable and an enforcing rule rejects — see [`CostEvidence`].
pub fn evaluate_admission<F, R>(
    rules: &[LimitRule],
    ev: &LlmEvent,
    contribution: Usage,
    mut current_usage: F,
    resolve_threshold: R,
) -> Result<Admission>
where
    F: FnMut(LimitWindow, Option<&LimitScope>) -> Result<Usage>,
    R: Fn(&LimitRule) -> (f64, ThresholdBasis),
{
    let now = Utc::now();
    let dims = ev.scope_dims();
    // Usage cache now keys by (window, scope): a scoped cap and a project-wide cap over the same
    // window read different rolling totals.
    let mut prospective: HashMap<(LimitWindow, Option<LimitScope>), Usage> = HashMap::new();
    let mut statuses = Vec::new();
    for r in rules {
        if !r.is_active_at(now) {
            continue; // a policy-created rule past its expiry is inert, sweep or no sweep
        }
        if !scope_matches(r.scope.as_ref(), &dims) {
            continue; // a scoped rule the candidate doesn't match can neither count it nor reject it
        }
        let key = (r.window, r.scope.clone());
        let usage = match prospective.get(&key) {
            Some(u) => *u,
            None => {
                // Applied rule → the candidate matches this scope → fold it into the scoped total.
                let u = current_usage(r.window, r.scope.as_ref())?.plus(contribution);
                prospective.insert(key, u);
                u
            }
        };
        let (threshold, basis) = resolve_threshold(r);
        let mut st = evaluate_rule_resolved(r, &usage, threshold, basis);
        // Graduated throttling is decided here, where the candidate event is known: a `Throttle`
        // rule past its ramp start sheds a proportional, deterministic share of traffic. Recorded on
        // the status so the rejection ledger and the alerts attribute the shed to the right rule.
        st.shedding = st.sheds(&ev.id);
        statuses.push(st);
    }
    Ok(Admission::from_statuses(statuses))
}

/// **Non-atomic** admission: `list_limit_rules` → `usage_since` → `insert_event` as three separate
/// store calls with no lock spanning them.
///
/// This is a last resort for a backend that cannot express a check-then-insert as one critical
/// section, and it is *named* rather than hidden inside a trait default so nobody mistakes it for an
/// implementation: between the usage read and the insert, any concurrent ingest can insert too, so a
/// burst can all read the same pre-burst usage and sail past the cap (check-then-act TOCTOU). A
/// backend using it must report [`Store::admission_is_atomic`] `= false` and say so at startup — an
/// advisory cap that reads as enforced is the bug; an honest refusal is not.
pub fn insert_event_checked_nonatomic<S: Store + ?Sized>(
    store: &S,
    ev: &LlmEvent,
) -> Result<Admission> {
    let rules = store.list_limit_rules(&ev.project_id, true)?;
    let now = Utc::now();
    // Revenue-share rules are resolved before the usage walk and only when at least one exists, so a
    // deployment that uses none pays nothing here. A backend that cannot serve revenue at all
    // (`Unsupported`) leaves them unresolved, which is the inert `+inf` case — an unmeasurable
    // guardrail must not become a surprise block.
    let resolved = threshold::resolve_all(&rules, now, |since, until| {
        match store.list_revenue_events(Some(&ev.project_id), since, until) {
            Err(StoreError::Unsupported(_)) => Ok(Vec::new()),
            other => other,
        }
    })?;
    let resolve = threshold::resolver(&resolved);
    let admission = evaluate_admission(
        &rules,
        ev,
        event_contribution(ev),
        |w, scope| match scope {
            None => store.usage_since(&ev.project_id, w.since(now)),
            Some(s) => store.usage_since_scoped(&ev.project_id, w.since(now), s),
        },
        resolve,
    )?;
    if admission.admitted {
        store.insert_event(ev)?;
    }
    Ok(admission)
}

/// Batch form of [`insert_event_checked_nonatomic`]: one independent (and individually non-atomic)
/// admission per item. Cap-honest *within* the batch only because each accepted insert is committed
/// before the next item's usage read.
pub fn insert_events_checked_nonatomic<S: Store + ?Sized>(
    store: &S,
    evs: &[LlmEvent],
) -> Vec<Result<Admission>> {
    evs.iter()
        .map(|e| insert_event_checked_nonatomic(store, e))
        .collect()
}

/// How a byte figure in a [`StorageReport`] was obtained.
///
/// The two answers are *different claims*, and conflating them makes the report unable to answer its
/// own follow-up question ("will anything shrink the file?"): pages allocated to an object include
/// the free space inside those pages; live bytes do not. They diverge by exactly the reclaimable
/// space, which is the number the maintenance pass acts on.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ByteMeasure {
    /// Summed `dbstat.pgsize` — **bytes of pages allocated** to the object, free space within those
    /// pages included. This is what the engine's own page accounting reports.
    PagesAllocated,
    /// The engine could not be asked (the `dbstat` virtual table is not compiled into this SQLite),
    /// so per-object bytes are `None`. Deliberately not a zero: "I could not look" and "there is
    /// nothing there" are different findings, and a zero would read as the second.
    Unavailable,
}

impl ByteMeasure {
    /// The predicate every byte figure in the report travels with.
    pub fn predicate(self) -> &'static str {
        match self {
            ByteMeasure::PagesAllocated => {
                "bytes of pages allocated to the object (dbstat.pgsize), free space inside those \
                 pages included — not bytes of live rows"
            }
            ByteMeasure::Unavailable => {
                "not measured: this SQLite build has no dbstat virtual table, so per-object bytes \
                 are unknown (not zero)"
            }
        }
    }
}

/// One accounted object in the store — a table or one of its indexes.
///
/// Indexes are listed as their own rows rather than folded into their table: an index that has
/// outgrown its table is a different finding, with a different remedy, from a table that has grown.
#[derive(Debug, Clone, Serialize)]
pub struct StorageObject {
    pub name: String,
    /// `table` or `index`.
    pub kind: String,
    /// `None` for indexes (an index has no independent row count) and whenever counting failed.
    pub rows: Option<i64>,
    /// `None` when [`StorageReport::measured`] is [`ByteMeasure::Unavailable`].
    pub bytes: Option<u64>,
    /// This object's share of `db_bytes`, `None` when bytes are unmeasured.
    pub share: Option<f64>,
}

/// Per-object disk accounting for an embedded store, plus what a maintenance pass could reclaim.
///
/// The unit of actionability is the object, not the file: "the database is 2 GB" triggers panic,
/// "one table is 1.7 GB of it" triggers a fix. Every byte figure carries how it was measured
/// ([`ByteMeasure`]), and the file-level figures name their own source in their doc comments.
#[derive(Debug, Clone, Serialize)]
pub struct StorageReport {
    /// Which backend answered — a report that does not say what it measured is a rumour.
    pub backend: &'static str,
    /// The database file, when the backend has one.
    pub path: Option<String>,
    /// `PRAGMA page_size`.
    pub page_size: u64,
    /// `PRAGMA page_count × page_size` — the main database file's size as the engine accounts it.
    pub db_bytes: u64,
    /// The write-ahead journal sidecar, from the filesystem. `None` when there is no file to stat
    /// (in-memory) or the stat failed — never a zero, which would read as "the WAL is empty".
    pub wal_bytes: Option<u64>,
    /// `PRAGMA freelist_count × page_size` — pages the engine already owns and will reuse, which a
    /// reclamation pass can return to the filesystem **without deleting a single row**.
    pub reclaimable_bytes: u64,
    /// `reclaimable_bytes / db_bytes`, the ratio the reclamation trigger reads.
    pub reclaimable_share: f64,
    /// `none` | `full` | `incremental` — decides whether reclamation can happen in yieldable chunks
    /// (`incremental`) or only as a full offline rewrite (`none`).
    pub auto_vacuum: &'static str,
    /// Whether `maintenance_pass` can reclaim on this file at all, and what to do when it cannot.
    pub reclaim_note: String,
    pub measured: ByteMeasure,
    /// The predicate for every `bytes` figure below, carried in the payload so a number quoted out
    /// of this report keeps its meaning.
    pub bytes_predicate: &'static str,
    /// Largest first.
    pub objects: Vec<StorageObject>,
    /// What this store deletes on its own, stated where the disk is measured. Retention is a product
    /// decision, and the current one is written here rather than left to be inferred from an empty
    /// list of pruners.
    pub retention: &'static str,
}

/// What one maintenance pass was asked to do. Both actions are **lossless**: a checkpoint moves
/// already-committed pages from the journal into the database, and incremental vacuum returns pages
/// the engine had already freed. Neither deletes a row, and there is no pruning door here on purpose.
#[derive(Debug, Clone, Copy)]
pub struct MaintenanceRequest {
    /// Truncate the WAL back to zero bytes rather than the routine passive checkpoint. The heavier
    /// form: it needs the writer briefly and is reserved for the escalation rung where the sidecar
    /// itself is the stated harm.
    pub truncate_wal: bool,
    /// Pages of already-freed space to return to the filesystem this pass. `0` skips reclamation.
    /// This is the chunk size — the caller re-reads its activity gauge between chunks.
    pub reclaim_pages: u32,
}

/// How a maintenance pass ended. Three outcomes, never two: a log that cannot distinguish "ran and
/// found nothing to do" from "attempted and failed" cannot tell a healthy store from a broken
/// scheduler, and the difference arrives later as a disk-full report.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MaintenanceOutcome {
    /// Work was done — pages checkpointed and/or reclaimed.
    Ran,
    /// The pass executed and there was nothing to do (empty journal, no free pages).
    NothingToDo,
    /// The engine refused or errored; `detail` carries what it said.
    Failed,
}

/// One record in the store's maintenance flight recorder.
#[derive(Debug, Clone, Serialize)]
pub struct MaintenancePass {
    pub outcome: MaintenanceOutcome,
    pub duration_ms: u64,
    /// Pages moved from the journal into the database file.
    pub pages_checkpointed: u64,
    /// Pages returned to the filesystem by incremental vacuum.
    pub pages_reclaimed: u64,
    /// `PRAGMA freelist_count` before and after, so "we reclaimed nothing" is distinguishable from
    /// "there was nothing to reclaim".
    pub freelist_before: u64,
    pub freelist_after: u64,
    /// What the engine said, or what was skipped and why.
    pub detail: String,
}

/// One measured operation family's numbers.
///
/// Every figure here travels with what it means; see [`DbMetricsReport::recomputation`], which is
/// served in the same payload rather than living in a doc the reader of a number will not have open.
#[derive(Debug, Clone, Serialize)]
pub struct DbOpStats {
    /// The family key — a table or a named operation family, never statement text.
    pub key: &'static str,
    /// Operations recorded since process start.
    pub count: u64,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub max_ms: f64,
    /// How many recent durations the percentiles above were computed from.
    pub sampled: usize,
    /// Operations at or over `slow_over_ms`. The count is meaningless without the threshold, so the
    /// threshold ships beside it in every row.
    pub slow_count: u64,
    pub slow_over_ms: f64,
    /// `None` for read families: a read changes no rows, which is a different statement from
    /// changing zero rows.
    pub rows_written: Option<u64>,
}

/// The store's view of its own behaviour.
#[derive(Debug, Clone, Serialize)]
pub struct DbMetricsReport {
    pub since_secs: u64,
    /// Per-key ring size — the bound on how much history the percentiles can see.
    pub ring_capacity: usize,
    /// Only families that have actually run. A family nobody called is omitted rather than rendered
    /// as a row of zeros, which is a number someone would quote.
    pub ops: Vec<DbOpStats>,
    /// How each figure above was derived, in the payload.
    pub recomputation: &'static str,
    pub note: &'static str,
}

/// Backend-agnostic persistence interface.
pub trait Store: Send + Sync {
    /// What this backend actually implements — see [`crate::capabilities`].
    ///
    /// **Required, deliberately without a default.** A default here would be a claim a new backend
    /// inherits without deciding, which is the exact failure the manifest exists to end: the
    /// surfaces below carry ~45 methods that refuse by default, and until this existed the only
    /// record of which ones a backend had ported was its `impl` block. Adding a backend now forces
    /// an explicit answer, and the conformance suite holds it to it (full semantics for a declared
    /// surface, an asserted `Unsupported` refusal for an undeclared one).
    fn capabilities(&self) -> Capabilities;

    /// Create tables if they don't exist.
    fn init_schema(&self) -> Result<()>;

    /// Persist one normalized event.
    fn insert_event(&self, ev: &LlmEvent) -> Result<()>;

    /// Admission-controlled ingest: evaluate the project's enabled limit rules against rolling
    /// usage *including this event* and persist the event only if no enforcing (`Throttle`/`Block`)
    /// rule would be breached. Returns whether the event was admitted plus the evaluated statuses.
    ///
    /// This is the path ingest must use so a configured cap actually caps. Every backend that can
    /// express the check-and-insert as one critical section **must** override this — SQLite (one
    /// locked connection) and Postgres (one transaction under a per-project lock) do — so a
    /// concurrent burst cannot all read pre-burst usage and sail past the cap (check-then-act
    /// TOCTOU).
    ///
    /// The default is the documented, deliberately-named last resort
    /// [`insert_event_checked_nonatomic`]: it caps on average and leaks under contention. A backend
    /// that inherits it must leave [`Store::admission_is_atomic`] `false` so the conformance suite,
    /// the operator, and the startup log all agree the cap is advisory there.
    fn insert_event_checked(&self, ev: &LlmEvent) -> Result<Admission> {
        insert_event_checked_nonatomic(self, ev)
    }

    /// Whether [`Store::insert_event_checked`] / [`Store::insert_events_checked`] evaluate and
    /// persist in **one atomic step** on this backend, i.e. whether a configured cap is genuinely
    /// enforced under concurrent ingest rather than enforced-on-average.
    ///
    /// Declared in the backend's [`Capabilities`] manifest, never inherited: a newly-added backend
    /// has to answer the question rather than pick up a claim it doesn't honor. The conformance
    /// suite reads this to decide whether to *require* that a concurrent burst stayed under the cap
    /// or merely to report the leak, and the API/startup surfaces it to the operator.
    /// Reads the manifest ([`Capabilities::atomic_admission`]) so the flag and the declaration can
    /// never disagree; a backend states it once, in `capabilities()`.
    fn admission_is_atomic(&self) -> bool {
        self.capabilities().atomic_admission
    }

    /// Admission-controlled **batch** ingest: evaluate + insert each event in `evs`, in order,
    /// returning one result per item (same order). Admission for item _k_ must account for the usage
    /// of every *previously-accepted* item in the same batch, so a caller cannot bypass a cap by
    /// packing many events into one request. Per-item errors (e.g. a duplicate id → `Conflict`) are
    /// returned in that item's slot rather than aborting the whole batch.
    ///
    /// The default ([`insert_events_checked_nonatomic`]) loops the per-item non-atomic path and is
    /// **not** one critical section; it inherits that path's TOCTOU leak against *concurrent*
    /// ingest, and is only cap-honest within its own batch. SQLite (one connection lock + one
    /// transaction) and Postgres (one transaction under a per-project lock) override it so the whole
    /// sequence is atomic and each accepted item is visible to the next item's usage read.
    fn insert_events_checked(&self, evs: &[LlmEvent]) -> Vec<Result<Admission>> {
        insert_events_checked_nonatomic(self, evs)
    }

    /// Most recent events, newest first, optionally filtered by project.
    fn list_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>>;

    /// Filtered, keyset-paginated event listing (newest first). Applies the [`EventFilter`] and pages
    /// on `(ts, id)` descending, returning up to `limit` events plus a `next_cursor` when more remain.
    ///
    /// The default ignores the filter/cursor and delegates to [`Store::list_events`] (no pagination) so
    /// backends that haven't ported the keyset query compile unchanged — the SQLite backend implements
    /// the full filtered/paginated form. Correct string-keyset paging relies on the fixed-width
    /// `RFC3339(Nanos, Z)` timestamp invariant (see [`codec::fmt_ts`]).
    fn list_events_filtered(
        &self,
        project: Option<&str>,
        filter: &EventFilter,
        limit: usize,
    ) -> Result<EventPage> {
        if let Some(what) = filter.unsupported_extension() {
            return Err(StoreError::Unsupported(what));
        }
        Ok(EventPage {
            events: self.list_events(project, limit)?,
            next_cursor: None,
            total: None,
        })
    }

    /// Cost/usage rollup grouped by project + provider + model, optionally filtered by project.
    fn cost_summary(&self, project: Option<&str>) -> Result<Vec<CostRow>>;

    // --- the grouped-rollup primitive ---
    /// **The** grouped rollup: usage and cost over a window, grouped by one to three
    /// [`Dimension`](lighttrack_core::Dimension)s, optionally filtered.
    ///
    /// Every other method in this area — `cost_summary_windowed`, `usecase_costs`, `usage_by_scope`,
    /// the two daily series, the two `*_by_dimension` splits and the two customer breakdowns — is a
    /// **default impl over this one** ([`rollup_compat`]). That is the point: those nine used to be
    /// nine hand-written `GROUP BY`s, four of them on SQLite only, so a production Postgres
    /// deployment answered 501 for `/v1/forecast` and three margin surfaces because nobody had
    /// written the ninth near-identical query. A backend that implements `rollup` serves all of them
    /// with identical semantics by construction.
    ///
    /// A backend may still override any of the nine (SQLite keeps its hand-written versions); the
    /// conformance suite asserts the override and the `rollup`-derived answer agree row for row.
    ///
    /// The default is an honest refusal, not an empty `Vec`: a rollup that reads as "nobody spent
    /// anything" is the failure mode this whole area exists to remove.
    fn rollup(&self, _q: &RollupQuery<'_>) -> Result<Vec<RollupRow>> {
        Err(StoreError::Unsupported("the grouped rollup"))
    }

    /// Cost/usage rollup over an optional `[since, until)` time window (both bounds optional).
    /// Defaults over [`Store::rollup`]; a backend with neither falls back to full history.
    fn cost_summary_windowed(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
        until: Option<DateTime<Utc>>,
    ) -> Result<Vec<CostRow>> {
        match rollup_compat::cost_summary_windowed(self, project, since, until) {
            // No rollup *and* no windowed query: the pre-existing lenient fallback, which returns
            // more than asked rather than nothing. Kept so this can only widen, never blank, a page.
            Err(StoreError::Unsupported(_)) => self.cost_summary(project),
            other => other,
        }
    }

    /// Use-case rollup: cost/usage grouped by (name, provider, model), optionally restricted to
    /// events at/after `since`. Defaults over [`Store::rollup`].
    fn usecase_costs(
        &self,
        project: Option<&str>,
        since: Option<DateTime<Utc>>,
    ) -> Result<Vec<UseCaseCostRow>> {
        rollup_compat::usecase_costs(self, project, since)
    }

    /// Aggregate usage for one project since `since` (inclusive). Used by limit evaluation.
    fn usage_since(&self, project: &str, since: DateTime<Utc>) -> Result<Usage>;

    /// Aggregate usage for one project since `since`, restricted to a single dimension
    /// ([`LimitScope`]: provider / model / use-case). Used to evaluate scoped limit rules.
    ///
    /// The default **conservatively** falls back to project-wide [`Store::usage_since`] — i.e. a
    /// scoped cap on a backend that hasn't ported the scoped query counts *all* project usage against
    /// it, so it may trip early but can never silently under-enforce. Backends add a `WHERE`-clause
    /// query (SQLite does) for exact scoping; Postgres/Firestore are a documented handoff.
    fn usage_since_scoped(
        &self,
        project: &str,
        since: DateTime<Utc>,
        _scope: &LimitScope,
    ) -> Result<Usage> {
        self.usage_since(project, since)
    }

    /// Rolling usage for one project since `since`, **grouped by** every distinct value of one scope
    /// dimension (`kind` is a [`LimitScope::kind_str`]: `provider` | `model` | `name` | `api_key` |
    /// `customer`), newest-window totals per value.
    ///
    /// The pre-breach counterpart to [`Store::usage_since_scoped`]: that one answers "how much has
    /// *this* key spent", this one answers "how much has *each* key spent" — the question an operator
    /// needs answered **before** writing a per-key budget, and the one a breach makes urgent.
    ///
    /// No conservative fallback is possible here (there is no safe way to guess a grouping), so a
    /// backend with no [`Store::rollup`] answers an honest [`StoreError::Unsupported`] → HTTP 501
    /// rather than an empty list that would read as "nobody spent anything".
    fn usage_by_scope(
        &self,
        project: &str,
        since: DateTime<Utc>,
        kind: &str,
    ) -> Result<Vec<ScopeUsage>> {
        rollup_compat::usage_by_scope(self, project, since, kind)
    }

    // --- redaction posture (M9): what the ingest boundary did, grouped ---
    /// Events since `since`, grouped by the [`RedactionStamp`] they carry — the answer to "is this
    /// database raw, scrubbed, or a mix, and by which rule set".
    ///
    /// Unstamped rows (written before the stamp existed, or by a path that does not scrub) group
    /// under `stamp: None`, which is a *different* finding from a stamped row that recorded no
    /// scrub, and the two must never be folded together: one says "we do not know", the other says
    /// "we looked and stored it verbatim".
    ///
    /// [`StoreError::Unsupported`] by default rather than an empty list: an empty posture report
    /// would read as "no events", which is the most reassuring possible lie about this exact
    /// question.
    fn redaction_posture(
        &self,
        _project: Option<&str>,
        _since: DateTime<Utc>,
    ) -> Result<Vec<RedactionPostureRow>> {
        Err(StoreError::Unsupported("the redaction posture report"))
    }

    // --- daily time-series for predictive cost/margin forecasting ---
    // Both default over `rollup` (grouping by `Dimension::Day` on the `received_at` key), so a
    // backend that implements the primitive serves the forecast surface without porting anything.
    /// Daily (UTC) usage totals for one project over `[since, until)`, oldest day first — the series
    /// trend forecasting fits. Days with no traffic are absent (the caller densifies to zero).
    fn daily_usage(
        &self,
        project: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyUsage>> {
        rollup_compat::daily_usage(self, project, since, until)
    }
    /// Daily (UTC) LLM cost per billing-dimension value (`customer` | `product`, from event
    /// metadata) over `[since, until)`, for per-customer/product margin-trend forecasting.
    fn daily_cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<DailyDimCost>> {
        rollup_compat::daily_cost_by_dimension(self, project, dim, since, until)
    }

    // --- projects ---
    fn create_project(&self, p: &Project) -> Result<()>;
    /// Replace a project's mutable fields (name / enabled / redaction / collective opt-in), matched by
    /// `p.id`; `id` and `created_at` are immutable. Returns `true` when a row changed, `false` when the
    /// id is unknown (the API maps that to 404).
    ///
    /// Default is a clear unimplemented error rather than a silent no-op: `redaction` is a compliance
    /// control, and an operator who tightened it must never be told "done" by a backend that dropped
    /// the write (the same stance as `update_limit_rule`).
    fn update_project(&self, _p: &Project) -> Result<bool> {
        Err(StoreError::Unsupported("updating projects"))
    }
    fn get_project(&self, id: &str) -> Result<Option<Project>>;
    fn list_projects(&self) -> Result<Vec<Project>>;

    // --- API keys ---
    fn create_api_key(&self, k: &ApiKey) -> Result<()>;
    /// Look up a key by its (non-secret) prefix, for auth. Returns even revoked keys; caller checks.
    fn find_api_key_by_prefix(&self, prefix: &str) -> Result<Option<ApiKey>>;
    /// Best-effort update of `last_used_at`.
    fn touch_api_key(&self, id: &str, when: DateTime<Utc>) -> Result<()>;
    /// Every key minted for a project (revoked ones included — the caller decides what to show), so an
    /// operator can list, audit last-use, and pick one to revoke. Default `Ok(vec![])` so unported
    /// backends compile (matching the `get_limit_rule` precedent). NEVER expose `key_hash` upward.
    fn list_api_keys(&self, _project: &str) -> Result<Vec<ApiKey>> {
        Err(StoreError::Unsupported("listing API keys"))
    }
    /// Set a key's `revoked` flag (soft — the row is kept for audit; auth already rejects a revoked
    /// key at `guards.rs`). Returns `true` when a row changed, `false` when the id is unknown (→ 404).
    /// Default is a clear unimplemented error rather than a silent no-op, so an operator on an unported
    /// backend learns the key was NOT revoked instead of believing a leaked key is dead.
    fn set_api_key_revoked(&self, _id: &str, _revoked: bool) -> Result<bool> {
        Err(StoreError::Unsupported("revoking API keys"))
    }
    /// Stamp (or clear, with `None`) a key's `expires_at`. Returns `true` when a row changed,
    /// `false` when the id is unknown (→ 404).
    ///
    /// This is what makes key *rotation* durable: the successor is minted and the predecessor is
    /// given a deadline, so the grace window closes itself. A background revoke task would be lost
    /// on the next restart — exactly when nobody is watching — and would then leave the old key
    /// live forever. Same stance as `set_api_key_revoked`: an unported backend must say so, never
    /// report a rotation it did not persist.
    fn set_api_key_expiry(&self, _id: &str, _when: Option<DateTime<Utc>>) -> Result<bool> {
        Err(StoreError::Unsupported("API-key expiry"))
    }

    // --- limit rules ---
    fn create_limit_rule(&self, r: &LimitRule) -> Result<()>;
    fn list_limit_rules(&self, project: &str, only_enabled: bool) -> Result<Vec<LimitRule>>;
    /// Fetch one rule by id (across projects — the caller is admin-gated). Default `None` so
    /// backends that haven't ported the lifecycle read compile unchanged.
    fn get_limit_rule(&self, _id: &str) -> Result<Option<LimitRule>> {
        Err(StoreError::Unsupported("limit-rule lookup"))
    }
    /// Replace a rule's mutable fields (metric/window/threshold/action/enabled — and, once ported,
    /// `warn_at`/`scope`), matched by `r.id`; `project_id` is immutable. Returns `true` when a row
    /// was updated, `false` when the id is unknown (the API maps that to 404). The default is a clear
    /// unimplemented error rather than a silent no-op, so an operator on an unported backend learns
    /// the rule was *not* changed instead of believing a cap was tightened.
    fn update_limit_rule(&self, _r: &LimitRule) -> Result<bool> {
        Err(StoreError::Unsupported("updating limit rules"))
    }
    /// Delete a rule by id. Returns `true` when a row was removed, `false` when the id is unknown
    /// (the API maps that to 404). Default is a clear unimplemented error (see `update_limit_rule`).
    fn delete_limit_rule(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("deleting limit rules"))
    }

    // --- margin policies (the standing guardrails that create limit rules) ---
    //
    // A policy is configuration for the forecast sweep, never read on the ingest hot path, so these
    // are ordinary CRUD. They default to `Unsupported` rather than to an empty list: a backend that
    // has not ported the table must say so, or an operator who configured a guardrail on Postgres
    // would watch `list_margin_policies` return `[]` and conclude the feature simply did nothing.
    /// Persist one margin policy.
    fn create_margin_policy(&self, _p: &MarginPolicy) -> Result<()> {
        Err(StoreError::Unsupported("margin policies"))
    }
    /// A project's policies, optionally only the enabled ones (what the sweep reads).
    fn list_margin_policies(
        &self,
        _project: &str,
        _only_enabled: bool,
    ) -> Result<Vec<MarginPolicy>> {
        Err(StoreError::Unsupported("margin policies"))
    }
    /// One policy by id, or `None` when it does not exist.
    fn get_margin_policy(&self, _id: &str) -> Result<Option<MarginPolicy>> {
        Err(StoreError::Unsupported("margin policies"))
    }
    /// Remove a policy; `false` when no row matched (the API maps that to 404). Rules the policy
    /// created are NOT deleted here — the sweep's reverse pass reaps them, so removal goes through
    /// exactly one code path.
    fn delete_margin_policy(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("margin policies"))
    }

    // --- single event lookup + scores (Phase 3) ---
    fn get_event(&self, id: &str) -> Result<Option<LlmEvent>>;
    /// Persist a judge verdict, including its structured provenance (`Score::detail`: per-dimension
    /// breakdown, agreement, sample accounting, bias/injection flags) and its benchmark-run scoping
    /// (`Score::run_id` / `Score::case_index`). Every shipped backend persists all three — a verdict
    /// that reads back without its provenance, or a case that can't say which run produced it, is a
    /// silently degraded record rather than an obviously missing one.
    fn insert_score(&self, s: &Score) -> Result<()>;
    fn list_scores(&self, project: Option<&str>, limit: usize) -> Result<Vec<Score>>;
    /// Scores narrowed by the typed identity: which rubric, and what sort of verdict.
    ///
    /// A separate method rather than widening [`Store::list_scores`], for the same reason
    /// `list_events_filtered` is separate: the unfiltered listing is on the hot path of every
    /// dashboard, and it must not grow a filter argument every consumer has to pass `None` for.
    ///
    /// [`StoreError::Unsupported`] by default — never a silently unfiltered listing. Answering a
    /// `kind=bench_case` query with every score in the project would look authoritative and be
    /// wrong, which is the failure this whole trait's default policy exists to refuse.
    fn list_scores_filtered(
        &self,
        _project: Option<&str>,
        _filter: &ScoreFilter,
        _limit: usize,
    ) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("the typed score filters"))
    }
    /// Every case result recorded for one benchmark run, in case order (`case_index`, then
    /// `created_at`; cases without an index sort last). This is the answer to "why did run 47 fail?"
    /// — the per-case verdicts, with the provenance that produced each one.
    ///
    /// `project` is the caller's authorization scope: `Some(p)` restricts to that project (a
    /// project-scoped API key), `None` reads across projects (admin). Backends apply it in the
    /// query, so it can't be forgotten by a caller.
    ///
    /// Default is a clear [`StoreError::Unsupported`] (→ 501) rather than an empty list: "this
    /// backend never stored run scoping" and "this run had no cases" are different facts, and only
    /// one of them means the run passed.
    fn list_run_scores(
        &self,
        _run_id: &str,
        _project: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("run-scoped case results"))
    }
    /// Of the given event ids, which already carry at least one score. Scoped to **exactly these ids**
    /// — never a blind top-N of the `scores` table — so the online scorer's "skip already-scored"
    /// stays correct however large the scores table grows. Required (no default): a wrong answer here
    /// re-judges events (burning paid judge calls) or skips new ones, so every backend implements it
    /// and the conformance suite pins it. Backed by `idx_scores_event`.
    fn scored_event_ids(&self, event_ids: &[String]) -> Result<Vec<String>>;
    /// Recent events (newest first, optionally project-scoped) that do **not** yet have a score — the
    /// online scorer's work list. The default fetches a page via [`Store::list_events`] and removes the
    /// scored ones via [`Store::scored_event_ids`], which is correct and bounded on every backend (it
    /// reads scores only for the page's ids, unlike the old client-side top-1000 anti-join that
    /// silently re-judged once a project passed 1000 scores). SQL backends may override with a single
    /// `LEFT JOIN scores ... WHERE s.id IS NULL` for one round-trip.
    fn list_unscored_events(&self, project: Option<&str>, limit: usize) -> Result<Vec<LlmEvent>> {
        let events = self.list_events(project, limit)?;
        let ids: Vec<String> = events.iter().map(|e| e.id.clone()).collect();
        let scored: std::collections::HashSet<String> =
            self.scored_event_ids(&ids)?.into_iter().collect();
        Ok(events
            .into_iter()
            .filter(|e| !scored.contains(&e.id))
            .collect())
    }

    // --- traces: roll events sharing a trace_id into one end-to-end view ---
    // Default impls so backends that don't (yet) index by trace compile unchanged: the listing reads
    // empty and `get_trace` composes `list_trace_events` (so any backend that can list a trace's
    // events gets a correct rollup for free, from the pure `Trace::from_events`).
    /// Whether this backend actually serves the trace surface (listing, detail, trace scores).
    ///
    /// A declared capability rather than a probe, on the same terms as
    /// [`Store::admission_is_atomic`] — it is [`Surface::Traces`] in the backend's manifest. The
    /// conformance suite runs the full trace semantics against a backend that declares it
    /// and, against one that declares `false`, asserts every trace method *refuses* with
    /// [`StoreError::Unsupported`] — so "not implemented" can never quietly become an empty page.
    /// The API surfaces the refusal as HTTP 501 `unsupported`.
    /// Reads the manifest ([`Surface::Traces`]) so the flag and the declaration can never disagree.
    fn serves_traces(&self) -> bool {
        self.capabilities().has(Surface::Traces)
    }
    /// Compact summaries of the most recent traces (grouped by `trace_id`), newest activity first.
    fn list_traces(&self, _project: Option<&str>, _limit: usize) -> Result<Vec<TraceSummary>> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Filtered, keyset-paginated trace listing (newest `ended` first). Applies the [`TraceFilter`]
    /// and pages on `(ended, trace_id)` descending, returning up to `limit` summaries plus a
    /// `next_cursor` when more remain.
    ///
    /// The default ignores the filter/cursor and delegates to [`Store::list_traces`] (no pagination),
    /// which on a backend that doesn't serve traces is itself an [`StoreError::Unsupported`] refusal —
    /// never a silently unfiltered page. SQLite and Postgres implement the full windowed/paginated
    /// form. Correct string-keyset paging relies on the fixed-width `RFC3339(Nanos, Z)` invariant.
    fn list_traces_filtered(
        &self,
        project: Option<&str>,
        _filter: &TraceFilter,
        limit: usize,
    ) -> Result<TracePage> {
        Ok(TracePage {
            traces: self.list_traces(project, limit)?,
            next_cursor: None,
        })
    }
    /// All events of one trace **within `project`**, oldest first.
    ///
    /// A `trace_id` is caller-supplied and therefore NOT a tenant boundary: two projects can pick the
    /// same natural id (`"req-1"`, a shared upstream request id), and anyone who knows an id can post
    /// an event under it. So the project filter belongs in the query, not in a post-hoc authorization
    /// check over a cross-project merge — a colliding id in another project must be invisible here,
    /// never folded into the caller's trace. `None` means "across every project" and is reserved for
    /// operator-level principals (admin/dev); a project-scoped caller always passes `Some`.
    ///
    /// At most `max_spans` events come back (the oldest, so the trace keeps its head), with
    /// [`TraceEvents::total`] reporting the trace's real span count — the detail path is otherwise
    /// unbounded, which is how one runaway loop slows every read of that trace.
    fn list_trace_events(
        &self,
        _project: Option<&str>,
        _trace_id: &str,
        _max_spans: usize,
    ) -> Result<TraceEvents> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Scores attached to any event within a trace (i.e. `scores.event_id` ∈ the trace's events),
    /// scoped by `project` on the same terms as [`Store::list_trace_events`].
    fn list_trace_scores(&self, _project: Option<&str>, _trace_id: &str) -> Result<Vec<Score>> {
        Err(StoreError::Unsupported("traces"))
    }
    /// Full rollup (totals + span tree) for one trace within `project`, or `None` if it has no events
    /// there. See [`Store::list_trace_events`] for why the project scope is part of the query and why
    /// the fan-out is capped; a clipped trace carries `spans_truncated`.
    fn get_trace(
        &self,
        project: Option<&str>,
        trace_id: &str,
        max_spans: usize,
    ) -> Result<Option<Trace>> {
        let page = self.list_trace_events(project, trace_id, max_spans)?;
        Ok(Trace::from_events_bounded(page.events, page.total))
    }

    // --- benchmarks (Phase 3.5) ---
    fn create_benchmark(&self, b: &Benchmark) -> Result<()>;
    fn get_benchmark(&self, id: &str) -> Result<Option<Benchmark>>;
    fn list_benchmarks(&self, project: &str) -> Result<Vec<Benchmark>>;
    fn create_benchmark_run(&self, r: &BenchmarkRun) -> Result<()>;
    fn list_benchmark_runs(&self, benchmark_id: &str) -> Result<Vec<BenchmarkRun>>;

    // --- model prices (Phase 3.6a) ---
    fn upsert_price(&self, p: &ModelPriceRow) -> Result<()>;
    fn list_prices(&self) -> Result<Vec<ModelPriceRow>>;

    // --- datasets (Phase 3.6b) ---
    fn create_dataset(&self, d: &Dataset) -> Result<()>;
    fn get_dataset(&self, id: &str) -> Result<Option<Dataset>>;
    fn list_datasets(&self, project: &str) -> Result<Vec<Dataset>>;
    fn set_dataset_frozen(&self, id: &str, frozen: bool) -> Result<()>;
    fn create_dataset_item(&self, item: &DatasetItem) -> Result<()>;
    fn list_dataset_items(&self, dataset_id: &str) -> Result<Vec<DatasetItem>>;

    // --- rubrics (Phase 3.6c) ---
    fn create_rubric(&self, r: &Rubric) -> Result<()>;
    fn get_rubric(&self, id: &str) -> Result<Option<Rubric>>;
    fn list_rubrics(&self, project: &str) -> Result<Vec<Rubric>>;

    // --- job queue (Phase 3.6d) ---
    fn create_job(&self, j: &Job) -> Result<()>;
    /// Atomically claim the oldest queued (or stale-running) job: sets it `running`, bumps attempts.
    /// Reclaiming a stale `running` job counts a **worker death** (`stale_reclaims` + the
    /// `JOB_ERROR_WORKER_LOST` marker), never a benchmark failure. `cancelling`/`cancelled` jobs are
    /// outside the claimable set, so a cancelled run can never be restarted by the reclaim path.
    ///
    /// `kinds` is the claiming worker's capability declaration — the job kinds it can actually
    /// execute. Empty means "any kind", which is what every worker meant while `bench_run` was the
    /// only one and what an older runner still sends. The filter belongs INSIDE the claim: a worker
    /// that claims a kind it cannot run has already taken the job off the queue and stamped a lease
    /// on it, so the job burns its retry budget failing while a capable worker idles beside it.
    fn claim_job(&self, stale_before: DateTime<Utc>, kinds: &[&str]) -> Result<Option<Job>>;
    /// Ask a queued/running job to stop. `queued` → `cancelled` outright; `running` → `cancelling`,
    /// which the worker notices at its next case boundary. `Ok(None)` = no such job.
    ///
    /// Backends that cannot do this atomically must return [`StoreError::Unsupported`] (→ 501)
    /// rather than a quiet default: a cancel that silently did nothing is worse than a 501, because
    /// the operator walks away believing the spend stopped.
    fn cancel_job(&self, _id: &str) -> Result<Option<JobCancel>> {
        Err(StoreError::Unsupported("cancelling a job"))
    }
    fn update_job_progress(&self, id: &str, progress: &str) -> Result<()>;
    /// Extend the holder's lease: move `claimed_at` forward, **conditioned on it still being
    /// `fence`** and the job still live. Returns the new `claimed_at` on success, `None` when the
    /// lease is no longer this caller's (expired and reclaimed, requeued, cancelled, finished).
    ///
    /// A renewal that nobody reads gates nothing. `None` is the executor's own gate on its
    /// legitimacy and must reach its work loop and stop it: an executor that keeps working after
    /// losing its lease interleaves its effects with its successor's.
    ///
    /// Renewal is on a **timer**, not per unit of work — a loop that renews "after each item"
    /// silently stops renewing inside the one step that takes an hour, which is exactly the step
    /// during which the lease matters. And it never waits on progress: the moment liveness is
    /// conditioned on having something to report, a live-but-stuck worker reads as a dead one, and
    /// those are the two states the whole mechanism exists to tell apart.
    fn renew_job_lease(&self, _id: &str, _fence: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        Err(StoreError::Unsupported("renewing a job lease"))
    }
    /// Write a job's verdict — **conditioned on the job still being non-terminal, and (when `fence`
    /// is given) still held by this caller**.
    ///
    /// `fence` is the `claimed_at` the caller believes it holds. Pass it from every worker; `None`
    /// is for an operator/administrative finish that is not claiming to be the holder, and even
    /// then a terminal verdict is never overwritten.
    ///
    /// Returns [`JobFinish::NotHeld`] rather than an error when the write is refused, carrying the
    /// status and lease the record actually has, so a slow worker that lost the finish-line race
    /// loses it politely and can say what beat it.
    fn finish_job(
        &self,
        id: &str,
        status: &str,
        result: &Value,
        error: Option<&str>,
        fence: Option<DateTime<Utc>>,
    ) -> Result<JobFinish>;
    fn get_job(&self, id: &str) -> Result<Option<Job>>;
    fn list_jobs(&self, status: Option<&str>, limit: usize) -> Result<Vec<Job>>;

    // --- stored schedules (M7): recurrence as a row, swept by the API ---
    // Default impls so a backend that has not ported the table compiles unchanged — but they refuse
    // rather than answer empty: a `due_schedules` that quietly returned `[]` would read as "nothing
    // recurring is configured here", which is the exact lie an operator would act on.
    fn create_schedule(&self, _s: &Schedule) -> Result<()> {
        Err(StoreError::Unsupported("stored schedules"))
    }
    fn get_schedule(&self, _id: &str) -> Result<Option<Schedule>> {
        Err(StoreError::Unsupported("stored schedules"))
    }
    fn list_schedules(&self, _project: &str) -> Result<Vec<Schedule>> {
        Err(StoreError::Unsupported("stored schedules"))
    }
    /// Replace a schedule's mutable fields; `Ok(false)` = no such id. The id and `project_id` are
    /// identity and are never written — a schedule that could change project would be a way around
    /// project scoping.
    fn update_schedule(&self, _s: &Schedule) -> Result<bool> {
        Err(StoreError::Unsupported("stored schedules"))
    }
    fn delete_schedule(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("stored schedules"))
    }
    /// Enabled schedules whose `next_due` has passed — the sweep's one read per tick.
    fn due_schedules(&self, _now: DateTime<Utc>) -> Result<Vec<Schedule>> {
        Err(StoreError::Unsupported("stored schedules"))
    }

    // --- prompt registry (versioned prompts + label-gated promotion) ---
    // Default impls so backends that don't (yet) host the registry compile unchanged: writes are a
    // clear error rather than a silent drop, and reads are empty/None.
    /// Register a new named prompt (with its initial labels/benchmark link).
    fn create_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Update a prompt's mutable fields (label pointers, linked benchmark, `updated_at`).
    fn update_prompt(&self, _p: &Prompt) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Look up a prompt by its registry name within a project (the runtime fetch path).
    fn get_prompt(&self, _project: &str, _name: &str) -> Result<Option<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn get_prompt_by_id(&self, _id: &str) -> Result<Option<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn list_prompts(&self, _project: &str) -> Result<Vec<Prompt>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// Append an immutable version to a prompt.
    fn create_prompt_version(&self, _v: &PromptVersion) -> Result<()> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    fn get_prompt_version(&self, _prompt_id: &str, _version: u32) -> Result<Option<PromptVersion>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }
    /// All versions of a prompt, newest version first.
    fn list_prompt_versions(&self, _prompt_id: &str) -> Result<Vec<PromptVersion>> {
        Err(StoreError::Unsupported("the prompt registry"))
    }

    // --- revenue + margin (Phase 1 profit tracking) ---
    // Default impls so backends that don't (yet) support profit tracking compile unchanged: cost is a
    // no-op (empty), and inserting revenue is a clear error rather than a silent drop.
    /// Persist one normalized revenue record.
    fn insert_revenue_event(&self, _ev: &RevenueEvent) -> Result<()> {
        Err(StoreError::Unsupported("revenue tracking"))
    }
    /// Persist a batch of revenue records **atomically** — all-or-nothing. A webhook delivery carries
    /// many events; if one fails a constraint mid-batch, none may be committed, or the provider's
    /// retry would re-fail on the same record and the events after it would be lost permanently (the
    /// handler returns an error, so 1..N-1 are already committed while N..end never land). The default
    /// loops over [`Store::insert_revenue_event`] and is **not** atomic; backends whose writes share a
    /// single critical section (e.g. SQLite) override it to wrap the batch in one transaction.
    fn insert_revenue_events(&self, evs: &[RevenueEvent]) -> Result<()> {
        for ev in evs {
            self.insert_revenue_event(ev)?;
        }
        Ok(())
    }
    /// Revenue records that may be recognized within `[since, until)`, optionally scoped to a project.
    fn list_revenue_events(
        &self,
        _project: Option<&str>,
        _since: DateTime<Utc>,
        _until: DateTime<Utc>,
    ) -> Result<Vec<RevenueEvent>> {
        Err(StoreError::Unsupported("revenue tracking"))
    }
    /// Re-convert stored revenue rows of one currency at a corrected `rate`, stamping `version`.
    ///
    /// Only rows that took the **1:1 fallback** are touched. A row that converted genuinely is
    /// recognized revenue at a rate that was correct when it was taken; re-basing it would restate
    /// history, which is what the redelivery guard on the upsert exists to prevent — this door must
    /// not be a way around it. `dry_run` counts without writing.
    ///
    /// This is the remedy `docs/CURRENCY.md` used to spell "re-ingest from the provider", which is
    /// not a remedy for a webhook nobody can replay.
    fn reprice_revenue(
        &self,
        _project: Option<&str>,
        _currency: &str,
        _rate: f64,
        _version: &str,
        _dry_run: bool,
    ) -> Result<RepriceReport> {
        Err(StoreError::Unsupported("revenue repricing"))
    }
    /// LLM cost grouped by a billing dimension (`customer` | `product`, from event metadata) over
    /// `[since, until)`. Defaults over [`Store::rollup`].
    fn cost_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CostByDimension>> {
        rollup_compat::cost_by_dimension(self, project, dim, since, until)
    }
    /// Prompt+completion tokens grouped by a billing dimension (`customer` | `product`, from event
    /// metadata) over `[since, until)` — the usage side of the pricing what-if simulator. Defaults
    /// over [`Store::rollup`].
    fn tokens_by_dimension(
        &self,
        project: Option<&str>,
        dim: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<TokensByDimension>> {
        rollup_compat::tokens_by_dimension(self, project, dim, since, until)
    }
    /// One customer's LLM cost broken down **by model** (`provider/model`) over `[since, until)`,
    /// scoped by `metadata.customer_id = customer`. Defaults over [`Store::rollup`], where the
    /// customer is a *filter* rather than a grouping — a row for anyone else here is a tenant leak.
    fn customer_cost_by_model(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        rollup_compat::customer_cost_by_model(self, project, customer, since, until)
    }
    /// One customer's LLM cost broken down **by use-case `name`** over `[since, until)`, scoped by the
    /// same `metadata.customer_id` (see [`Store::customer_cost_by_model`]).
    fn customer_cost_by_name(
        &self,
        project: Option<&str>,
        customer: &str,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<Vec<CustomerCostRow>> {
        rollup_compat::customer_cost_by_name(self, project, customer, since, until)
    }

    // --- cloud→device relay queue (docs/RELAY.md) ---
    // Default impls so backends that don't (yet) host the relay compile unchanged: writes are a
    // clear error rather than a silent drop, and reads/leases are empty/None.
    /// Enqueue one device task.
    fn create_relay_task(&self, _t: &RelayTask) -> Result<()> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    fn get_relay_task(&self, _id: &str) -> Result<Option<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Dedupe lookup for idempotent enqueue: the task holding `key` within `project`, if any.
    fn find_relay_task_by_key(&self, _project: &str, _key: &str) -> Result<Option<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    fn list_relay_tasks(
        &self,
        _project: Option<&str>,
        _status: Option<&str>,
        _limit: usize,
    ) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Atomically lease up to `max` due tasks for `device`: queued tasks past `next_attempt_at`
    /// plus expired leases with attempts to spare (each lease consumes an attempt).
    ///
    /// **Filtered by what the device can actually run** (M18). `capabilities` is the leasing
    /// device's advertised action types — exact names, or `"<ns>/*"` namespace prefixes — and only
    /// tasks it covers are handed over. Before this the lease gave any due task to whoever asked,
    /// so a device whose action library lacked the action burned a real attempt reporting "no
    /// action" and then waited out a five-hour retry interval to do it again.
    ///
    /// An **empty** slice means no filter, which is what a pre-M18 agent and the legacy shared
    /// device key send: a device that suddenly leased nothing after an upgrade would be a worse
    /// failure than an unfiltered one.
    fn lease_relay_tasks(
        &self,
        _device: &str,
        _capabilities: &[String],
        _lease_secs: i64,
        _max: usize,
    ) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Dead-letter expired leases with exhausted attempts, returning the newly-dead tasks (for
    /// alerting). The API runs this before each lease.
    fn sweep_relay_dead(&self) -> Result<Vec<RelayTask>> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Settle a leased task with the device's outcome — **conditioned on the caller still holding
    /// the lease**, exactly like `finish_job`.
    ///
    /// `fence` is the `lease_fence` the caller was handed at lease time; `None` is the
    /// operator-shaped settle, which waives the ownership condition but never the liveness one.
    /// Returns [`RelaySettle::NotHeld`] rather than an error when the write is refused, carrying
    /// what the record holds now — the check it replaces (`status == "leased"`) asked about
    /// liveness where ownership was meant, so a device reclaimed mid-run reported back onto its
    /// successor's task and overwrote the run in progress.
    fn settle_relay_task(
        &self,
        _id: &str,
        _fence: Option<DateTime<Utc>>,
        _outcome: &RelayOutcome,
    ) -> Result<RelaySettle> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Extend the holder's lease by `lease_secs`, conditioned on `fence`. Moves the DEADLINE, never
    /// the fence: one device's lease keeps one identity for its whole run, so the report it sends
    /// hours later carries the token it was given.
    ///
    /// This is what turns `lease_secs` from "the longest a run may take" (it was clamped to 6 h, and
    /// was simultaneously the detection latency for a dead device) into detection latency alone.
    fn renew_relay_lease(
        &self,
        _id: &str,
        _fence: DateTime<Utc>,
        _lease_secs: i64,
    ) -> Result<LeaseHeld> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Publish the holder's liveness detail, on its own door — never on the renewal, or a device
    /// that is alive but stuck computing something to say reads as a dead one.
    fn update_relay_progress(
        &self,
        _id: &str,
        _fence: DateTime<Utc>,
        _progress: &str,
    ) -> Result<LeaseHeld> {
        Err(StoreError::Unsupported("the relay queue"))
    }
    /// Ask a task to stop: `queued` → `cancelled`, `leased` → `cancelling` (outside the leasable
    /// set, so it is never handed to a second device), terminal → untouched. `Ok(None)` = no such
    /// task. A backend that cannot do this atomically must refuse rather than default quietly: a
    /// cancel that silently did nothing leaves the operator believing the run stopped.
    fn cancel_relay_task(&self, _id: &str) -> Result<Option<RelayCancel>> {
        Err(StoreError::Unsupported("the relay queue"))
    }

    // --- collective model intelligence (network effect) ---
    // Default impls so backends that don't (yet) host a leaderboard compile unchanged: ingest is a
    // clear error rather than a silent drop, and the leaderboard reads as empty.
    /// Upsert one privacy-safe digest entry received from a contributor (keyed on
    /// contributor_id + provider + model + task_type).
    fn upsert_collective_entry(&self, _e: &CollectiveEntry) -> Result<()> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// Drop all of a contributor's entries (so a re-contribution replaces, never accretes, its set).
    fn delete_collective_entries(&self, _contributor_id: &str) -> Result<u64> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// All stored digest entries, for merging into the public leaderboard.
    fn list_collective_entries(&self) -> Result<Vec<CollectiveEntry>> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }
    /// Physically delete entries received before `cutoff` (retention sweep); returns how many went.
    /// A backend that leaves this unimplemented still honors the retention policy — the API filters
    /// expired entries out of the leaderboard before merging, on every backend — it just keeps the
    /// dead rows on disk. The API therefore treats `Unsupported` here as non-fatal.
    fn purge_collective_entries_before(&self, _cutoff: DateTime<Utc>) -> Result<u64> {
        Err(StoreError::Unsupported("the collective leaderboard"))
    }

    /// Replace **all** of `contributor_id`'s entries with `entries` — and, when `purge_before` is
    /// given, run the retention sweep on the same pass. One call, so a backend with transactions can
    /// make the replacement atomic: an interrupted delete-then-upsert loop leaves a contributor
    /// half-replaced, which publishes a *wrong* merged row rather than a missing one.
    ///
    /// The default composes the fine-grained methods and honestly reports `atomic: false`; backends
    /// that can do better override it. A backend that does not serve the surface at all refuses
    /// here too, because the first composed call already refuses.
    fn replace_collective_contribution(
        &self,
        contributor_id: &str,
        entries: &[CollectiveEntry],
        purge_before: Option<DateTime<Utc>>,
    ) -> Result<ReplaceAck> {
        replace_collective_contribution_nonatomic(self, contributor_id, entries, purge_before)
    }
    /// The most recent `received_at` this contributor has stored, or `None` if it has none.
    ///
    /// Exists so the per-contributor minimum-interval check is a keyed read rather than a decode of
    /// the entire table on every ingest. The default scans; backends index it.
    fn latest_collective_receipt(&self, contributor_id: &str) -> Result<Option<DateTime<Utc>>> {
        collective::latest_receipt_scanned(self, contributor_id)
    }
    /// Stored entries narrowed by [`CollectiveFilter`] — today, the retention cutoff.
    ///
    /// Only *pre-floor-safe* predicates live here: a user-supplied provider/task filter pushed into
    /// the store could strip a merged row down to one contributor. See [`CollectiveFilter`].
    fn list_collective_entries_filtered(
        &self,
        f: &CollectiveFilter,
    ) -> Result<Vec<CollectiveEntry>> {
        collective::list_filtered_scanned(self, f)
    }

    // --- storage accounting + lossless maintenance ---
    //
    // Both are `Unsupported` by default rather than returning an empty report: a managed backend
    // (Postgres, Firestore) has a disk somebody else monitors, and answering "0 bytes, no tables"
    // for it would be a confident lie in exactly the surface an operator consults about disk.

    /// Per-object disk accounting for this store. Cheap enough to serve on demand; it walks the
    /// engine's page accounting, so it is a read, not an estimate — see [`ByteMeasure`].
    fn storage_report(&self) -> Result<StorageReport> {
        Err(StoreError::Unsupported("storage accounting"))
    }

    /// Run one **lossless** maintenance chunk: checkpoint the journal, return already-freed pages.
    /// Never deletes a row (there is no pruning parameter on purpose — see `MaintenanceRequest`).
    /// The caller owns the activity gate and the chunk loop; this is one chunk.
    fn maintenance_pass(&self, _req: MaintenanceRequest) -> Result<MaintenancePass> {
        Err(StoreError::Unsupported("store maintenance"))
    }

    /// What this store has observed about its own operation latency, keyed by operation family.
    ///
    /// `Unsupported` by default for the same reason as the two above: a backend that measures
    /// nothing must say so, because an empty report reads as "everything is fast".
    fn db_metrics(&self) -> Result<DbMetricsReport> {
        Err(StoreError::Unsupported("store self-instrumentation"))
    }

    // --- the unpriced ledger + the dated price book (M26) ---

    /// Which `(provider, model)` pairs carried traffic this store could not price, since `since`.
    ///
    /// The null-cost invariant means an unpriceable call stores `cost_usd = NULL` rather than a
    /// zero — honest, but until now invisible: no surface said *which* models were missing, so the
    /// only symptom was a cost dashboard that felt low. Ranked and totalled by
    /// [`UnpricedLedger`](lighttrack_core::UnpricedLedger) above this.
    ///
    /// The default folds [`Store::rollup`]; a backend without the rollup refuses through it, which
    /// is the honest answer — an empty ledger reads as "everything is priced".
    fn list_unpriced(
        &self,
        project: Option<&str>,
        since: DateTime<Utc>,
    ) -> Result<Vec<UnpricedRow>> {
        rollup_compat::refusal(
            pricing::list_unpriced_via_rollup(self, project, since),
            "the unpriced-traffic ledger",
        )
    }

    /// Price the stored rows for one `(provider, model)` that have no cost on them, from `f`'s
    /// book. Returns how many rows were written.
    ///
    /// Only `cost_usd IS NULL` rows are eligible, which is what makes this compatible with the
    /// no-retroactive-repricing rule: a row already costed — from the caller's own number
    /// (`cost_source = "client"`) or from the book at ingest — is never touched, whatever the new
    /// rate says. Filled rows are stamped `cost_source = "book_fill"` and `priced_at`, so a
    /// reconstructed cost stays distinguishable from one that was right at the time. Idempotent by
    /// construction: a second fill finds nothing left to fill and returns 0.
    fn fill_unpriced_cost(&self, _f: &pricing::PriceFill<'_>) -> Result<u64> {
        Err(StoreError::Unsupported("the unpriced-cost forward fill"))
    }

    /// Every stored rate for one key, newest `effective_from` first — the price timeline.
    ///
    /// `list_prices` answers "what are we charging *now*"; this answers "what were we charging in
    /// June", which is the question a cost number from June can only be defended with.
    fn list_price_history(&self, _provider: &str, _model: &str) -> Result<Vec<ModelPriceRow>> {
        Err(StoreError::Unsupported("the dated price-book history"))
    }

    // --- the relay device fleet (M18, docs/RELAY.md) ---
    //
    // `Unsupported` by default rather than an empty fleet, for the reason the whole manifest
    // exists: "no devices are enrolled" is a *load-bearing* answer here — it is what tells the
    // enqueue door to admit a task it cannot route (the legacy shared-key deployment) — so a
    // backend that simply has no `devices` table must not be able to say it by accident.

    /// Enrol one device. `key_hash` is the salted digest of a key shown to the operator exactly
    /// once; the raw key is never stored.
    fn create_device(&self, _d: &Device) -> Result<()> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// One device by id, revoked ones included — an operator listing a fleet has to see what they
    /// revoked, and a task that named a device must keep resolving after the revocation.
    fn get_device(&self, _id: &str) -> Result<Option<Device>> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// The fleet, newest first: one project's devices, or (with `None`) every device on the
    /// instance — including the operator-wide ones, which belong to no project.
    fn list_devices(&self, _project: Option<&str>) -> Result<Vec<Device>> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// Resolve a presented `ltd_<prefix>_<secret>` by its non-secret prefix, so the caller can
    /// verify the secret against the stored digest. Exactly the `api_keys` lookup shape.
    fn find_device_by_key_prefix(&self, _prefix: &str) -> Result<Option<Device>> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// Record that this device is alive and what it currently advertises: `last_seen_at = now`,
    /// plus the capability set and agent version it reported.
    ///
    /// The device's own advertisement is authoritative on purpose. A stored capability list an
    /// operator typed at enrolment goes stale the moment somebody adds an action folder, and a
    /// stale list is exactly the routing failure this surface exists to end.
    fn touch_device(
        &self,
        _id: &str,
        _capabilities: &[String],
        _agent_version: Option<&str>,
    ) -> Result<()> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// Revoke a device: it authenticates nothing and is eligible for nothing. A flag, not a delete,
    /// so the tasks it already ran keep naming a device that still resolves. `Ok(false)` = no such
    /// device.
    fn revoke_device(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }
    /// How much of the fleet could run `action_type` — both figures, because one count cannot tell
    /// "nothing is enrolled" from "nothing advertises this", and the enqueue door treats those
    /// oppositely (see [`DeviceEligibility::admit`]).
    fn count_eligible_devices(&self, _action_type: &str) -> Result<DeviceEligibility> {
        Err(StoreError::Unsupported("the relay device fleet"))
    }

    // --- alert ledger + routing (M3): the product's own audit trail ---
    //
    // Every method refuses by default rather than answering empty. An `Ok(())` here would drop the
    // one record that says an operator was told; a `[]` from `list_alerts` would read as "nothing
    // has fired", which is the single most reassuring lie this system could tell.

    /// Admit or suppress one fired alert, as **one atomic store step**.
    ///
    /// This is the durable replacement for the in-process cooldown map. A row with the same
    /// `dedup_key` fired inside `cooldown` means the same ongoing condition, so the new one is
    /// suppressed and nothing is written. Atomicity is the whole point: two API replicas evaluating
    /// the same breach in the same second must produce **one** delivered alert, and that can only be
    /// decided by the store they share.
    fn insert_alert_dedup(
        &self,
        _a: &Alert,
        _cooldown: std::time::Duration,
    ) -> Result<AlertAdmission> {
        Err(StoreError::Unsupported("the alert ledger"))
    }
    /// Append one channel's delivery outcome to an alert; `Ok(false)` = no such alert id.
    fn mark_delivery(&self, _alert_id: &str, _d: &Delivery) -> Result<bool> {
        Err(StoreError::Unsupported("the alert ledger"))
    }
    /// Fired alerts newest-first, narrowed by [`AlertFilter`] and keyset-paged on `(fired_at, id)`.
    fn list_alerts(&self, _f: &AlertFilter) -> Result<Vec<Alert>> {
        Err(StoreError::Unsupported("the alert ledger"))
    }
    fn get_alert(&self, _id: &str) -> Result<Option<Alert>> {
        Err(StoreError::Unsupported("the alert ledger"))
    }
    /// Acknowledge an alert. Idempotent in effect but honest in its answer: `Ok(false)` = no such id.
    fn ack_alert(&self, _id: &str, _by: &str, _at: DateTime<Utc>) -> Result<bool> {
        Err(StoreError::Unsupported("the alert ledger"))
    }
    /// Attach what came of an alert — the responder's diagnosis, or an operator's note.
    fn attach_alert_resolution(&self, _id: &str, _resolution: &Value) -> Result<bool> {
        Err(StoreError::Unsupported("the alert ledger"))
    }

    /// Register a routing destination. `project_id: None` is a global channel.
    fn create_alert_channel(&self, _c: &AlertChannel) -> Result<()> {
        Err(StoreError::Unsupported("alert routing"))
    }
    fn get_alert_channel(&self, _id: &str) -> Result<Option<AlertChannel>> {
        Err(StoreError::Unsupported("alert routing"))
    }
    /// Channels owned by `project`, or — with `None` — the global ones. Exactly one of the two sets,
    /// never both: [`Store::channels_for`] is the method that unions them.
    fn list_alert_channels(&self, _project: Option<&str>) -> Result<Vec<AlertChannel>> {
        Err(StoreError::Unsupported("alert routing"))
    }
    fn delete_alert_channel(&self, _id: &str) -> Result<bool> {
        Err(StoreError::Unsupported("alert routing"))
    }
    /// Where an alert for `project` goes: its own channels **∪** the global ones. A deployment that
    /// has configured nothing per-project therefore behaves exactly as it did before routing existed.
    ///
    /// The default composes the two `list_alert_channels` reads, so a backend that serves those
    /// serves this — and one that serves neither refuses here too, through the first call.
    fn channels_for(&self, project: Option<&str>) -> Result<Vec<AlertChannel>> {
        let mut out = self.list_alert_channels(None)?;
        if let Some(p) = project {
            out.extend(self.list_alert_channels(Some(p))?);
        }
        Ok(out)
    }

    // --- the contributor-side contribution ledger (M22) ---
    //
    // The mirror image of the collective surface above: that one is what a **hub** receives, this
    // one is what **this instance sent**. Every method refuses by default rather than answering
    // empty, for the reason the whole manifest exists — an empty ledger reads as "we have never
    // contributed anything", which is the one answer that makes a hash-gated push send every time
    // and a `withdraw --all` cover nothing.

    /// Append one contribution attempt to the ledger. Append-only: rows are never updated and never
    /// deleted (ARCHITECTURE §12), because the record of what left the building is the point.
    fn insert_contribution(&self, _c: &ContributionRecord) -> Result<()> {
        Err(StoreError::Unsupported("the contribution ledger"))
    }
    /// The ledger newest-first, keyset-paged on `(created_at, id)` — the same opaque cursor shape
    /// every other listing uses ([`codec::encode_event_cursor`]). `limit` of `0` means
    /// [`collective::CONTRIBUTIONS_DEFAULT_LIMIT`].
    fn list_contributions(
        &self,
        _limit: usize,
        _cursor: Option<&str>,
    ) -> Result<Vec<ContributionRecord>> {
        Err(StoreError::Unsupported("the contribution ledger"))
    }
    /// The newest row for one hub, or `None` if this instance has never pushed to it.
    ///
    /// This is the hash gate's read, so it is keyed rather than a scan: the whole point of the gate
    /// is that a scheduled push which would change nothing costs one indexed probe and no HTTP call.
    fn latest_contribution(&self, _hub_url_hash: &str) -> Result<Option<ContributionRecord>> {
        Err(StoreError::Unsupported("the contribution ledger"))
    }
}

/// What [`Store::insert_alert_dedup`] decided. There is no third answer on purpose: an alert was
/// either written and is now the caller's to deliver, or an identical one is already live and this
/// caller must stay quiet.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", tag = "admission")]
pub enum AlertAdmission {
    /// The row was written; the caller owns delivering it.
    Admitted,
    /// A row with the same `dedup_key` fired at this time, inside the cooldown.
    Suppressed { fired_at: DateTime<Utc> },
}

impl AlertAdmission {
    pub fn admitted(&self) -> bool {
        matches!(self, AlertAdmission::Admitted)
    }
}

/// How `GET /v1/alerts` narrows the ledger.
#[derive(Debug, Clone, Default)]
pub struct AlertFilter {
    pub project: Option<String>,
    pub kind: Option<AlertKind>,
    /// Only alerts fired at or after this instant.
    pub since: Option<DateTime<Utc>>,
    /// `Some(true)` = acknowledged only, `Some(false)` = open only, `None` = both.
    pub acked: Option<bool>,
    /// `0` means [`AlertFilter::DEFAULT_LIMIT`].
    pub limit: usize,
    /// Opaque keyset cursor from a previous page (see [`codec::encode_event_cursor`]).
    pub cursor: Option<String>,
}

impl AlertFilter {
    pub const DEFAULT_LIMIT: usize = 100;
    pub const MAX_LIMIT: usize = 1000;

    /// The page size to actually use: the caller's, clamped, with `0` meaning the default.
    pub fn effective_limit(&self) -> usize {
        match self.limit {
            0 => Self::DEFAULT_LIMIT,
            n => n.min(Self::MAX_LIMIT),
        }
    }
}
