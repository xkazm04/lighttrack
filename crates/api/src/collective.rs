//! Collective Model Intelligence Network — the opt-in network-effect surface.
//!
//! Three endpoints, mirroring the design in `docs/BENCHMARK_FRAMEWORK.md`:
//! - `GET  /v1/collective/digest` — build *this* instance's privacy-safe digest from its own benchmark
//!   run scorecards (admin; a preview of what it would contribute). Never reads `events`.
//! - `POST /v1/collective/ingest` — a hub receives a digest from a contributor and stores it (gated by
//!   `LIGHTTRACK_COLLECTIVE_ACCEPT`; off by default).
//! - `GET  /v1/collective/leaderboard` — the merged public leaderboard across all contributors.
//!
//! Privacy lives in `core::collective`: digests are aggregate-only and k-anonymized; the contributor
//! id is an opaque, non-reversible hash so a hub can update a source idempotently without learning who
//! it is.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use lighttrack_core::{
    build_digest, merge_leaderboard, task_type_from, Benchmark, BenchmarkRun, CollectiveDigest,
    CollectiveEntry, LeaderboardRow, ModelAliases, RunStat, DEFAULT_LOW_CONFIDENCE_CASES,
    DEFAULT_MIN_CASES, DIGEST_SCHEMA_VERSION, MIN_SCHEMA_VERSION,
};
use lighttrack_store::{Store, StoreError};

use crate::auth::{AuthMode, Principal};
use crate::error::ApiError;
use crate::guards::{authenticate, bearer, ensure_can_admin};
use crate::state::{spawn_db, AppState};

/// Collective-network config, built from env once at boot (mirrors `Alerter`/`Redactor`).
pub(crate) struct Collective {
    /// Opaque, stable id this instance stamps on its *own* digest preview (a hash of
    /// `LIGHTTRACK_COLLECTIVE_ID`, or `anonymous` when unset). Never the raw id. NB: a hub **ignores**
    /// this on ingest and derives the identity from the presented bearer key — see [`post_ingest`].
    pub(crate) contributor_id: String,
    /// Whether this instance acts as a hub that accepts contributions.
    pub(crate) accept: bool,
    /// Hub-side: accept anonymous (keyless) contributions under a single shared `anonymous` identity.
    /// Off by default — a keyless push is refused so one poster can't masquerade as many.
    pub(crate) allow_anon: bool,
    /// Hub-side k-anonymity floor: buckets contributed with `n_cases` below this are dropped on ingest,
    /// regardless of what floor the contributor claims to have used. Clamped to ≥1.
    pub(crate) min_cases: u32,
    /// Leaderboard display floor: merged rows with fewer than this many total cases are flagged
    /// `low_confidence` (shown, not hidden).
    pub(crate) display_floor: u32,
    /// k-anonymity floor over **sources**: merged rows backed by fewer than this many distinct
    /// contributors are withheld from the leaderboard entirely. `min_cases` anonymizes over cases
    /// *within* one contributor's bucket — it does nothing against a row whose numbers all belong to
    /// a single instance, which `?provider=`/`?task_type=` can isolate in one request. Default 2 (the
    /// weakest defensible K); a private/single-tenant hub sets 1 to opt out explicitly.
    pub(crate) min_contributors: u32,
    /// Minimum hours between two contributions from the same source. Ingest is delete-then-replace and
    /// the source id is stable, so a hub operator who can diff successive pushes learns what changed
    /// inside a contributor's private benchmark suite ("a new task type appeared", "their cost dropped
    /// 30%"). Rate-limiting the pushes bounds how fine-grained that differencing can be. `0` (the
    /// default) disables the limit — see `docs/BENCHMARK_FRAMEWORK.md` for what that costs.
    pub(crate) min_interval_hours: u64,
    /// Days after which a contributed entry stops being published and is swept. A benchmark result
    /// from a year ago describes a model that has since been retrained; keeping it forever also means a
    /// contributor that loses its key leaves rows behind permanently. `0` disables expiry.
    pub(crate) max_age_days: u64,
    /// Model-identity normalization applied to `(provider, model)` at ingest, so `gpt-4o` /
    /// `openai/gpt-4o` / `gpt-4o-2024-08-06` collapse to one leaderboard row. Empty ⇒ pass-through.
    pub(crate) aliases: ModelAliases,
}

/// Default retention for contributed entries: a quarter. Long enough that a monthly contributor stays
/// on the board, short enough that the leaderboard describes models as they are now.
const DEFAULT_MAX_AGE_DAYS: u64 = 90;

