//! Pre-spend admission: decide, before the provider call, whether to make it at all.
//!
//! Every cap LightTrack has is record-side. The server refuses to *record* a call that already cost
//! money — the money is gone by the time the 429 arrives. The signals to do better were already on
//! the wire (`usage_ratio`, `shed_fraction`, `Retry-After`, and now the `X-LightTrack-*` headers);
//! this module is what finally reads them and acts.
//!
//! Three rules shape the design:
//!
//! 1. **Pure.** [`AdmissionCache::admit`] performs no I/O and reads no clock it was not handed. A
//!    decision that could block on a network call would put LightTrack on the critical path of every
//!    LLM call in the host app — precisely the cost `docs/ARCHITECTURE.md` §4 deferred the inline
//!    gateway to avoid.
//! 2. **Fails open.** No observation, or an observation older than the TTL, admits. A telemetry
//!    client that stops an app's LLM calls because it is itself confused is worse than one that
//!    records nothing.
//! 3. **Scoped.** A cap on the `summarize` use-case must stop `summarize` and nothing else. Views
//!    are cached per binding scope, which is why the server names it.
//!
//! The verdicts are fixed across all three SDKs in `clients/contract/fixtures/limits.json`. This
//! client's shed lottery is not a port of the server's: it *is* the server's, called through
//! [`lighttrack_core::shed_ticket`].

use std::collections::HashMap;

use crate::limits::{BindingScope, LimitView};

/// How long a cached view is still evidence. Past it, [`AdmissionCache::admit`] admits and says so.
pub const DEFAULT_ADMISSION_TTL_MS: i64 = 30_000;

const PROJECT_WIDE: &str = "";

/// What the enforcing gate does with a refusal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Enforce {
    /// Refuse the call: [`crate::Client::gate`] returns [`BudgetExceeded`].
    Block,
    /// Report it on stderr and let the call proceed.
    Warn,
    /// Observe only. The default: adding an observability SDK must not change what an app does.
    #[default]
    Off,
}

impl Enforce {
    /// Parse the `LIGHTTRACK_ENFORCE` spelling. Anything unrecognized is [`Enforce::Off`] — a typo
    /// in an env var must not silently start blocking a production app's traffic.
    pub fn from_str_or_off(s: &str) -> Enforce {
        match s.trim().to_ascii_lowercase().as_str() {
            "block" => Enforce::Block,
            "warn" => Enforce::Warn,
            _ => Enforce::Off,
        }
    }
}

/// Why a call was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdmitReason {
    /// A 429's advertised wait has not elapsed.
    RetryAfter,
    /// `usage_ratio >= 1.0`.
    AtCap,
    /// The deterministic shed lottery picked this event.
    Shed,
}

impl AdmitReason {
    /// The wire spelling shared by all three SDKs.
    pub fn as_str(&self) -> &'static str {
        match self {
            AdmitReason::RetryAfter => "retry_after",
            AdmitReason::AtCap => "at_cap",
            AdmitReason::Shed => "shed",
        }
    }
}

/// The verdict on one prospective call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Admit {
    /// Whether the provider call should be made.
    pub ok: bool,
    /// `None` when ok; otherwise which condition refused it.
    pub reason: Option<AdmitReason>,
    /// Only set for [`AdmitReason::RetryAfter`] — a client must not invent a back-off the server
    /// never promised.
    pub retry_after_secs: Option<u64>,
    /// The view is past its TTL, so this verdict was taken without current evidence (and admits).
    pub stale: bool,
}

impl Admit {
    fn admitted(stale: bool) -> Admit {
        Admit {
            ok: true,
            reason: None,
            retry_after_secs: None,
            stale,
        }
    }
}

#[derive(Debug, Clone)]
struct Entry {
    usage_ratio: Option<f64>,
    shed_fraction: Option<f64>,
    /// Absolute deadline of a 429's advertised wait, in epoch ms.
    retry_after_until_ms: Option<i64>,
    binding_rule: Option<String>,
    refreshed_at_ms: i64,
}

fn scope_key(scope: Option<&BindingScope>) -> String {
    match scope {
        Some(s) => format!("{}={}", s.kind, s.value),
        None => PROJECT_WIDE.to_string(),
    }
}

/// The per-client store of what the server last said, and the decision taken from it.
///
/// One entry per binding scope: the project-wide view under `""`, a `name`-scoped view under
/// `name=<use-case>`, and so on. Nothing is evicted by count — the set of scopes a project's rules
/// can name is small and operator-authored.
#[derive(Debug)]
pub struct AdmissionCache {
    ttl_ms: i64,
    views: HashMap<String, Entry>,
}

impl Default for AdmissionCache {
    fn default() -> Self {
        AdmissionCache::new(DEFAULT_ADMISSION_TTL_MS)
    }
}

impl AdmissionCache {
    pub fn new(ttl_ms: i64) -> Self {
        AdmissionCache {
            ttl_ms,
            views: HashMap::new(),
        }
    }

