use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// The dimensions of one event that a [`LimitScope`] can be matched against. Passed as a struct
/// rather than a widening tuple of `&str`s so adding a dimension (as `api_key` and `customer` were)
/// doesn't silently re-order every call site's positional arguments.
#[derive(Debug, Clone, Copy, Default)]
pub struct ScopeDims<'a> {
    pub provider: &'a str,
    pub model: &'a str,
    /// Use-case label (`LlmEvent::name`).
    pub name: Option<&'a str>,
    /// The **id** of the API key that wrote the event (`metadata.api_key_id`, server-stamped).
    /// Never the key material or a hash of it — see the note on [`LimitScope::ApiKey`].
    pub api_key_id: Option<&'a str>,
    /// Billing customer (`metadata.customer_id`), the same linkage margin analytics group on.
    pub customer_id: Option<&'a str>,
}

impl<'a> ScopeDims<'a> {
    /// The three original dimensions, for callers with no key/customer context (tests, tools).
    pub fn new(provider: &'a str, model: &'a str, name: Option<&'a str>) -> Self {
        ScopeDims {
            provider,
            model,
            name,
            api_key_id: None,
            customer_id: None,
        }
    }
}

/// Optional dimension a limit is scoped to — provider / model / use-case (`name`) / API key /
/// billing customer. An unscoped rule (`None` on [`LimitRule::scope`]) applies to the whole project,
/// exactly as before; a scoped rule only counts (and can reject) traffic matching the selected
/// dimension value, so an operator can "cap gpt-4o at $5/day", "cap use-case X", or give a staging
/// key $5/day while production keeps $500 — without touching other traffic.
///
/// Serializes externally-tagged, e.g. `{"model":"gpt-4o"}` / `{"provider":"openai"}` /
/// `{"name":"summarize"}` / `{"api_key":"<key-id>"}` / `{"customer":"cus_123"}`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitScope {
    Provider(String),
    Model(String),
    /// Use-case, matched against an event's `name`.
    Name(String),
    /// One API key, identified by its **row id** — the opaque primary key of the `api_keys` table.
    ///
    /// Deliberately *not* the key material, and not a hash of it: the id is generated independently
    /// of the secret, so nothing about the key can be recovered from a rule, a status payload, or an
    /// alert. The non-secret `prefix` would also have worked, but the id is what the keys API already
    /// returns and what the event carries, so there is exactly one identifier to reason about.
    ApiKey(String),
    /// One billing customer, matched against `metadata.customer_id` — the same linkage margin
    /// analytics already group on.
    Customer(String),
}

impl LimitScope {
    /// The storage discriminant (`provider` | `model` | `name` | `api_key` | `customer`).
    pub fn kind_str(&self) -> &'static str {
        match self {
            LimitScope::Provider(_) => "provider",
            LimitScope::Model(_) => "model",
            LimitScope::Name(_) => "name",
            LimitScope::ApiKey(_) => "api_key",
            LimitScope::Customer(_) => "customer",
        }
    }

    /// The scoped value.
    pub fn value(&self) -> &str {
        match self {
            LimitScope::Provider(v)
            | LimitScope::Model(v)
            | LimitScope::Name(v)
            | LimitScope::ApiKey(v)
            | LimitScope::Customer(v) => v,
        }
    }

    /// Reconstruct from stored `(kind, value)` columns; `None` for an unknown kind.
    pub fn from_parts(kind: &str, value: String) -> Option<LimitScope> {
        match kind {
            "provider" => Some(LimitScope::Provider(value)),
            "model" => Some(LimitScope::Model(value)),
            "name" => Some(LimitScope::Name(value)),
            "api_key" => Some(LimitScope::ApiKey(value)),
            "customer" => Some(LimitScope::Customer(value)),
            _ => None,
        }
    }

    /// Every scope discriminant, for surfaces that enumerate the dimensions (e.g. the per-dimension
    /// usage breakdown). Order is the one they're presented in.
    pub const KINDS: &'static [&'static str] =
        &["provider", "model", "name", "api_key", "customer"];

    /// A compact `kind=value` label for alert messages / dedup keys / rendering.
    pub fn label(&self) -> String {
        format!("{}={}", self.kind_str(), self.value())
    }

    /// Whether an event with these dimensions falls under this scope. A dimension the event doesn't
    /// carry (`None`) never matches a scope on it — an untagged call can't be charged to a customer
    /// cap, exactly as an unnamed call can't be charged to a use-case cap.
    pub fn matches(&self, d: &ScopeDims<'_>) -> bool {
        match self {
            LimitScope::Provider(v) => d.provider == v,
            LimitScope::Model(v) => d.model == v,
            LimitScope::Name(v) => d.name == Some(v.as_str()),
            LimitScope::ApiKey(v) => d.api_key_id == Some(v.as_str()),
            LimitScope::Customer(v) => d.customer_id == Some(v.as_str()),
        }
    }
}