impl Collective {
    pub(crate) fn from_env() -> Self {
        let contributor_id = match std::env::var("LIGHTTRACK_COLLECTIVE_ID") {
            Ok(id) if !id.trim().is_empty() => format!("c-{}", opaque(id.trim())),
            _ => lighttrack_core::collective::ANON_CONTRIBUTOR.to_string(),
        };
        let accept = env_flag("LIGHTTRACK_COLLECTIVE_ACCEPT");
        let allow_anon = env_flag("LIGHTTRACK_COLLECTIVE_ALLOW_ANON");
        let min_cases = std::env::var("LIGHTTRACK_COLLECTIVE_MIN_CASES")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_MIN_CASES)
            .max(1);
        let display_floor = std::env::var("LIGHTTRACK_COLLECTIVE_DISPLAY_FLOOR")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(DEFAULT_LOW_CONFIDENCE_CASES);
        let min_contributors = std::env::var("LIGHTTRACK_COLLECTIVE_MIN_CONTRIBUTORS")
            .ok()
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(2)
            .max(1);
        let min_interval_hours = env_u64("LIGHTTRACK_COLLECTIVE_MIN_INTERVAL_HOURS", 0);
        let max_age_days = env_u64("LIGHTTRACK_COLLECTIVE_MAX_AGE_DAYS", DEFAULT_MAX_AGE_DAYS);
        let aliases = load_aliases();
        Self {
            contributor_id,
            accept,
            allow_anon,
            min_cases,
            display_floor,
            min_contributors,
            min_interval_hours,
            max_age_days,
            aliases,
        }
    }

    /// Cutoff before which stored entries are neither published nor kept. `None` when expiry is off.
    pub(crate) fn retention_cutoff(&self, now: chrono::DateTime<Utc>) -> Option<chrono::DateTime<Utc>> {
        (self.max_age_days > 0).then(|| now - chrono::Duration::days(self.max_age_days as i64))
    }

    /// Say out loud, at boot, when this hub's `min_contributors` floor cannot mean what it says.
    /// A dev-mode hub can't distinguish one unrecognized bearer string from another, so contributions
    /// from uncredentialed posters are refused at ingest (see [`resolve_contributor`]) — which makes a
    /// dev-mode hub effectively closed unless keys are minted or anon is opted into. Better to name
    /// that at startup than to have operators discover it as a wall of 403s.
    pub(crate) fn warn_if_hub_is_weak(&self, mode: AuthMode) {
        if !self.accept {
            return;
        }
        if mode == AuthMode::Dev {
            eprintln!(
                "WARNING: collective hub is accepting contributions while auth mode is DEV. \
                 min_contributors={} cannot be enforced against forged identities in dev mode, so only \
                 hub-issued contributor keys (a project with collective_opt_in) and the admin key may \
                 contribute; every other poster is refused. Run with LIGHTTRACK_AUTH_MODE=enforced for a real hub.",
                self.min_contributors
            );
        }
        if self.allow_anon {
            eprintln!(
                "WARNING: LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1 — uncredentialed contributions all land \
                 under one shared '{}' identity and overwrite each other; they count as ONE source \
                 toward min_contributors={}.",
                lighttrack_core::collective::ANON_CONTRIBUTOR,
                self.min_contributors
            );
        }
    }

    pub(crate) fn describe(&self) -> String {
        let who = if self.contributor_id == "anonymous" { "anon" } else { "id-set" };
        format!(
            "{who}, accept={}, allow_anon={}, min_cases={}, display_floor={}, min_contributors={}, \
             min_interval_h={}, max_age_d={}",
            self.accept,
            self.allow_anon,
            self.min_cases,
            self.display_floor,
            self.min_contributors,
            self.min_interval_hours,
            self.max_age_days
        )
    }
}

fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true") | Ok("on") | Ok("yes"))
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default)
}

/// Load the model-alias table from `LIGHTTRACK_MODEL_ALIASES` (default `config/model_aliases.json`).
/// Absent ⇒ an empty (pass-through) table; a parse error is logged and normalization is disabled.
fn load_aliases() -> ModelAliases {
    let path = std::env::var("LIGHTTRACK_MODEL_ALIASES")
        .unwrap_or_else(|_| "config/model_aliases.json".to_string());
    match std::fs::read_to_string(&path) {
        Ok(s) => ModelAliases::from_json_str(&s).unwrap_or_else(|e| {
            eprintln!("model aliases parse error in {path}: {e}; normalization disabled");
            ModelAliases::default()
        }),
        Err(_) => ModelAliases::default(),
    }
}