    /// Fold one parsed ingest response into the cache.
    pub fn observe(&mut self, view: &LimitView, now_ms: i64) {
        let key = scope_key(view.binding_scope.as_ref());
        let prior = self.views.get(&key);
        // Only a 429 arms the wait. A 503 carries `Retry-After` too, but it means the *ingest
        // endpoint* is saturated — pausing the app's LLM calls over that would be the observability
        // tool causing the outage it exists to observe. And a 2xx is the server saying the refusal
        // is over, which outranks a schedule the client is still holding.
        let until = if view.accepted {
            None
        } else if view.rate_limited {
            match view.retry_after_secs {
                Some(s) => Some(now_ms + (s as i64) * 1000),
                None => prior.and_then(|p| p.retry_after_until_ms),
            }
        } else {
            prior.and_then(|p| p.retry_after_until_ms)
        };
        self.views.insert(
            key,
            Entry {
                usage_ratio: view.usage_ratio,
                shed_fraction: view.shed_fraction,
                retry_after_until_ms: until,
                binding_rule: view.binding_rule.clone(),
                refreshed_at_ms: now_ms,
            },
        );
    }

    /// Drop everything (a key rotation, a project switch — anything invalidating the evidence).
    pub fn clear(&mut self) {
        self.views.clear();
    }

    /// Decide one prospective call. Pure: no I/O, and no clock beyond `now_ms`.
    ///
    /// A `name` is answered from that use-case's own view when the server has named one, and from
    /// the project-wide view otherwise — applying the worst rule in the project to every call is how
    /// a scoped budget turns into a project-wide outage.
    pub fn admit(&self, name: Option<&str>, event_id: Option<&str>, now_ms: i64) -> Admit {
        let entry = name
            .and_then(|n| self.views.get(&format!("name={n}")))
            .or_else(|| self.views.get(PROJECT_WIDE));
        let Some(entry) = entry else {
            return Admit::admitted(false);
        };
        // The advertised wait is an absolute deadline, so it is honoured even past the TTL: the
        // server told us when to come back, and that instruction does not go stale, it expires.
        if let Some(until) = entry.retry_after_until_ms {
            if now_ms < until {
                let remaining = (until - now_ms) as f64 / 1000.0;
                return Admit {
                    ok: false,
                    reason: Some(AdmitReason::RetryAfter),
                    retry_after_secs: Some(remaining.ceil() as u64),
                    stale: false,
                };
            }
        }
        if now_ms - entry.refreshed_at_ms > self.ttl_ms {
            return Admit::admitted(true);
        }
        if entry.usage_ratio.is_some_and(|r| r >= 1.0) {
            return Admit {
                ok: false,
                reason: Some(AdmitReason::AtCap),
                retry_after_secs: None,
                stale: false,
            };
        }
        if let (Some(f), Some(id)) = (entry.shed_fraction, event_id) {
            let rule = entry.binding_rule.as_deref().unwrap_or("");
            if f > 0.0 && lighttrack_core::shed_ticket(rule, id) < f {
                return Admit {
                    ok: false,
                    reason: Some(AdmitReason::Shed),
                    retry_after_secs: None,
                    stale: false,
                };
            }
        }
        Admit::admitted(false)
    }
}

/// The refusal an enforcing gate returns instead of letting the provider call happen.
///
/// Typed, because the host app has to be able to tell "your budget said no" from a provider outage:
/// the first is a decision it may want to degrade around (a smaller model, a cached answer, a
/// queue), the second is a retry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BudgetExceeded {
    pub reason: Option<AdmitReason>,
    pub retry_after_secs: Option<u64>,
}

impl std::fmt::Display for BudgetExceeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "LightTrack refused this call before it was made ({})",
            self.reason.map(|r| r.as_str()).unwrap_or("unknown")
        )?;
        if let Some(s) = self.retry_after_secs {
            write!(f, "; retry in {s}s")?;
        }
        Ok(())
    }
}

impl std::error::Error for BudgetExceeded {}

/// Collapse `GET /v1/limits/status`'s `statuses` into one view, the same way the ingest doors do:
/// worst ratio, strongest shed, and the identity of the worst rule.
pub fn view_from_statuses(statuses: &serde_json::Value) -> Option<LimitView> {
    let arr = statuses.as_array()?;
    let mut worst: Option<&serde_json::Value> = None;
    let mut ratio: Option<f64> = None;
    let mut shed: Option<f64> = None;
    for s in arr {
        if let Some(r) = s["ratio"].as_f64() {
            if ratio.is_none_or(|best| r > best) {
                ratio = Some(r);
                worst = Some(s);
            }
        }
        if let Some(f) = s["shed_fraction"].as_f64() {
            if f > 0.0 && shed.is_none_or(|best| f > best) {
                shed = Some(f);
            }
        }
    }
    let w = worst?;
    // `LimitScope` is externally tagged on the wire (`{"model":"gpt-4o"}`), so the kind is the key.
    let binding_scope = w["scope"].as_object().and_then(|m| {
        m.iter().next().and_then(|(k, v)| {
            v.as_str().map(|value| BindingScope {
                kind: k.clone(),
                value: value.to_string(),
            })
        })
    });
    Some(LimitView {
        accepted: true,
        rate_limited: false,
        usage_ratio: ratio,
        shed_fraction: shed,
        retry_after_secs: None,
        error_code: None,
        binding_scope,
        binding_rule: w["rule_id"].as_str().map(str::to_string),
    })
}