/// Whether a rule's optional scope admits an event with these dimensions. `None` (unscoped) always
/// matches — identical to pre-scope behavior.
pub fn scope_matches(scope: Option<&LimitScope>, dims: &ScopeDims<'_>) -> bool {
    scope.is_none_or(|s| s.matches(dims))
}

/// What a limit measures over its window.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitMetric {
    #[default]
    CostUsd,
    Calls,
    Tokens,
}

/// Rolling window a limit is evaluated over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitWindow {
    Hour,
    #[default]
    Day,
    Month,
}

impl LimitWindow {
    /// Every window, so a wire parser can derive its accepted set from the enum rather than
    /// hand-maintaining a parallel string list that drifts when a variant is added (the same
    /// authority shape as `Status::ALL`).
    pub const ALL: [LimitWindow; 3] = [LimitWindow::Hour, LimitWindow::Day, LimitWindow::Month];

    /// The wire/storage literal (`hour` | `day` | `month`) — what serde writes.
    pub fn as_str(&self) -> &'static str {
        match self {
            LimitWindow::Hour => "hour",
            LimitWindow::Day => "day",
            LimitWindow::Month => "month",
        }
    }

    /// Parse a wire literal back to a [`LimitWindow`], or `None` outside the vocabulary.
    pub fn from_wire(s: &str) -> Option<LimitWindow> {
        LimitWindow::ALL.into_iter().find(|w| w.as_str() == s)
    }

    /// How long a client should wait before retrying an ingest a **hard** cap turned away. Nothing
    /// frees capacity until usage ages out of the rolling window, so polling faster than this is
    /// pure waste; it is deliberately far shorter than the window itself, because usage leaves the
    /// window continuously rather than all at once. Advisory — the server does not enforce it.
    pub fn retry_after_secs(&self) -> u64 {
        match self {
            LimitWindow::Hour => 30,
            LimitWindow::Day => 300,
            LimitWindow::Month => 900,
        }
    }

    /// Rolling look-back duration for this window (Month is treated as 30 days for now).
    pub fn lookback(&self) -> Duration {
        match self {
            LimitWindow::Hour => Duration::hours(1),
            LimitWindow::Day => Duration::days(1),
            LimitWindow::Month => Duration::days(30),
        }
    }

    /// The start of the rolling window relative to `now`.
    pub fn since(&self, now: DateTime<Utc>) -> DateTime<Utc> {
        now - self.lookback()
    }
}

/// What happens as a limit is approached and breached. Three genuinely distinct tiers:
///
/// - **`Alert`** — observe-only. Notifies; never rejects anything.
/// - **`Throttle`** — *graduated*. Below [`LimitRule::throttle_start`] nothing happens. Between that
///   ratio and the threshold a proportionally growing share of ingest is shed (HTTP 429 with a short
///   `Retry-After`), so a client feels back-pressure and slows down *before* the wall instead of
///   going from fully accepted to fully rejected between two consecutive events. At and above the
///   threshold it is a hard stop, identical to `Block`.
/// - **`Block`** — an unambiguous hard stop at the threshold, with no shedding beforehand. A strict
///   cap stays strict.
///
/// Both enforcing tiers reject at ingest admission (the event is not recorded). Inline *pre-call*
/// blocking still requires the future gateway/proxy mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LimitAction {
    #[default]
    Alert,
    Throttle,
    Block,
}

impl LimitAction {
    /// Whether breaching a rule with this action rejects ingest (HTTP 429). `Alert` is
    /// observe-only (notify but never block); `Throttle` and `Block` both enforce, so a
    /// configured cap actually caps.
    pub fn enforces(self) -> bool {
        matches!(self, LimitAction::Throttle | LimitAction::Block)
    }

