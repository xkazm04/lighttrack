//! Collective-network configuration: the env-derived knobs every collective handler reads.
//!
//! Built once at boot (mirrors `Alerter`/`Redactor`) and parked on [`crate::state::AppState`], so the
//! handlers never touch process env on the request path.

use chrono::Utc;

use lighttrack_core::{ModelAliases, DEFAULT_LOW_CONFIDENCE_CASES, DEFAULT_MIN_CASES};

use crate::auth::AuthMode;

use super::identity::opaque;

/// Collective-network config, built from env once at boot (mirrors `Alerter`/`Redactor`).
pub(crate) struct Collective {
    /// Opaque, stable id this instance stamps on its *own* digest preview (a hash of
    /// `LIGHTTRACK_COLLECTIVE_ID`, or `anonymous` when unset). Never the raw id. NB: a hub **ignores**
    /// this on ingest and derives the identity from the presented bearer key — see
    /// [`super::ingest::post_ingest`].
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
    pub(crate) fn retention_cutoff(
        &self,
        now: chrono::DateTime<Utc>,
    ) -> Option<chrono::DateTime<Utc>> {
        (self.max_age_days > 0).then(|| now - chrono::Duration::days(self.max_age_days as i64))
    }

    /// Say out loud, at boot, when this hub's `min_contributors` floor cannot mean what it says.
    /// A dev-mode hub can't distinguish one unrecognized bearer string from another, so contributions
    /// from uncredentialed posters are refused at ingest (see
    /// [`super::identity::resolve_contributor`]) — which makes a dev-mode hub effectively closed
    /// unless keys are minted or anon is opted into. Better to name that at startup than to have
    /// operators discover it as a wall of 403s.
    pub(crate) fn warn_if_hub_is_weak(&self, mode: AuthMode) {
        if !self.accept {
            return;
        }
        if mode == AuthMode::Dev {
            tracing::warn!(
                min_contributors = self.min_contributors,
                "collective hub is accepting contributions while auth mode is DEV. \
                 min_contributors cannot be enforced against forged identities in dev mode, so only \
                 hub-issued contributor keys (a project with collective_opt_in) and the admin key may \
                 contribute; every other poster is refused. Run with LIGHTTRACK_AUTH_MODE=enforced for a real hub.",
            );
        }
        if self.allow_anon {
            tracing::warn!(
                anon_identity = lighttrack_core::collective::ANON_CONTRIBUTOR,
                min_contributors = self.min_contributors,
                "LIGHTTRACK_COLLECTIVE_ALLOW_ANON=1 — uncredentialed contributions all land under \
                 one shared identity and overwrite each other; they count as ONE source toward \
                 min_contributors.",
            );
        }
    }

    pub(crate) fn describe(&self) -> String {
        let who = if self.contributor_id == "anonymous" {
            "anon"
        } else {
            "id-set"
        };
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
    matches!(
        std::env::var(name).as_deref(),
        Ok("1") | Ok("true") | Ok("on") | Ok("yes")
    )
}

fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(default)
}

/// Load the model-alias table from `LIGHTTRACK_MODEL_ALIASES` (default `config/model_aliases.json`).
/// Absent ⇒ an empty (pass-through) table; a parse error is logged and normalization is disabled.
fn load_aliases() -> ModelAliases {
    let path = std::env::var("LIGHTTRACK_MODEL_ALIASES")
        .unwrap_or_else(|_| "config/model_aliases.json".to_string());
    match std::fs::read_to_string(&path) {
        Ok(s) => ModelAliases::from_json_str(&s).unwrap_or_else(|e| {
            tracing::warn!(path = %path, error = %e, "model aliases parse error; normalization disabled");
            ModelAliases::default()
        }),
        Err(_) => ModelAliases::default(),
    }
}
