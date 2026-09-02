//! `Surface::MarginPolicies` — the standing guardrails, and the rule fields the guardrail engine
//! writes onto a limit rule.
//!
//! Two halves, because they can fail independently:
//!
//! 1. **Policy CRUD.** A policy whose `trigger` or `action` does not round-trip fires on a different
//!    condition, or takes a different action, than the one the operator configured — a semantic
//!    inversion, not an absence, and exactly the class of bug the conformance suite exists for.
//! 2. **Rule provenance.** `threshold` (now a sum type), `escalation`, `escalated_until`, `origin`
//!    and `expires_at` must survive a create/get/update round-trip. A backend that drops `origin`
//!    makes every policy-created rule look hand-made — so the sweep's reverse pass stops recognizing
//!    its own work and guardrails accumulate forever. A backend that drops `threshold_json` turns
//!    "80% of revenue" into "$0.00", which hard-stops the customer.

use chrono::{Duration, Utc};

use lighttrack_core::{
    new_id, Escalation, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, MarginPolicy,
    PolicyAction, PolicyTrigger, Threshold, ThresholdDimension,
};

use crate::Scope;
use crate::{Result, Store, Surface};

pub(super) fn margin_policies(store: &dyn Store, pid: &str) -> Result<()> {
    policies(store, pid)?;
    // The rule-provenance half reads and updates rules after creation, which is `LimitLifecycle`'s
    // vocabulary. A backend that serves policies but not that surface refuses those calls by
    // contract, and the refusal walk already asserts it — so assert the round-trip only where the
    // methods exist, instead of manufacturing a failure the driver has already covered.
    if store.capabilities().has(Surface::LimitLifecycle) {
        guardrail_rule_fields(store, pid)?;
    }
    Ok(())
}

fn policies(store: &dyn Store, pid: &str) -> Result<()> {
    let p = MarginPolicy {
        id: new_id(),
        project_id: pid.into(),
        trigger: PolicyTrigger::ErosionEtaDays(4.5),
        min_cost_usd: 12.5,
        action: PolicyAction::CapToRevenue { factor: 0.8 },
        cooldown_secs: 1800,
        expiry_secs: 7200,
        enabled: true,
    };
    store.create_margin_policy(&p)?;

    let got = store
        .get_margin_policy(Scope::Operator, &p.id)?
        .expect("get_margin_policy finds the policy just created");
    assert_eq!(
        got.trigger,
        PolicyTrigger::ErosionEtaDays(4.5),
        "the trigger round-trips — a coerced trigger fires on a different condition entirely"
    );
    assert_eq!(
        got.action,
        PolicyAction::CapToRevenue { factor: 0.8 },
        "the action (and its factor) round-trips"
    );
    assert!(
        (got.min_cost_usd - 12.5).abs() < 1e-9,
        "min_cost_usd persists"
    );
    assert_eq!(got.cooldown_secs, 1800);
    assert_eq!(got.expiry_secs, 7200);
    assert!(got.enabled);

    // A disabled policy must still be listable — the admin surface shows it, only the sweep skips it.
    let disabled = MarginPolicy {
        id: new_id(),
        enabled: false,
        trigger: PolicyTrigger::NegativeMargin,
        action: PolicyAction::Warn,
        ..p.clone()
    };
    store.create_margin_policy(&disabled)?;
    let all = store.list_margin_policies(pid, false)?;
    assert!(
        all.iter().any(|x| x.id == p.id) && all.iter().any(|x| x.id == disabled.id),
        "listing without the enabled filter returns both"
    );
    let enabled = store.list_margin_policies(pid, true)?;
    assert!(
        enabled.iter().any(|x| x.id == p.id) && !enabled.iter().any(|x| x.id == disabled.id),
        "only_enabled excludes the disabled policy — otherwise the sweep would act on it"
    );

    assert!(
        store.delete_margin_policy(Scope::Operator, &p.id)?,
        "delete removes the row"
    );
    assert!(
        store.get_margin_policy(Scope::Operator, &p.id)?.is_none(),
        "and it is gone"
    );
    assert!(
        !store.delete_margin_policy(Scope::Operator, &new_id())?,
        "deleting an unknown id returns false (the API's 404)"
    );
    store.delete_margin_policy(Scope::Operator, &disabled.id)?;
    Ok(())
}

/// The rule-side half: everything the guardrail engine and the escalation pass write.
fn guardrail_rule_fields(store: &dyn Store, pid: &str) -> Result<()> {
    let until = Utc::now() + Duration::hours(6);
    let expires = Utc::now() + Duration::hours(12);
    let derived = LimitRule {
        id: new_id(),
        project_id: pid.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Month,
        threshold: Threshold::RevenueShare {
            pct: 80.0,
            dimension: ThresholdDimension::Customer,
        },
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: Some(LimitScope::Customer("conf-guard-cus".into())),
        escalation: Some(Escalation {
            on_eta_days: 2.0,
            to: LimitAction::Throttle,
            for_hours: 12,
        }),
        escalated_until: Some(until),
        origin: Some("margin_policy:conf-pol:conf-guard-cus".into()),
        expires_at: Some(expires),
    };
    store.create_limit_rule(&derived)?;
    let got = store
        .get_limit_rule(Scope::Operator, &derived.id)?
        .expect("the derived rule is readable");
    assert_eq!(
        got.threshold,
        Threshold::RevenueShare {
            pct: 80.0,
            dimension: ThresholdDimension::Customer
        },
        "a derived threshold round-trips — flattening it to a number would cap the customer at $0.00"
    );
    assert_eq!(
        got.escalation, derived.escalation,
        "the escalation clause round-trips"
    );
    assert_eq!(
        got.escalated_until.map(|t| t.timestamp()),
        Some(until.timestamp()),
        "the escalation deadline round-trips"
    );
    assert_eq!(
        got.origin.as_deref(),
        Some("margin_policy:conf-pol:conf-guard-cus"),
        "origin round-trips — without it the sweep cannot recognize its own rules to take them down"
    );
    assert_eq!(
        got.expires_at.map(|t| t.timestamp()),
        Some(expires.timestamp()),
        "the expiry round-trips"
    );

    // The reverse direction matters just as much: clearing an escalation must actually clear it, or
    // a de-escalated project stays throttled.
    let mut calm = got.clone();
    calm.escalated_until = None;
    calm.origin = None;
    calm.threshold = Threshold::Fixed(42.0);
    assert!(store.update_limit_rule(Scope::Operator, &calm)?);
    let after = store
        .get_limit_rule(Scope::Operator, &derived.id)?
        .expect("still present after de-escalation");
    assert!(
        after.escalated_until.is_none(),
        "clearing escalated_until persists — a stale deadline keeps a project throttled"
    );
    assert!(after.origin.is_none(), "clearing origin persists");
    assert_eq!(
        after.threshold,
        Threshold::Fixed(42.0),
        "a derived threshold can be replaced by a fixed one"
    );

    // And a plain rule must still round-trip as `Fixed` — the byte-identical old behaviour.
    let plain = LimitRule {
        id: new_id(),
        threshold: Threshold::Fixed(9.5),
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
        ..derived.clone()
    };
    store.create_limit_rule(&plain)?;
    let got_plain = store
        .get_limit_rule(Scope::Operator, &plain.id)?
        .expect("plain rule reads");
    assert_eq!(got_plain.threshold, Threshold::Fixed(9.5));
    assert!(got_plain.escalation.is_none() && got_plain.expires_at.is_none());

    store.delete_limit_rule(Scope::Operator, &derived.id)?;
    store.delete_limit_rule(Scope::Operator, &plain.id)?;
    Ok(())
}