    /// Whether this action sheds traffic *before* the threshold. Only `Throttle` does — that is what
    /// makes it a different tier from `Block` rather than a synonym for it.
    pub fn sheds(self) -> bool {
        matches!(self, LimitAction::Throttle)
    }
}

/// Ratio at which a `Throttle` rule starts shedding when it sets no [`LimitRule::warn_at`]. Chosen
/// to coincide with the default "you're approaching the cap" intuition: the last fifth of the budget
/// is the ramp.
pub const DEFAULT_THROTTLE_START: f64 = 0.8;

/// Map `(rule, event)` to a stable point in `[0, 1)` — the throttle's shed lottery ticket.
///
/// Deliberately **not** a random draw. A given event always gets the same verdict from a given rule
/// at a given pressure, so behavior is reproducible, testable, and free of flapping: re-evaluating
/// the same event never changes its answer, and raising the shed fraction only ever *adds* events to
/// the shed set (it never un-sheds one, which is what makes the ramp monotone). FNV-1a rather than
/// `DefaultHasher` so the mapping is pinned to this code, not to a std implementation detail.
fn shed_ticket(rule_id: &str, event_id: &str) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in rule_id
        .as_bytes()
        .iter()
        .chain(b"\x1f")
        .chain(event_id.as_bytes())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x1000_0000_01b3);
    }
    // FNV mixes its low bits well but its high ones poorly on short inputs, and we want the *top*
    // 53. Finish with the SplitMix64 avalanche so the shed set is evenly spread across ids.
    h ^= h >> 30;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^= h >> 27;
    h = h.wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^= h >> 31;
    // Top 53 bits → the exactly-representable [0, 1) grid.
    (h >> 11) as f64 / (1u64 << 53) as f64
}

/// A per-project limit. Tripped by **monitored traffic only** — the scoring engine is exempt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitRule {
    pub id: String,
    pub project_id: String,
    pub metric: LimitMetric,
    pub window: LimitWindow,
    pub threshold: f64,
    pub action: LimitAction,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Optional soft-warning tier: a fraction of the threshold in `(0, 1)`. When rolling usage
    /// reaches `ratio >= warn_at` *without* breaching, a distinct `limit_warning` alert fires (its
    /// own cooldown) so the operator hears about an approaching cap before the 429. `None` = no
    /// pre-warning (old rules deserialize to this, unchanged). Never enforces.
    #[serde(default)]
    pub warn_at: Option<f64>,
    /// Optional dimension this rule caps (provider / model / use-case). `None` (serde-default) =
    /// project-wide, byte-identical to pre-scope behavior. A scoped rule only counts and rejects
    /// traffic matching its scope.
    #[serde(default)]
    pub scope: Option<LimitScope>,
}

fn default_true() -> bool {
    true
}

/// Provenance of the cost figure a [`LimitMetric::CostUsd`] rule was evaluated against — how much of
/// it is hard evidence and how much is inference.
///
/// **Why this exists.** An event whose model is absent from the price book stores `cost_usd = NULL`
/// (never a phantom zero — that invariant is load-bearing for margin honesty), and a `SUM` reads that
/// `NULL` as `0.00`. A cost cap therefore used to be free to walk past on exactly the newest,
/// least-vetted traffic. The limit path now *imputes* a cost for those calls from the window's own
/// priced traffic — the mean cost of a priced call in the same window, times the number of unpriced
/// calls — and reports the imputation here rather than hiding it inside `current`.
///
/// The degenerate case is [`CostEvidence::unpriceable`]: a window with unpriced calls and **no**
/// priced call has nothing to impute from, so the cap has no evidence at all. An enforcing rule
/// refuses ingest in that state (see [`LimitStatus::rejects_ingest`]) — a cap that cannot be measured
/// is not a cap.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CostEvidence {
    /// Calls in the window that carry a resolved `cost_usd`.
    pub priced_calls: i64,
    /// Calls in the window whose model was absent from the price book. They contribute `$0.00` of
    /// *stored* cost; `imputed_cost_usd` is what the limit path charged them instead.
    pub unpriced_calls: i64,
    /// Cost attributed to `unpriced_calls` by imputation. **Already included in
    /// [`LimitStatus::current`]** — subtract it to get the stored (hard-evidence) sum.
    pub imputed_cost_usd: f64,
    /// The part of the stored cost the *client* self-reported (`metadata.cost_source = "client"`)
    /// rather than our own price-book estimate. Not less valid, but not our arithmetic either.
    pub client_reported_cost_usd: f64,
    /// The window holds unpriced calls and no priced call to impute from: the cap is unevaluable.
    pub unpriceable: bool,
}