/// First 12 hex chars of SHA-256 — opaque and non-reversible, enough to keep contributors distinct.
fn opaque(s: &str) -> String {
    let mut h = Sha256::new();
    h.update(s.as_bytes());
    h.finalize().iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// Derive a hub-side contributor id from a **verified** credential: `c-` + the first 12 hex of
/// SHA-256 of the credential's stable identifier (an `api_keys.id`, or the admin key). The id is
/// never taken from the request body, and — since only a credential the hub itself issued can reach
/// this function — a poster can neither overwrite a victim's set nor mint unlimited ids to inflate
/// `n_contributors`. See [`resolve_contributor`].
fn derive_contributor_id(credential: &str) -> String {
    format!("c-{}", opaque(credential))
}

/// Resolve the contributing identity behind an ingest request, or refuse.
///
/// **Why this is not just `authenticate`.** `authenticate` is *lenient in dev mode*: it maps any
/// unrecognized bearer string to [`Principal::Dev`]. Hashing the presented token would therefore let
/// one poster on a dev-mode hub mint an unbounded number of distinct contributor ids and walk straight
/// through `min_contributors` — the floor both the k-anonymity guarantee and the "≥2 independent
/// sources" story rest on. So the identity is derived from a credential the hub *issued*, never from
/// the bytes the poster typed:
///   - [`Principal::Project`] — a key the hub minted, **and** whose project carries
///     `collective_opt_in`. That opt-in is the contribution scope: an ordinary ingest key belongs to a
///     project that never consented, so it cannot contribute. Identity = hash of the `api_keys.id`.
///   - [`Principal::Admin`] — the hub operator pushing its own digest. One key, one identity.
///   - [`Principal::Dev`] — no credential at all (or an unrecognized token on a dev-mode hub).
///     Refused, unless `allow_anon`, in which case *every* such poster collapses into the single
///     shared `anonymous` identity — one source, not N, so nothing can be forged from it either.
async fn resolve_contributor(st: &AppState, headers: &HeaderMap) -> Result<String, ApiError> {
    match authenticate(st, headers).await? {
        Principal::Project { project_id, key_id } => {
            let store = st.store.clone();
            let pid = project_id.clone();
            let project = spawn_db(move || store.get_project(&pid)).await?;
            if !project.map(|p| p.collective_opt_in).unwrap_or(false) {
                return Err(ApiError::forbidden(
                    "this key may not contribute: contribution requires a key whose project has \
                     collective_opt_in set — an ordinary ingest key is not a contributor credential",
                ));
            }
            Ok(derive_contributor_id(&key_id))
        }
        Principal::Admin => Ok(derive_contributor_id(
            st.admin_key.as_deref().unwrap_or("admin"),
        )),
        Principal::Dev => {
            if !st.collective.allow_anon {
                let hint = if bearer(headers).is_some() && st.auth_mode == AuthMode::Dev {
                    "the presented token is not a key this hub issued, and a dev-mode hub cannot tell \
                     one unrecognized token from another — min_contributors cannot be enforced against \
                     forged identities, so the contribution is refused"
                } else {
                    "anonymous (keyless) contributions are refused; present a contributor key, or set \
                     LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1 to accept them under one shared identity"
                };
                return Err(ApiError::forbidden(hint));
            }
            eprintln!(
                "WARNING: accepting an ANONYMOUS collective contribution (LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1) \
                 — every uncredentialed poster shares the '{}' identity and overwrites the others' set",
                lighttrack_core::collective::ANON_CONTRIBUTOR
            );
            Ok(lighttrack_core::collective::ANON_CONTRIBUTOR.to_string())
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct DigestParams {
    /// k-anonymity floor; defaults to [`DEFAULT_MIN_CASES`]. Clamped to ≥1.
    min_cases: Option<u32>,
}

/// Build this instance's digest from every benchmark run it stores (admin-only — it walks all
/// projects). Returns what `lt collective contribute` would POST to a hub.
pub(crate) async fn get_digest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<DigestParams>,
) -> Result<Json<CollectiveDigest>, ApiError> {
    ensure_can_admin(&authenticate(&st, &headers).await?)?;
    let min_cases = q.min_cases.unwrap_or(DEFAULT_MIN_CASES).max(1);

    let store = st.store.clone();
    let (stats, projects_included, projects_excluded) =
        spawn_db(move || gather_run_stats(store.as_ref())).await?;
    let entries = build_digest(&stats, min_cases);
    Ok(Json(CollectiveDigest {
        schema_version: DIGEST_SCHEMA_VERSION,
        contributor_id: st.collective.contributor_id.clone(),
        generated_at: Utc::now(),
        min_cases,
        projects_included,
        projects_excluded,
        entries,
    }))
}

#[derive(Serialize)]
pub(crate) struct IngestAck {
    /// The **hub-derived** identity this contribution landed under (from the bearer key, not the body).
    contributor_id: String,
    accepted: usize,
    /// Entries dropped as malformed / identity-less (empty provider, model, or task_type).
    skipped: usize,
    /// Entries dropped for failing the hub's enforced k-anonymity floor (`n_cases < min_cases`).
    dropped_under_min: usize,
    /// Entries refused as not-believable benchmark results (see `implausible`) — a claim of a billion
    /// cases is disclosed back to the contributor, never silently absorbed into a merged row.
    rejected_implausible: usize,
}

/// Hub side: accept a contributor's digest and replace its stored entry set (delete-then-upsert so a
/// bucket that fell below the floor doesn't linger). Off unless `LIGHTTRACK_COLLECTIVE_ACCEPT` is set.
///
/// Hardening: the contributor identity is **derived from a credential the hub issued**, never trusted
/// from the request body — so a poster can only ever replace *its own* set, and cannot mint identities
/// to defeat `min_contributors` (see [`resolve_contributor`]). Contribution needs a key whose project
/// carries `collective_opt_in`; an uncredentialed push is refused unless
/// `LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1`, in which case it lands under one shared `anonymous` identity
/// with a loud warning. The hub also re-enforces its own k-anonymity floor
/// (`LIGHTTRACK_COLLECTIVE_MIN_CASES`), dropping under-k buckets rather than trusting the poster's floor.
pub(crate) async fn post_ingest(
    State(st): State<AppState>,
    headers: HeaderMap,
    Json(digest): Json<CollectiveDigest>,
) -> Result<Json<IngestAck>, ApiError> {
    if !st.collective.accept {
        return Err(ApiError::forbidden(
            "this instance does not accept collective contributions (set LIGHTTRACK_COLLECTIVE_ACCEPT=1)",
        ));
    }
    if !(MIN_SCHEMA_VERSION..=DIGEST_SCHEMA_VERSION).contains(&digest.schema_version) {
        return Err(ApiError::bad_request(format!(
            "unsupported digest schema_version {} (this hub accepts v{MIN_SCHEMA_VERSION}..=v{DIGEST_SCHEMA_VERSION})",
            digest.schema_version
        )));
    }

    // Identity comes from a hub-issued credential; the body's `contributor_id` is ignored (wire compat).
    let contributor = resolve_contributor(&st, &headers).await?;

    let min_cases = st.collective.min_cases;
    let now = Utc::now();
    enforce_min_interval(&st, &contributor, now).await?;
    let mut skipped = 0usize;
    let mut dropped_under_min = 0usize;
    let mut rejected_implausible = 0usize;
    let entries: Vec<CollectiveEntry> = digest
        .entries
        .into_iter()
        .filter_map(|e| match sanitize_entry(&contributor, e, now, &st.collective.aliases) {
            Err(Reject::Malformed) => {
                skipped += 1;
                None
            }
            Err(Reject::Implausible) => {
                rejected_implausible += 1;
                None
            }
            Ok(ce) if ce.n_cases < min_cases => {
                dropped_under_min += 1;
                None
            }
            Ok(ce) => Some(ce),
        })
        .take(MAX_ENTRIES)
        .collect();
    let accepted = entries.len();

    let store = st.store.clone();
    let contrib = contributor.clone();
    let cutoff = st.collective.retention_cutoff(now);
    spawn_db(move || -> Result<(), StoreError> {
        store.delete_collective_entries(&contrib)?;
        for e in &entries {
            store.upsert_collective_entry(e)?;
        }
        // Retention sweep, piggy-backed on the write that already holds the connection. A backend
        // without a sweep still enforces the policy at read time, so `Unsupported` is not an error.
        if let Some(c) = cutoff {
            match store.purge_collective_entries_before(c) {
                Ok(_) | Err(StoreError::Unsupported(_)) => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    })
    .await?;

    Ok(Json(IngestAck {
        contributor_id: contributor,
        accepted,
        skipped,
        dropped_under_min,
        rejected_implausible,
    }))
}

/// Bound how finely a hub operator can difference a contributor over time.
///
/// Ingest is delete-then-replace under a stable source id, so successive pushes are a changelog of a
/// contributor's private benchmark suite: a new `task_type` appearing, a cost moving 30%, a bucket
/// vanishing. Nothing in the payload leaks that — the *sequence* does. A minimum interval makes the
/// changelog coarse; `0` (default) leaves it off, which is why the exposure is also documented for
/// both sides in `docs/BENCHMARK_FRAMEWORK.md` rather than being quietly relied on.
async fn enforce_min_interval(
    st: &AppState,
    contributor: &str,
    now: chrono::DateTime<Utc>,
) -> Result<(), ApiError> {
    let hours = st.collective.min_interval_hours;
    if hours == 0 {
        return Ok(());
    }
    let store = st.store.clone();
    let who = contributor.to_string();
    let last = spawn_db(move || {
        store.list_collective_entries().map(|es| {
            es.iter().filter(|e| e.contributor_id == who).map(|e| e.received_at).max()
        })
    })
    .await?;
    let Some(last) = last else { return Ok(()) };
    let next = last + chrono::Duration::hours(hours as i64);
    if now < next {
        let secs = (next - now).num_seconds().max(1) as u64;
        return Err(ApiError::rate_limited(format!(
            "this hub accepts one contribution per source every {hours}h (frequent re-pushes let a hub \
             operator difference your private benchmark suite); retry in {secs}s"
        ))
        .retry_after(Some(secs)));
    }
    Ok(())
}

#[derive(Deserialize)]
pub(crate) struct WithdrawParams {
    /// Admin-only escape hatch: withdraw a *named* source. The point is the contributor that lost its
    /// key — without this, its rows would be unreachable forever. A non-admin may only withdraw itself.
    contributor: Option<String>,
}

#[derive(Serialize)]
pub(crate) struct WithdrawAck {
    contributor_id: String,
    deleted: u64,
}

/// The right to withdraw: `DELETE /v1/collective/contribution` removes every entry a source
/// contributed. Authenticated exactly like ingest — you may withdraw what you could have published —
/// so a contributor can leave the network without asking the hub operator, and consent stays revocable
/// rather than one-way.
pub(crate) async fn delete_contribution(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<WithdrawParams>,
) -> Result<Json<WithdrawAck>, ApiError> {
    let self_id = resolve_contributor(&st, &headers).await?;
    let target = match q.contributor.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        None => self_id,
        Some(other) if other == self_id => self_id,
        Some(other) => {
            ensure_can_admin(&authenticate(&st, &headers).await?)?;
            other.to_string()
        }
    };
    let store = st.store.clone();
    let who = target.clone();
    let deleted = spawn_db(move || store.delete_collective_entries(&who)).await?;
    Ok(Json(WithdrawAck { contributor_id: target, deleted }))
}

#[derive(Deserialize)]
pub(crate) struct LeaderboardParams {
    /// Filter to one task-type bucket (e.g. `qa`, `summarization`).
    task_type: Option<String>,
    /// Filter to one provider (e.g. `anthropic`).
    provider: Option<String>,
    /// Filter to rows scored (at least partly) by one judge family (`anthropic|openai|google|unknown`).
    judge: Option<String>,
    /// Rigor filter — keep only rows whose **weakest** determinism stamp is this level, i.e. rows
    /// where *every* source ran at that level or better is expressed by asking for the level itself
    /// (`?determinism=exact` ⇒ every source was exact). An unknown label matches nothing.
    determinism: Option<String>,
    /// Rigor filter — `true` keeps only rows whose every source ran against a frozen, single-version
    /// dataset (`frozen_dataset = all`); `false` keeps rows that are anything less than that.
    frozen_dataset: Option<bool>,
    /// Rigor filter — `true` keeps only rows whose every source carried a significance-tested verdict.
    significance_tested: Option<bool>,
}

#[derive(Serialize)]
pub(crate) struct LeaderboardResponse {
    /// Distinct contributing instances **backing the visible rows** — computed over the filtered row
    /// set, so it never disagrees with what's shown. A filter that excludes a contributor's only rows
    /// drops it from this count.
    contributors: usize,
    /// Distinct `(provider, model)` identities across the filtered rows — a true model count, not a row
    /// count. (A single model spans multiple rows when it appears under several task types.)
    n_models: usize,
    /// Number of visible leaderboard rows after filtering (one per `(provider, model, task_type)`).
    n_rows: usize,
    /// Rows withheld for having fewer than the hub's `min_contributors` distinct sources — disclosed
    /// rather than silently shrinking the board, so an empty/short board is legible.
    held_back: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    task_type: Option<String>,
    rows: Vec<LeaderboardRow>,
}

/// The merged public leaderboard. Readable by anyone the API lets in (no admin) — the whole point is
/// that every operator benefits.
pub(crate) async fn get_leaderboard(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<LeaderboardParams>,
) -> Result<Json<LeaderboardResponse>, ApiError> {
    authenticate(&st, &headers).await?;
    let store = st.store.clone();
    let mut entries = spawn_db(move || store.list_collective_entries()).await?;

    // Retention, enforced at read time so the policy holds on every backend — including those whose
    // sweep is unimplemented, where the row survives on disk but is never published again.
    if let Some(cutoff) = st.collective.retention_cutoff(Utc::now()) {
        entries.retain(|e| e.received_at >= cutoff);
    }

    let mut rows = merge_leaderboard(&entries, st.collective.display_floor);

    // k-anonymity over SOURCES, applied before any filter: a row backed by fewer than
    // `min_contributors` distinct instances is not "the collective", it is that instance's private
    // eval results on a billboard — and a `?provider=` filter must never be able to strip a row down
    // to a lone source. (`min_cases` is a *case*-count floor within one contributor's bucket; it does
    // not anonymize across contributors. A 5000-case single-source row is still one source.)
    let k = st.collective.min_contributors;
    let held_back = {
        let before = rows.len();
        rows.retain(|r| r.n_contributors >= k);
        before - rows.len()
    };

    if let Some(tt) = q.task_type.as_deref() {
        rows.retain(|r| r.task_type == tt);
    }
    if let Some(p) = q.provider.as_deref() {
        rows.retain(|r| r.provider == p);
    }
    if let Some(j) = q.judge.as_deref() {
        rows.retain(|r| r.judge_providers.iter().any(|p| p == j));
    }
    // Rigor filters — deliberately applied HERE, after the `min_contributors` retain above, for the
    // same reason `?provider=` is: rigor is a low-cardinality but real fingerprint, and a filter that
    // ran before the source floor could strip a row down to a lone contributor's private eval.
    if let Some(d) = q.determinism.as_deref() {
        let want = lighttrack_core::canon_determinism(d);
        rows.retain(|r| want.is_some() && r.rigor.determinism == want);
    }
    if let Some(want) = q.frozen_dataset {
        rows.retain(|r| (r.rigor.frozen_dataset == lighttrack_core::Coverage::All) == want);
    }
    if let Some(want) = q.significance_tested {
        rows.retain(|r| (r.rigor.significance_tested == lighttrack_core::Coverage::All) == want);
    }

    // Header counts are computed over the FILTERED rows so they never disagree with what's shown.
    // Contributors backing the visible rows = distinct contributor ids of every stored entry whose
    // `(provider, model, task_type)` survived filtering (an entry's identity is normalized at ingest,
    // so its key matches the merged row's key exactly).
    let surviving: std::collections::BTreeSet<(&str, &str, &str)> = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str(), r.task_type.as_str()))
        .collect();
    let contributors = entries
        .iter()
        .filter(|e| surviving.contains(&(e.provider.as_str(), e.model.as_str(), e.task_type.as_str())))
        .map(|e| e.contributor_id.as_str())
        .collect::<std::collections::BTreeSet<_>>()
        .len();
    let n_models = rows
        .iter()
        .map(|r| (r.provider.as_str(), r.model.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
        .len();

    Ok(Json(LeaderboardResponse {
        contributors,
        n_models,
        n_rows: rows.len(),
        held_back,
        task_type: q.task_type,
        rows,
    }))
}

/// Hard cap on entries accepted from one contributor, so a malformed/abusive digest can't blow up.
const MAX_ENTRIES: usize = 5000;

/// Walk the **consenting** projects' benchmarks and reduce each run scorecard to a [`RunStat`].
/// A project contributes only when `collective_opt_in` is set — contribution is an act, not an
/// inheritance, so an NDA'd project sitting next to a dozen internal ones can never ship by accident.
/// Returns `(stats, projects_included, projects_excluded)` so the digest discloses its own scope.
/// Only runs whose model identity is known and that scored ≥1 case contribute.
fn gather_run_stats(store: &dyn Store) -> Result<(Vec<RunStat>, u32, u32), StoreError> {
    let mut stats = Vec::new();
    let (mut included, mut excluded) = (0u32, 0u32);
    for p in store.list_projects()? {
        if !p.collective_opt_in {
            excluded += 1;
            continue;
        }
        included += 1;
        for b in store.list_benchmarks(&p.id)? {
            for run in store.list_benchmark_runs(&b.id)? {
                if let Some(s) = run_stat(&b, &run) {
                    stats.push(s);
                }
            }
        }
    }
    Ok((stats, included, excluded))
}

/// Reduce one `(Benchmark, run)` to a [`RunStat`], or `None` when it can't contribute (no known
/// provider/model, no quality, or no cases).
fn run_stat(bench: &Benchmark, run: &BenchmarkRun) -> Option<RunStat> {
    let (provider, model) = provider_model(bench, run)?;
    let quality = run.mean_score?;
    if run.n_cases == 0 {
        return None;
    }
    let cost_per_case_usd = run.cost_usd / run.n_cases as f64;
    Some(RunStat {
        provider,
        model,
        task_type: task_type_from(&bench.name, None),
        quality,
        pass_rate: run.pass_rate.unwrap_or(0.0),
        cost_per_case_usd,
        n_cases: run.n_cases,
        p50_latency_ms: run.p50_latency_ms,
        p95_latency_ms: run.p95_latency_ms,
        judge_provider: judge_provider_of(&bench.judge_model),
        rubric_fingerprint: rubric_fingerprint_of(bench),
        determinism: run
            .report
            .get("determinism")
            .and_then(Value::as_str)
            .and_then(lighttrack_core::canon_determinism),
        dataset_frozen: run.report.get("dataset_frozen").and_then(Value::as_bool),
        dataset_version: run
            .report
            .get("dataset_version")
            .and_then(Value::as_u64)
            .map(|v| v.min(u32::MAX as u64) as u32),
        significance_tested: significance_tested_of(&run.report),
    })
}

/// Whether a run's verdict was **significance-tested**: the report carries a two-sided interval
/// (`ci95`) over at least two scored cases. `n < 2` has no spread, so its "interval" is a point
/// dressed up as one — that counts as untested, not as tested. `None` when the run predates the
/// significance annotation entirely (no `n` recorded), so an old run reads as *unknown* rather than
/// being libelled as sloppy.
fn significance_tested_of(report: &Value) -> Option<bool> {
    let n = report.get("n").and_then(Value::as_u64)?;
    let has_ci = report.get("ci95").and_then(Value::as_array).is_some_and(|a| a.len() == 2);
    Some(n >= 2 && has_ci)
}

/// Classify a benchmark's `judge_model` (`[provider/]model`) into a coarse judge family — provider
/// only (`anthropic|openai|google|unknown`), never the full model, to limit fingerprinting. An
/// explicit `provider/` prefix wins; otherwise the family is inferred from the model name.
fn judge_provider_of(judge_model: &str) -> Option<String> {
    let m = judge_model.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }
    let (prefix, name) = m.split_once('/').unwrap_or(("", m.as_str()));
    let canon_prefix = match prefix {
        "anthropic" | "claude" => Some("anthropic"),
        "openai" | "azure-openai" | "azure" => Some("openai"),
        "google" | "gemini" | "vertex" | "google-vertex" => Some("google"),
        _ => None,
    };
    if let Some(c) = canon_prefix {
        return Some(c.to_string());
    }
    let name = if name.is_empty() { m.as_str() } else { name };
    let family = if ["claude", "haiku", "sonnet", "opus"].iter().any(|k| name.contains(k)) {
        "anthropic"
    } else if name.contains("gpt") || name.starts_with("o1") || name.starts_with("o3") {
        "openai"
    } else if name.contains("gemini") || name.contains("gemma") || name.contains("bison") {
        "google"
    } else {
        "unknown"
    };
    Some(family.to_string())
}

/// A short, one-way fingerprint of a benchmark's rubric shape — 8 hex of SHA-256 over the
/// whitespace-normalized rubric definition (or its id, if the text is empty). Lets two instances tell
/// whether they scored under the same rubric without either revealing the rubric text. `None` when the
/// benchmark carries no rubric at all.
fn rubric_fingerprint_of(bench: &Benchmark) -> Option<String> {
    let basis = if !bench.rubric.trim().is_empty() {
        bench.rubric.split_whitespace().collect::<Vec<_>>().join(" ")
    } else {
        bench.rubric_id.as_deref().map(str::trim).filter(|s| !s.is_empty())?.to_string()
    };
    let mut h = Sha256::new();
    h.update(basis.as_bytes());
    Some(h.finalize().iter().take(4).map(|b| format!("{b:02x}")).collect())
}

/// Resolve the model identity from the compare-mode run report, else the benchmark's single target.
fn provider_model(bench: &Benchmark, run: &BenchmarkRun) -> Option<(String, String)> {
    let from = |v: &Value| {
        let p = v.get("provider").and_then(Value::as_str)?.trim().to_string();
        let m = v.get("model").and_then(Value::as_str)?.trim().to_string();
        (!p.is_empty() && !m.is_empty()).then_some((p, m))
    };
    from(&run.report).or_else(|| from(&bench.target))
}

/// Why a contributed entry was refused. Kept apart in the ack so a contributor can tell "you sent
/// junk" from "your numbers are not believable".
#[derive(Debug, PartialEq, Eq)]
enum Reject {
    /// No usable model identity (empty provider / model / task_type, or zero cases).
    Malformed,
    /// Structurally fine but not a believable benchmark result — see [`implausible`].
    Implausible,
}

/// The largest per-bucket case count a hub will believe from one contributor. A single
/// `(model, task_type)` bucket with more than a million scored cases is a typo or an attack, not a
/// benchmark; accepting it hands the merged row to whoever types the biggest number.
const MAX_CASES_PER_ENTRY: u32 = 1_000_000;

/// The largest per-case cost a hub will believe. $1000 for one case is not a price, it is noise.
const MAX_COST_PER_CASE_USD: f64 = 1_000.0;

/// The plausibility rules, written down in one place so they can be documented verbatim:
///   - every published number is finite (no NaN/∞ smuggled through JSON);
///   - `n_runs ≥ 1` — a bucket with no runs produced no cases;
///   - `n_cases ≥ n_runs` — a run scores at least one case, so more runs than cases is impossible;
///   - `n_cases ≤ MAX_CASES_PER_ENTRY`;
///   - `avg_cost_usd ≤ MAX_COST_PER_CASE_USD`.
///
/// Quality/pass-rate are *clamped* rather than rejected (a `[0,1]` overshoot is a rounding artifact);
/// counts are *rejected*, because a count is the weight the merge trusts.
fn implausible(e: &lighttrack_core::ModelDigestEntry) -> bool {
    !e.quality.is_finite()
        || !e.pass_rate.is_finite()
        || !e.avg_cost_usd.is_finite()
        || e.n_runs == 0
        || e.n_cases < e.n_runs
        || e.n_cases > MAX_CASES_PER_ENTRY
        || e.avg_cost_usd > MAX_COST_PER_CASE_USD
}

/// Validate/clamp one contributed entry. The model identity is **normalized** through `aliases` so
/// equivalent spellings merge into one leaderboard row.
fn sanitize_entry(
    contributor: &str,
    e: lighttrack_core::ModelDigestEntry,
    now: chrono::DateTime<Utc>,
    aliases: &ModelAliases,
) -> Result<CollectiveEntry, Reject> {
    let provider = e.provider.trim();
    let model = e.model.trim();
    let task_type = e.task_type.trim().to_string();
    if provider.is_empty() || model.is_empty() || task_type.is_empty() || e.n_cases == 0 {
        return Err(Reject::Malformed);
    }
    if implausible(&e) {
        return Err(Reject::Implausible);
    }
    let (provider, model) = aliases.normalize(provider, model);
    Ok(CollectiveEntry {
        contributor_id: contributor.to_string(),
        provider,
        model,
        task_type,
        quality: e.quality.clamp(0.0, 1.0),
        pass_rate: e.pass_rate.clamp(0.0, 1.0),
        // Re-bucketed hub-side for the same reason the k-floor is re-enforced hub-side: what the
        // contributor did to its own numbers is its business, what gets published is the hub's.
        avg_cost_usd: lighttrack_core::bucket_cost(e.avg_cost_usd.max(0.0)),
        p50_latency_ms: e.p50_latency_ms,
        p95_latency_ms: e.p95_latency_ms,
        n_runs: e.n_runs,
        n_cases: e.n_cases,
        // v2: carry the variance if present; a negative value is nonsense, so drop it to None.
        quality_variance: e.quality_variance.filter(|v| v.is_finite() && *v >= 0.0),
        // v2: clamp the judge tag to the known vocabulary; anything else is `unknown`.
        judge_provider: e
            .judge_provider
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(canon_judge),
        rubric_fingerprint: e
            .rubric_fingerprint
            .map(|r| r.trim().chars().take(32).collect::<String>())
            .filter(|s| !s.is_empty()),
        // v3 rigor: closed vocabularies, clamped hub-side. An unrecognized determinism label becomes
        // "not recorded" rather than a fourth level — a poster must not be able to widen the rigor
        // vocabulary, which is exactly what would turn it into a fingerprinting channel.
        determinism: e.determinism.as_deref().and_then(lighttrack_core::canon_determinism),
        frozen_dataset: e.frozen_dataset,
        significance_tested: e.significance_tested,
        received_at: now,
    })
}

/// Clamp a contributed judge tag to the known vocabulary (`anthropic|openai|google|mixed`), mapping
/// anything unrecognized to `unknown` so a poster can't inject arbitrary judge labels.
fn canon_judge(j: &str) -> String {
    match j.to_lowercase().as_str() {
        "anthropic" => "anthropic",
        "openai" => "openai",
        "google" => "google",
        "mixed" => "mixed",
        _ => "unknown",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn bench(name: &str, target: Value) -> Benchmark {
        Benchmark {
            id: "b1".into(),
            project_id: "p1".into(),
            name: name.into(),
            rubric: String::new(),
            judge_model: "haiku".into(),
            target,
            dataset_ref: None,
            rubric_id: None,
            dataset: vec![],
            baseline_score: None,
            created_at: Utc::now(),
        }
    }

    fn run(report: Value, mean: Option<f64>, cases: u32, cost: f64) -> BenchmarkRun {
        BenchmarkRun {
            id: "r1".into(),
            benchmark_id: "b1".into(),
            started_at: Utc::now(),
            finished_at: None,
            n_cases: cases,
            mean_score: mean,
            pass_rate: Some(0.8),
            cost_usd: cost,
            status: "compared".into(),
            p50_latency_ms: Some(700),
            p95_latency_ms: Some(1400),
            total_tokens: Some(1000),
            report,
        }
    }

    #[test]
    fn run_stat_reads_compare_report() {
        let b = bench("Nightly QA bench", Value::Null);
        let r = run(json!({"provider":"anthropic","model":"haiku"}), Some(0.82), 20, 0.4);
        let s = run_stat(&b, &r).unwrap();
        assert_eq!((s.provider.as_str(), s.model.as_str()), ("anthropic", "haiku"));
        assert_eq!(s.task_type, "qa");
        assert!((s.cost_per_case_usd - 0.02).abs() < 1e-9); // 0.4 / 20
    }

    #[test]
    fn run_stat_falls_back_to_target_then_skips() {
        // No report identity, but the benchmark's single target carries it.
        let b = bench("Summaries", json!({"provider":"openai","model":"gpt-x"}));
        let r = run(Value::Null, Some(0.7), 10, 0.1);
        let s = run_stat(&b, &r).unwrap();
        assert_eq!(s.model, "gpt-x");
        assert_eq!(s.task_type, "summarization");
        // No identity anywhere → skipped.
        let b2 = bench("x", Value::Null);
        assert!(run_stat(&b2, &run(Value::Null, Some(0.7), 10, 0.1)).is_none());
        // No quality → skipped.
        assert!(run_stat(&b, &run(json!({"provider":"a","model":"m"}), None, 10, 0.1)).is_none());
    }

    #[test]
    fn run_stat_reads_rigor_out_of_the_run_report() {
        let b = bench("QA bench", Value::Null);
        let report = json!({
            "provider": "anthropic", "model": "haiku",
            "determinism": "exact", "dataset_frozen": true, "dataset_version": 3,
            "n": 20, "ci95": [0.78, 0.86],
        });
        let s = run_stat(&b, &run(report, Some(0.82), 20, 0.4)).unwrap();
        assert_eq!(s.determinism.as_deref(), Some("exact"));
        assert_eq!(s.dataset_frozen, Some(true));
        assert_eq!(s.dataset_version, Some(3));
        assert_eq!(s.significance_tested, Some(true));
        // A run that predates the significance annotation is *unknown*, never libelled as untested.
        let bare = json!({ "provider": "anthropic", "model": "haiku" });
        let s = run_stat(&b, &run(bare, Some(0.82), 20, 0.4)).unwrap();
        assert!(s.determinism.is_none());
        assert!(s.dataset_frozen.is_none());
        assert!(s.significance_tested.is_none());
    }

    #[test]
    fn significance_needs_an_interval_over_more_than_one_case() {
        // n=1 has no spread: its "interval" is a point dressed up as one.
        assert_eq!(significance_tested_of(&json!({ "n": 1, "ci95": [0.8, 0.8] })), Some(false));
        assert_eq!(significance_tested_of(&json!({ "n": 20 })), Some(false), "no interval recorded");
        assert_eq!(significance_tested_of(&json!({ "n": 20, "ci95": [0.7, 0.9] })), Some(true));
        assert_eq!(significance_tested_of(&json!({})), None, "unrecorded ≠ untested");
    }

    #[test]
    fn opaque_id_is_stable_and_not_the_input() {
        let a = opaque("my-secret-instance-id");
        assert_eq!(a, opaque("my-secret-instance-id"));
        assert_ne!(a, "my-secret-instance-id");
        assert_eq!(a.len(), 12);
    }

    #[test]
    fn sanitize_entry_clamps_and_drops_identityless() {
        let now = Utc::now();
        let a = ModelAliases::default();
        let good = lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 1.4, pass_rate: -0.2, avg_cost_usd: -1.0,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 2, n_cases: 9,
            quality_variance: Some(-0.5), // negative variance is nonsense → dropped to None
            judge_provider: Some("weird-label".into()), // unknown label → clamped to "unknown"
            rubric_fingerprint: Some("ab12cd34".into()),
            // A determinism label outside the closed vocabulary must not become a fourth level.
            determinism: Some("perfectly-reproducible".into()),
            frozen_dataset: lighttrack_core::Coverage::All,
            significance_tested: lighttrack_core::Coverage::Mixed,
        };
        let s = sanitize_entry("c-abc", good, now, &a).unwrap();
        assert_eq!(s.quality, 1.0);
        assert_eq!(s.pass_rate, 0.0);
        assert_eq!(s.avg_cost_usd, 0.0);
        assert!(s.quality_variance.is_none(), "negative variance dropped");
        assert_eq!(s.judge_provider.as_deref(), Some("unknown"), "unknown judge label clamped");
        assert_eq!(s.rubric_fingerprint.as_deref(), Some("ab12cd34"));
        assert!(s.determinism.is_none(), "an invented determinism label is dropped, not admitted");
        assert_eq!(s.frozen_dataset, lighttrack_core::Coverage::All, "rigor coverage survives ingest");
        assert_eq!(s.significance_tested, lighttrack_core::Coverage::Mixed);
        let bad = lighttrack_core::ModelDigestEntry {
            provider: "  ".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 0.5, pass_rate: 0.5, avg_cost_usd: 0.1,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 1, n_cases: 5,
            quality_variance: None, judge_provider: None, rubric_fingerprint: None,
            determinism: None, frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        assert_eq!(sanitize_entry("c-abc", bad, now, &a).unwrap_err(), Reject::Malformed);
    }

    #[test]
    fn implausible_counts_are_rejected_not_clamped() {
        let now = Utc::now();
        let a = ModelAliases::default();
        let base = || lighttrack_core::ModelDigestEntry {
            provider: "anthropic".into(), model: "haiku".into(), task_type: "qa".into(),
            quality: 0.8, pass_rate: 0.8, avg_cost_usd: 0.01,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 2, n_cases: 100,
            quality_variance: None, judge_provider: None, rubric_fingerprint: None,
            determinism: None, frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        let rejected = |mutate: fn(&mut lighttrack_core::ModelDigestEntry)| {
            let mut e = base();
            mutate(&mut e);
            sanitize_entry("c", e, now, &a).unwrap_err()
        };
        // A billion cases in one bucket is a typo or an attack, never a benchmark.
        assert_eq!(rejected(|e| e.n_cases = 1_000_000_000), Reject::Implausible);
        // More runs than cases is arithmetically impossible.
        assert_eq!(rejected(|e| e.n_runs = 500), Reject::Implausible);
        assert_eq!(rejected(|e| e.n_runs = 0), Reject::Implausible);
        assert_eq!(rejected(|e| e.avg_cost_usd = 5_000.0), Reject::Implausible);
        assert_eq!(rejected(|e| e.quality = f64::NAN), Reject::Implausible);
        // The believable end of the range still lands.
        let mut e = base();
        e.n_cases = MAX_CASES_PER_ENTRY;
        assert!(sanitize_entry("c", e, now, &a).is_ok(), "the ceiling itself is accepted");
    }

    #[test]
    fn ingest_normalizes_model_identity() {
        let now = Utc::now();
        let a = ModelAliases::from_json_str(
            r#"{"providers":{"azure-openai":"openai"},"models":{"gpt-4o-2024-08-06":"gpt-4o"}}"#,
        )
        .unwrap();
        let e = |provider: &str, model: &str| lighttrack_core::ModelDigestEntry {
            provider: provider.into(), model: model.into(), task_type: "qa".into(),
            quality: 0.8, pass_rate: 0.8, avg_cost_usd: 0.01,
            p50_latency_ms: None, p95_latency_ms: None, n_runs: 1, n_cases: 10,
            quality_variance: None, judge_provider: None, rubric_fingerprint: None,
            determinism: None, frozen_dataset: Default::default(),
            significance_tested: Default::default(),
        };
        // provider/ prefix stripped + dated variant collapsed + provider synonym mapped.
        let s = sanitize_entry("c", e("openai", "openai/gpt-4o-2024-08-06"), now, &a).unwrap();
        assert_eq!((s.provider.as_str(), s.model.as_str()), ("openai", "gpt-4o"));
        let s = sanitize_entry("c", e("azure-openai", "gpt-4o"), now, &a).unwrap();
        assert_eq!(s.provider, "openai");
    }

    #[test]
    fn judge_provider_classification() {
        assert_eq!(judge_provider_of("anthropic/claude-haiku-4-5").as_deref(), Some("anthropic"));
        assert_eq!(judge_provider_of("haiku").as_deref(), Some("anthropic"));
        assert_eq!(judge_provider_of("gpt-4o").as_deref(), Some("openai"));
        assert_eq!(judge_provider_of("openai/o3-mini").as_deref(), Some("openai"));
        assert_eq!(judge_provider_of("gemini-1.5-pro").as_deref(), Some("google"));
        assert_eq!(judge_provider_of("some-local-llm").as_deref(), Some("unknown"));
        assert_eq!(judge_provider_of("  "), None);
    }
}
