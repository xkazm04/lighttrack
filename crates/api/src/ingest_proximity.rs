//! The proximity signal every ingest door returns: how close this project is to a cap, which rule
//! is the binding one, and how long to wait if it already bit.
//!
//! `POST /v1/events` has carried `usage_ratio` / `shed_fraction` in its body since §7c. Two of the
//! three ingest doors cannot: `/v1/events/batch` answers multi-status (a per-item field is not the
//! project's position), and `/v1/traces` answers in the OTLP envelope, whose shape is not ours to
//! extend. A client that batches or exports OTLP therefore had no way to see the wall coming — the
//! signal existed only on the door it happened to be defined for.
//!
//! So the same three numbers ride in response *headers* on all three doors, and on the 429 as well.
//! Headers are the one channel every door shares, and an SDK reads them without knowing which shape
//! the body took.
//!
//! [`BindingScope`] is the other half. `usage_ratio: 0.94` is only actionable if you know *what* is
//! at 94%: a project-wide cap means stop, a cap scoped to `model=gpt-4o` means route elsewhere, and
//! a cap scoped to one use-case means only that call site should pause. Naming the binding rule's
//! scope is what lets a client cache its admission verdict per scope instead of applying the worst
//! rule in the project to every call it makes.

use axum::http::{HeaderMap, HeaderValue};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Serialize;

use lighttrack_core::LimitStatus;

pub(crate) const H_USAGE_RATIO: &str = "x-lighttrack-usage-ratio";
pub(crate) const H_SHED_FRACTION: &str = "x-lighttrack-shed-fraction";
pub(crate) const H_RETRY_AFTER: &str = "x-lighttrack-retry-after";

/// The dimension the binding rule applies to, flattened to `{kind, value}` so a client can compare
/// it without reproducing [`lighttrack_core::LimitScope`]'s externally-tagged encoding. Absent
/// (`None`) means the binding rule is project-wide.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct BindingScope {
    /// `provider` | `model` | `name` | `api_key` | `customer` — the rollup dimension's own name.
    pub(crate) kind: &'static str,
    pub(crate) value: String,
}

/// What an ingest response says about the project's position against its caps.
#[derive(Default, Clone, Debug)]
pub(crate) struct Proximity {
    /// Worst usage ratio among the rules that applied (`1.0` == at the cap).
    pub(crate) usage_ratio: Option<f64>,
    /// Strongest shedding pressure among them; `None` when nothing is throttling.
    pub(crate) shed_fraction: Option<f64>,
    /// Seconds to wait — set only when this response turned the write away.
    pub(crate) retry_after_secs: Option<u64>,
    /// Scope of the worst rule, so a name-scoped cap can be cached per name.
    pub(crate) binding_scope: Option<BindingScope>,
    /// Id of the worst rule. Carried because the shed decision is a hash of `(rule_id, event_id)`
    /// (§7c): without the rule's identity a client can run the same function but never the same
    /// decision, and "the SDK and the server agree which events shed" would be a claim it could
    /// not keep. Rule ids are already on the wire in `breached[]`; this only makes the *binding*
    /// one legible when nothing has breached yet.
    pub(crate) binding_rule: Option<String>,
}

impl Proximity {
    /// Read the proximity signal out of one admission's statuses.
    ///
    /// `usage_ratio` and `shed_fraction` stay aggregates (worst of each, possibly from different
    /// rules) — that is the pre-existing body contract and narrowing it would silently weaken the
    /// signal. The binding *identity* is the single worst-ratio rule, which is the one a client
    /// should act on.
    pub(crate) fn of(statuses: &[LimitStatus]) -> Proximity {
        let usage_ratio = statuses
            .iter()
            .map(|s| s.ratio)
            .fold(None::<f64>, |a, r| Some(a.map_or(r, |a| a.max(r))));
        let shed_fraction = statuses
            .iter()
            .map(|s| s.shed_fraction)
            .fold(None::<f64>, |a, r| Some(a.map_or(r, |a: f64| a.max(r))))
            .filter(|f| *f > 0.0);
        let binding = statuses
            .iter()
            .fold(None::<&LimitStatus>, |best, s| match best {
                Some(b) if b.ratio >= s.ratio => Some(b),
                _ => Some(s),
            });
        Proximity {
            usage_ratio,
            shed_fraction,
            retry_after_secs: None,
            binding_scope: binding.and_then(|b| {
                b.scope.as_ref().map(|sc| BindingScope {
                    kind: sc.kind_str(),
                    value: sc.value().to_string(),
                })
            }),
            binding_rule: binding.map(|b| b.rule_id.clone()),
        }
    }

    /// Fold another item's proximity into this one — the batch door's aggregate, where one request
    /// carries many admissions. Worst wins on every axis, including the wait: a client told to come
    /// back in 5s when one of its items needs 3600s would hammer a cap it cannot clear.
    pub(crate) fn merge(&mut self, other: &Proximity) {
        self.usage_ratio = max_opt(self.usage_ratio, other.usage_ratio);
        self.shed_fraction = max_opt(self.shed_fraction, other.shed_fraction);
        self.retry_after_secs = match (self.retry_after_secs, other.retry_after_secs) {
            (Some(a), Some(b)) => Some(a.max(b)),
            (a, b) => a.or(b),
        };
        // The binding identity follows the worst ratio, so it is replaced only when `other` is the
        // one that raised it.
        if other.usage_ratio.is_some() && other.usage_ratio >= self.usage_ratio {
            self.binding_scope = other.binding_scope.clone();
            self.binding_rule = other.binding_rule.clone();
        }
    }