impl CostEvidence {
    /// Whether any part of `current` is inferred rather than stored.
    pub fn estimated(&self) -> bool {
        self.unpriced_calls > 0
    }
}

/// Result of evaluating a rule against a current rolling value.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LimitStatus {
    pub rule_id: String,
    pub project_id: String,
    pub metric: LimitMetric,
    pub window: LimitWindow,
    pub action: LimitAction,
    pub current: f64,
    pub threshold: f64,
    pub breached: bool,
    /// Fraction of the threshold used (1.0 == at limit). Useful for "approaching limit" warnings.
    pub ratio: f64,
    /// The rule's configured soft-warning fraction, echoed for the status surface (`None` = none).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_at: Option<f64>,
    /// In the soft-warning tier: at/over `warn_at` but not yet breached. Drives the "warning" badge
    /// on the status surface and the `limit_warning` alert. Always `false` when `warn_at` is unset.
    #[serde(default)]
    pub warning: bool,
    /// The rule's dimension scope, echoed for the status surface / alerts (`None` = project-wide).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<LimitScope>,
    /// For a `cost_usd` rule: how much of `current` is stored cost, how much is imputed for unpriced
    /// traffic, and how much was client-self-reported. `None` for `calls`/`tokens` rules (nothing to
    /// qualify) and for evaluations made without a usage snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_evidence: Option<CostEvidence>,
    /// Share of ingest this rule is currently shedding, in `[0, 1]`. Non-zero only for `Throttle`
    /// rules past their [`LimitRule::throttle_start`]; `1.0` once the threshold is reached (where
    /// throttling has become a hard stop). This is the proximity signal a well-behaved client backs
    /// off on — it is returned on *accepted* ingest responses too, not only on rejections.
    #[serde(default)]
    pub shed_fraction: f64,
    /// Set during admission when this rule shed *the specific event* being evaluated. Always `false`
    /// on the read-only status surface, which has no candidate event.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub shedding: bool,
}

impl LimitStatus {
    /// True when this status must reject ingest. Two conditions, both requiring an enforcing action
    /// (`Throttle`/`Block`):
    /// - the rule is **breached** — usage reached the threshold; or
    /// - the cost cap is **unpriceable** — the window's traffic cannot be priced at all, so the cap
    ///   cannot be measured (see [`CostEvidence::unpriceable`]).
    ///
    /// The ingest path returns HTTP 429 when any status reports this.
    pub fn rejects_ingest(&self) -> bool {
        self.action.enforces() && (self.breached || self.unpriceable())
    }

    /// Whether this status is a cost cap with no priceable evidence behind it.
    pub fn unpriceable(&self) -> bool {
        self.cost_evidence.as_ref().is_some_and(|e| e.unpriceable)
    }

    /// Whether any part of `current` is inferred (imputed for unpriced traffic).
    pub fn estimated(&self) -> bool {
        self.cost_evidence
            .as_ref()
            .is_some_and(CostEvidence::estimated)
    }

    /// Whether graduated throttling sheds the event identified by `event_id`.
    ///
    /// Only meaningful *before* the threshold — at or past it the rule is breached and
    /// [`LimitStatus::rejects_ingest`] is the hard stop. Deterministic (see [`shed_ticket`]): the
    /// same event always gets the same answer, and the shed set only grows as pressure rises.
    pub fn sheds(&self, event_id: &str) -> bool {
        !self.breached
            && self.shed_fraction > 0.0
            && shed_ticket(&self.rule_id, event_id) < self.shed_fraction
    }

    /// Seconds a client should wait after this status turned an event away. A hard stop waits for the
    /// window to age out ([`LimitWindow::retry_after_secs`]); a graduated shed is transient
    /// back-pressure, so it asks for a short pause that grows with the pressure (1–15s).
    pub fn retry_after_secs(&self) -> u64 {
        if self.breached {
            self.window.retry_after_secs()
        } else {
            1 + (14.0 * self.shed_fraction.clamp(0.0, 1.0)).ceil() as u64
        }
    }

    /// A compact scope tag for keys/labels: `all` when project-wide, else `kind=value`.
    pub fn scope_tag(&self) -> String {
        match &self.scope {
            None => "all".to_string(),
            Some(s) => s.label(),
        }
    }

