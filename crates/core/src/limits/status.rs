//! What a rule evaluates to: the status the ingest path enforces on and the status surface reports,
//! the cost provenance behind a `cost_usd` figure, and the deterministic shed lottery.

use serde::{Deserialize, Serialize};

use super::{LimitAction, LimitMetric, LimitScope, LimitWindow, ThresholdBasis, ThresholdKind};

/// Map `(rule, event)` to a stable point in `[0, 1)` — the throttle's shed lottery ticket.
///
/// Deliberately **not** a random draw. A given event always gets the same verdict from a given rule
/// at a given pressure, so behavior is reproducible, testable, and free of flapping: re-evaluating
/// the same event never changes its answer, and raising the shed fraction only ever *adds* events to
/// the shed set (it never un-sheds one, which is what makes the ramp monotone). FNV-1a rather than
/// `DefaultHasher` so the mapping is pinned to this code, not to a std implementation detail.
///
/// **Public because the SDKs need the server's own function, not a re-implementation of it.**
/// Pre-spend admission asks a client to decide, locally and before it spends, whether this event
/// would be shed — and "would be" is only true if it is the same arithmetic. The Rust client calls
/// straight through to here; the TypeScript and Python clients carry a port, held to the same values
/// by `clients/contract/fixtures/limits.json`'s `shed_lottery` list, which this crate's own runner
/// checks against this function.
pub fn shed_ticket(rule_id: &str, event_id: &str) -> f64 {
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
    /// Where `threshold` came from: a constant, a share of measured revenue, or a derived threshold
    /// whose basis could not be read at all. Estimation announcing itself — the alternative is a
    /// number on a status page with no story behind it.
    #[serde(default)]
    pub basis: ThresholdBasis,
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

    /// Whether this status' threshold is derived from revenue rather than typed by an operator.
    pub fn derived_threshold(&self) -> bool {
        self.basis.derived()
    }

    /// True when this rule is a derived cap that could not be resolved, so it is currently inert.
    pub fn inert(&self) -> bool {
        matches!(self.basis.kind, ThresholdKind::Unknown)
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
        // An unpriceable cap is not breached, but nothing about it changes on a retry either — an
        // operator has to add a price. It used to fall into the shed branch and advertise a 1s
        // pause, so a cooperating client hammered a refusal that could only ever answer the same.
        if self.breached || self.unpriceable() {
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