    /// Whether anything at all is worth putting on the wire (a project with no limits has nothing).
    fn is_empty(&self) -> bool {
        self.usage_ratio.is_none()
            && self.shed_fraction.is_none()
            && self.retry_after_secs.is_none()
    }

    /// Stamp the three shared headers onto a response. Absent values emit no header — the same
    /// `null`-is-not-`0` rule the SDKs parse by.
    pub(crate) fn apply(&self, headers: &mut HeaderMap) {
        if self.is_empty() {
            return;
        }
        set(headers, H_USAGE_RATIO, self.usage_ratio.map(fmt_ratio));
        set(headers, H_SHED_FRACTION, self.shed_fraction.map(fmt_ratio));
        set(
            headers,
            H_RETRY_AFTER,
            self.retry_after_secs.map(|s| s.to_string()),
        );
    }
}

fn max_opt(a: Option<f64>, b: Option<f64>) -> Option<f64> {
    match (a, b) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

/// Six decimals: enough that a ratio moving by one event of a large window is still visible, short
/// enough that the value stays an exact, comparable decimal string rather than a float artifact.
fn fmt_ratio(v: f64) -> String {
    format!("{v:.6}")
}

fn set(headers: &mut HeaderMap, name: &'static str, value: Option<String>) {
    if let Some(v) = value {
        if let Ok(hv) = HeaderValue::from_str(&v) {
            headers.insert(name, hv);
        }
    }
}

/// A JSON body plus the proximity headers. Handlers return this instead of [`Json`] so the header
/// stamping happens in exactly one place and no door can forget it.
pub(crate) struct WithProximity<T> {
    pub(crate) body: T,
    pub(crate) proximity: Proximity,
}

impl<T> WithProximity<T> {
    pub(crate) fn new(body: T, proximity: Proximity) -> Self {
        WithProximity { body, proximity }
    }
}

impl<T: Serialize> IntoResponse for WithProximity<T> {
    fn into_response(self) -> Response {
        let mut resp = Json(self.body).into_response();
        self.proximity.apply(resp.headers_mut());
        resp
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::{LimitAction, LimitMetric, LimitScope, LimitWindow, ThresholdBasis};

    fn status(rule: &str, ratio: f64, shed: f64, scope: Option<LimitScope>) -> LimitStatus {
        LimitStatus {
            rule_id: rule.to_string(),
            project_id: "demo".into(),
            metric: LimitMetric::CostUsd,
            window: LimitWindow::Day,
            action: LimitAction::Block,
            current: ratio,
            threshold: 1.0,
            breached: ratio >= 1.0,
            ratio,
            warn_at: None,
            warning: false,
            scope,
            basis: ThresholdBasis::default(),
            cost_evidence: None,
            shed_fraction: shed,
            shedding: false,
        }
    }

    #[test]
    fn binding_identity_is_the_worst_rule_not_the_first() {
        let p = Proximity::of(&[
            status("r-cheap", 0.10, 0.0, None),
            status("r-hot", 0.94, 0.4, Some(LimitScope::Model("gpt-4o".into()))),
        ]);
        assert_eq!(p.usage_ratio, Some(0.94));
        assert_eq!(p.shed_fraction, Some(0.4));
        assert_eq!(p.binding_rule.as_deref(), Some("r-hot"));
        let scope = p.binding_scope.unwrap();
        assert_eq!(scope.kind, "model");
        assert_eq!(scope.value, "gpt-4o");
    }

    #[test]
    fn nothing_configured_emits_no_headers() {
        // The `null` vs `0` trap, on the header channel: a project with no limits must send no
        // ratio at all, or every client reads infinite headroom as "0% used".
        let mut h = HeaderMap::new();
        Proximity::of(&[]).apply(&mut h);
        assert!(h.is_empty());
    }

    #[test]
    fn merge_takes_the_worst_of_each_axis_and_the_longest_wait() {
        let mut a = Proximity::of(&[status("r-a", 0.2, 0.0, None)]);
        a.retry_after_secs = Some(5);
        let mut b = Proximity::of(&[status("r-b", 0.99, 0.9, None)]);
        b.retry_after_secs = Some(3600);
        a.merge(&b);
        assert_eq!(a.usage_ratio, Some(0.99));
        assert_eq!(a.shed_fraction, Some(0.9));
        assert_eq!(a.retry_after_secs, Some(3600));
        assert_eq!(a.binding_rule.as_deref(), Some("r-b"));
    }

    #[test]
    fn headers_are_exact_decimal_strings() {
        let mut h = HeaderMap::new();
        let mut p = Proximity::of(&[status("r", 0.82, 0.25, None)]);
        p.retry_after_secs = Some(30);
        p.apply(&mut h);
        assert_eq!(h[H_USAGE_RATIO], "0.820000");
        assert_eq!(h[H_SHED_FRACTION], "0.250000");
        assert_eq!(h[H_RETRY_AFTER], "30");
    }
}