    /// Stable per-rule key for alert-cooldown dedup and for matching a breach to its running
    /// rejection count. Includes the scope so a scoped cap and a project-wide cap on the same
    /// metric+window don't collide on one key.
    pub fn alert_key(&self) -> String {
        format!(
            "{}:{:?}:{:?}:{}",
            self.project_id,
            self.metric,
            self.window,
            self.scope_tag()
        )
    }
}

impl LimitRule {
    /// Validate a rule's numeric fields before it is created or updated. A `threshold` of `0`,
    /// negative, or non-finite (`NaN`/`inf`) is nonsensical — the old code silently accepted it and
    /// evaluated `ratio = ∞`, so the cap breached on *any* usage. Callers surface the `Err` as HTTP
    /// 400. Kept pure (and here, beside the type) so create and update share exactly one rule.
    pub fn validate(&self) -> Result<(), String> {
        if !(self.threshold.is_finite() && self.threshold > 0.0) {
            return Err(format!(
                "threshold must be a finite number greater than 0 (got {})",
                self.threshold
            ));
        }
        if let Some(w) = self.warn_at {
            if !(w.is_finite() && w > 0.0 && w < 1.0) {
                return Err(format!(
                    "warn_at must be a fraction strictly between 0 and 1 (got {w})"
                ));
            }
        }
        Ok(())
    }

    /// The usage ratio at which a `Throttle` rule begins shedding: its [`LimitRule::warn_at`] when
    /// set (the operator already told us where "approaching" starts — reusing it avoids a second
    /// knob that could contradict the first), else [`DEFAULT_THROTTLE_START`]. Meaningless for the
    /// other actions.
    pub fn throttle_start(&self) -> f64 {
        self.warn_at
            .filter(|w| w.is_finite() && *w > 0.0 && *w < 1.0)
            .unwrap_or(DEFAULT_THROTTLE_START)
    }

    /// Pure evaluation: given the project's current value for this rule's metric+window,
    /// decide whether the limit is breached. The caller computes `current` from the store.
    pub fn evaluate(&self, current: f64) -> LimitStatus {
        self.evaluate_with_evidence(current, None)
    }

    /// [`LimitRule::evaluate`] carrying the cost provenance of `current` (see [`CostEvidence`]). The
    /// store passes `Some(..)` for `cost_usd` rules so an operator — and the enforcement decision —
    /// can tell a cap breached on measured spend from one resting on imputation, and so a cap with no
    /// priceable evidence at all rejects instead of reading as a comfortable `$0.00`.
    pub fn evaluate_with_evidence(
        &self,
        current: f64,
        cost_evidence: Option<CostEvidence>,
    ) -> LimitStatus {
        let ratio = if self.threshold > 0.0 {
            current / self.threshold
        } else {
            f64::INFINITY
        };
        let breached = current >= self.threshold;
        // Warning tier: approaching the cap (ratio past warn_at) but not yet breached. A breached
        // rule is never "warning" — it has already crossed into enforcement/breach alerting.
        let warning = !breached && self.warn_at.is_some_and(|w| ratio >= w);
        // Graduated throttling: linear from `throttle_start` (0% shed) to the threshold (100%). At
        // the threshold and beyond the rule is breached and shedding is moot — reported as 1.0 so the
        // signal is continuous rather than snapping back to zero. `Block` and `Alert` never shed.
        let shed_fraction = if !self.action.sheds() {
            0.0
        } else if breached {
            1.0
        } else {
            let start = self.throttle_start();
            ((ratio - start) / (1.0 - start)).clamp(0.0, 1.0)
        };
        LimitStatus {
            rule_id: self.id.clone(),
            project_id: self.project_id.clone(),
            metric: self.metric,
            window: self.window,
            action: self.action,
            current,
            threshold: self.threshold,
            breached,
            ratio,
            warn_at: self.warn_at,
            warning,
            scope: self.scope.clone(),
            cost_evidence,
            shed_fraction,
            shedding: false, // set by the admission path, which knows the candidate event
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule() -> LimitRule {
        LimitRule {
            id: "r1".into(),
            project_id: "p1".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            threshold: 10.0,
            action: LimitAction::Alert,
            enabled: true,
            warn_at: None,
            scope: None,
        }
    }

    #[test]
    fn scope_matches_dimension() {
        let s = LimitScope::Model("gpt-4o".into());
        assert!(
            s.matches(&ScopeDims::new("openai", "gpt-4o", None)),
            "model matches"
        );
        assert!(
            !s.matches(&ScopeDims::new("openai", "gpt-4o-mini", None)),
            "other model does not"
        );
        let p = LimitScope::Provider("openai".into());
        assert!(p.matches(&ScopeDims::new("openai", "gpt-4o", Some("x"))));
        assert!(!p.matches(&ScopeDims::new("anthropic", "claude", None)));
        let n = LimitScope::Name("summarize".into());
        assert!(n.matches(&ScopeDims::new("openai", "gpt-4o", Some("summarize"))));
        assert!(
            !n.matches(&ScopeDims::new("openai", "gpt-4o", None)),
            "unnamed event doesn't match a name scope"
        );
        // Unscoped always matches.
        assert!(scope_matches(None, &ScopeDims::new("any", "any", None)));
    }

    #[test]
    fn key_and_customer_scopes_match_their_own_dimension_only() {
        let dims = ScopeDims {
            provider: "openai",
            model: "gpt-4o",
            name: Some("summarize"),
            api_key_id: Some("key-staging"),
            customer_id: Some("cus_1"),
        };
        assert!(LimitScope::ApiKey("key-staging".into()).matches(&dims));
        assert!(!LimitScope::ApiKey("key-prod".into()).matches(&dims));
        assert!(LimitScope::Customer("cus_1".into()).matches(&dims));
        assert!(!LimitScope::Customer("cus_2".into()).matches(&dims));
        // An event carrying neither dimension is never charged to a key/customer cap.
        let bare = ScopeDims::new("openai", "gpt-4o", None);
        assert!(!LimitScope::ApiKey("key-staging".into()).matches(&bare));
        assert!(!LimitScope::Customer("cus_1".into()).matches(&bare));
        // ...but the pre-existing dimensions are unaffected by the new ones.
        assert!(LimitScope::Model("gpt-4o".into()).matches(&bare));
    }

    #[test]
    fn new_scope_kinds_roundtrip_and_are_enumerated() {
        for (kind, ctor) in [
            ("api_key", LimitScope::ApiKey as fn(String) -> LimitScope),
            ("customer", LimitScope::Customer as fn(String) -> LimitScope),
        ] {
            let s = ctor("v".to_string());
            assert_eq!(s.kind_str(), kind);
            assert_eq!(LimitScope::from_parts(kind, "v".into()), Some(s.clone()));
            assert_eq!(s.label(), format!("{kind}=v"));
            assert!(LimitScope::KINDS.contains(&kind));
        }
        // JSON is externally tagged on the snake_case discriminant.
        let s: LimitScope = serde_json::from_str(r#"{"api_key":"k1"}"#).unwrap();
        assert_eq!(s, LimitScope::ApiKey("k1".into()));
        assert_eq!(serde_json::to_string(&s).unwrap(), r#"{"api_key":"k1"}"#);
    }

    #[test]
    fn scope_roundtrips_through_parts_and_key() {
        let s = LimitScope::Model("gpt-4o".into());
        assert_eq!(
            LimitScope::from_parts(s.kind_str(), s.value().to_string()),
            Some(s.clone())
        );
        assert_eq!(s.label(), "model=gpt-4o");
        let mut r = rule();
        r.scope = Some(s);
        let st = r.evaluate(5.0);
        assert_eq!(st.scope_tag(), "model=gpt-4o");
        assert!(st.alert_key().ends_with(":model=gpt-4o"));
        // Unscoped tag/key.
        assert_eq!(rule().evaluate(5.0).scope_tag(), "all");
    }

    #[test]
    fn warn_at_sets_warning_below_breach() {
        let mut r = rule();
        r.warn_at = Some(0.8);
        // Below warn_at: neither warning nor breached.
        let s = r.evaluate(7.0);
        assert!(!s.warning && !s.breached);
        // At/over warn_at, under threshold: warning, not breached.
        let s = r.evaluate(8.5);
        assert!(
            s.warning && !s.breached,
            "crossing warn_at warns without breaching"
        );
        // At threshold: breached, and warning is suppressed (already past the cap).
        let s = r.evaluate(10.0);
        assert!(s.breached && !s.warning);
    }

    #[test]
    fn validate_rejects_bad_warn_at() {
        let mut r = rule();
        r.warn_at = Some(1.0);
        assert!(r.validate().is_err(), "warn_at must be < 1");
        r.warn_at = Some(0.0);
        assert!(r.validate().is_err(), "warn_at must be > 0");
        r.warn_at = Some(f64::NAN);
        assert!(r.validate().is_err());
        r.warn_at = Some(0.8);
        assert!(r.validate().is_ok());
    }

    #[test]
    fn breaches_at_threshold() {
        assert!(rule().evaluate(10.0).breached);
        assert!(rule().evaluate(12.5).breached);
        assert!(!rule().evaluate(9.99).breached);
    }

    #[test]
    fn ratio_tracks_usage() {
        assert!((rule().evaluate(5.0).ratio - 0.5).abs() < 1e-9);
    }

    #[test]
    fn validate_rejects_nonpositive_or_nonfinite_threshold() {
        let mut r = rule();
        assert!(r.validate().is_ok());
        r.threshold = 0.0;
        assert!(r.validate().is_err(), "zero threshold is invalid");
        r.threshold = -1.0;
        assert!(r.validate().is_err(), "negative threshold is invalid");
        r.threshold = f64::INFINITY;
        assert!(r.validate().is_err(), "non-finite threshold is invalid");
        r.threshold = f64::NAN;
        assert!(r.validate().is_err(), "NaN threshold is invalid");
        r.threshold = 0.0001;
        assert!(r.validate().is_ok(), "small positive threshold is valid");
    }

    #[test]
    fn an_unpriceable_cost_cap_rejects_even_though_nothing_breached() {
        // The whole point of direction (1): a window whose traffic cannot be priced reads as
        // `$0.00` of spend. That must NOT look like headroom under an enforcing cap.
        let mut r = rule();
        r.action = LimitAction::Block;
        let ev = CostEvidence {
            priced_calls: 0,
            unpriced_calls: 3,
            imputed_cost_usd: 0.0,
            client_reported_cost_usd: 0.0,
            unpriceable: true,
        };
        let s = r.evaluate_with_evidence(0.0, Some(ev.clone()));
        assert!(
            !s.breached,
            "nothing was actually measured, so nothing breached"
        );
        assert!(
            s.unpriceable() && s.rejects_ingest(),
            "an unmeasurable cap must still refuse ingest"
        );
        // Alert-only rules are observe-only in every state, unpriceable included.
        r.action = LimitAction::Alert;
        assert!(!r.evaluate_with_evidence(0.0, Some(ev)).rejects_ingest());
    }

    #[test]
    fn evidence_marks_a_status_as_estimated() {
        let r = rule();
        let s = r.evaluate_with_evidence(
            6.0,
            Some(CostEvidence {
                priced_calls: 4,
                unpriced_calls: 2,
                imputed_cost_usd: 2.0,
                client_reported_cost_usd: 1.5,
                unpriceable: false,
            }),
        );
        assert!(
            s.estimated(),
            "a status carrying imputed cost is marked estimated"
        );
        assert!(!s.unpriceable());
        // A plain evaluate (calls/tokens rules, or evidence-free callers) carries none of this.
        assert!(!rule().evaluate(6.0).estimated());
        assert!(rule().evaluate(6.0).cost_evidence.is_none());
    }

    /// How many of `n` synthetic event ids a status sheds.
    fn shed_count(st: &LimitStatus, n: usize) -> usize {
        (0..n).filter(|i| st.sheds(&format!("ev-{i}"))).count()
    }

    #[test]
    fn throttle_ramps_where_block_is_a_cliff() {
        let mut t = rule();
        t.action = LimitAction::Throttle;
        let mut b = rule();
        b.action = LimitAction::Block;

        // Below the ramp start (0.8 of a threshold of 10) neither sheds anything.
        assert_eq!(t.evaluate(7.9).shed_fraction, 0.0);
        assert_eq!(shed_count(&t.evaluate(7.9), 400), 0);
        // Exactly AT the start is still zero — the boundary is deterministic, not a coin flip.
        assert_eq!(t.evaluate(8.0).shed_fraction, 0.0);
        assert_eq!(shed_count(&t.evaluate(8.0), 400), 0);

        // Halfway up the ramp (ratio 0.9) sheds about half; Block still sheds nothing at all.
        let mid = t.evaluate(9.0);
        assert!(
            (mid.shed_fraction - 0.5).abs() < 1e-9,
            "{}",
            mid.shed_fraction
        );
        let shed = shed_count(&mid, 400);
        assert!(
            (150..=250).contains(&shed),
            "proportional shedding, got {shed}/400"
        );
        assert_eq!(
            b.evaluate(9.0).shed_fraction,
            0.0,
            "Block never sheds before its threshold"
        );
        assert_eq!(shed_count(&b.evaluate(9.0), 400), 0);

        // At the threshold both are a hard stop; shedding is no longer the mechanism.
        assert!(t.evaluate(10.0).rejects_ingest() && b.evaluate(10.0).rejects_ingest());
        assert!(
            !t.evaluate(10.0).sheds("ev-1"),
            "a breached rule rejects outright, it doesn't shed"
        );
    }

    #[test]
    fn shedding_is_deterministic_and_monotone_so_it_cannot_flap() {
        let mut t = rule();
        t.action = LimitAction::Throttle;
        // Same event, same pressure, same answer — every time.
        let st = t.evaluate(9.0);
        let first = st.sheds("event-abc");
        for _ in 0..50 {
            assert_eq!(t.evaluate(9.0).sheds("event-abc"), first);
        }
        // Rising pressure only ever ADDS events to the shed set; nothing is ever un-shed. That is
        // what keeps traffic from oscillating as usage creeps up. (Walked up to — not past — the
        // threshold: at the threshold the rule stops shedding and becomes a hard stop instead.)
        let ids: Vec<String> = (0..500).map(|i| format!("e{i}")).collect();
        let mut previous: Vec<&String> = Vec::new();
        for step in 0..10 {
            let st = t.evaluate(8.0 + 0.2 * step as f64);
            let now: Vec<&String> = ids.iter().filter(|id| st.sheds(id)).collect();
            for id in &previous {
                assert!(now.contains(id), "event {id} was un-shed as pressure rose");
            }
            assert!(now.len() >= previous.len());
            previous = now;
        }
    }

    #[test]
    fn warn_at_doubles_as_the_throttle_ramp_start() {
        let mut t = rule();
        t.action = LimitAction::Throttle;
        t.warn_at = Some(0.5);
        assert_eq!(t.throttle_start(), 0.5);
        assert_eq!(t.evaluate(5.0).shed_fraction, 0.0, "ramp starts at warn_at");
        assert!((t.evaluate(7.5).shed_fraction - 0.5).abs() < 1e-9);
        // Unset warn_at falls back to the default ramp.
        t.warn_at = None;
        assert_eq!(t.throttle_start(), DEFAULT_THROTTLE_START);
    }

    #[test]
    fn retry_hint_separates_transient_back_pressure_from_a_hard_wall() {
        let mut t = rule();
        t.action = LimitAction::Throttle;
        // A shed is a short pause that grows with pressure.
        let light = t.evaluate(8.2).retry_after_secs();
        let heavy = t.evaluate(9.8).retry_after_secs();
        assert!((1..=15).contains(&light) && (1..=15).contains(&heavy));
        assert!(heavy > light, "harder shedding asks for a longer pause");
        // A breach waits for the window to age out — much longer, and window-dependent.
        assert_eq!(
            t.evaluate(10.0).retry_after_secs(),
            LimitWindow::Day.retry_after_secs()
        );
        let mut hourly = t.clone();
        hourly.window = LimitWindow::Hour;
        assert!(hourly.evaluate(10.0).retry_after_secs() < t.evaluate(10.0).retry_after_secs());
    }

    #[test]
    fn only_throttle_and_block_enforce() {
        assert!(!LimitAction::Alert.enforces());
        assert!(LimitAction::Throttle.enforces());
        assert!(LimitAction::Block.enforces());
    }

    #[test]
    fn rejects_ingest_requires_breach_and_enforcing_action() {
        let mut r = rule();
        // Breached + enforcing -> reject.
        r.action = LimitAction::Block;
        assert!(r.evaluate(10.0).rejects_ingest());
        r.action = LimitAction::Throttle;
        assert!(r.evaluate(10.0).rejects_ingest());
        // Breached but only Alert -> never rejects.
        r.action = LimitAction::Alert;
        assert!(!r.evaluate(10.0).rejects_ingest());
        // Not breached -> never rejects, even for Block.
        r.action = LimitAction::Block;
        assert!(!r.evaluate(9.99).rejects_ingest());
    }
}
