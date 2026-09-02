//! End-to-end tests for the measure→act path: derived thresholds through the real store, and the
//! sweep's two guardrail passes against a real margin picture.
//!
//! Everything here goes through `SqliteStore` rather than fixtures, because the properties that
//! matter are exactly the ones a pure test cannot see: that the revenue-share threshold the
//! admission path enforces is resolved from rows in the database, and that a rule the sweep writes
//! is still there on the next read.

use chrono::{Duration, Utc};
use lighttrack_core::{
    new_id, Escalation, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, MarginPolicy,
    PolicyAction, PolicyTrigger, RevenueEvent, RevenueKind, Threshold, ThresholdDimension,
    ThresholdKind,
};
use lighttrack_store::Store;

use crate::limits::evaluate_project_limits;
use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

fn revenue(project: &str, customer: &str, amount: f64) -> RevenueEvent {
    RevenueEvent {
        id: new_id(),
        project_id: project.into(),
        source: "manual".into(),
        external_id: None,
        customer_id: Some(customer.into()),
        product_id: None,
        amount_usd: amount,
        currency: "USD".into(),
        kind: RevenueKind::OneTime,
        period_start: None,
        period_end: None,
        ts: Utc::now() - Duration::days(2),
        amount_minor: None,
        fx_rate: None,
        fx_book_version: None,
        converted: None,
    }
}

fn share_rule(project: &str, customer: &str, pct: f64) -> LimitRule {
    LimitRule {
        id: new_id(),
        project_id: project.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Month,
        threshold: Threshold::RevenueShare {
            pct,
            dimension: ThresholdDimension::Customer,
        },
        action: LimitAction::Block,
        enabled: true,
        warn_at: None,
        scope: Some(LimitScope::Customer(customer.into())),
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    }
}

/// The headline of M4: a cap that reads the invoice. The threshold on the status surface is not a
/// stored number — it is 80% of what this customer actually paid, resolved from `revenue_events`.
#[tokio::test]
async fn a_revenue_share_cap_resolves_against_stored_revenue_and_announces_its_basis() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    store
        .insert_revenue_event(&revenue("proj-a", "cus-a", 412.0))
        .unwrap();
    store
        .create_limit_rule(&share_rule("proj-a", "cus-a", 80.0))
        .unwrap();

    let statuses = evaluate_project_limits(&state, "proj-a")
        .await
        .ok()
        .expect("limits evaluate");
    let s = statuses.first().expect("the rule evaluates");
    assert!(
        (s.threshold - 329.6).abs() < 1e-6,
        "80% of $412.00, resolved at evaluation time — got {}",
        s.threshold
    );
    assert_eq!(s.basis.kind, ThresholdKind::RevenueShare);
    assert_eq!(s.basis.revenue_usd, Some(412.0));
    assert!(
        s.basis.describe().unwrap().contains("$412.00"),
        "the basis explains itself for the 429 message and the status page"
    );
}

/// The safety property that makes this feature deployable: a customer with no revenue on file is
/// *unmeasurable*, not free. Resolving them to `$0.00` would hard-stop them on their first call.
#[tokio::test]
async fn a_customer_with_no_revenue_yields_an_inert_cap_not_a_zero_one() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    // Revenue exists for a *different* customer, so the window is not simply empty.
    store
        .insert_revenue_event(&revenue("proj-a", "cus-other", 100.0))
        .unwrap();
    store
        .create_limit_rule(&share_rule("proj-a", "cus-new", 80.0))
        .unwrap();

    let statuses = evaluate_project_limits(&state, "proj-a")
        .await
        .ok()
        .expect("limits evaluate");
    let s = statuses.first().expect("the rule evaluates");
    assert!(s.threshold.is_infinite(), "an unmeasurable cap is inert");
    assert_eq!(s.basis.kind, ThresholdKind::Unknown);
    assert!(!s.breached, "and it never breaches");
    assert!(!s.rejects_ingest(), "so it can never turn ingest away");
}

/// A rule past its policy-set expiry stops counting, whether or not any sweep is running to reap it.
#[tokio::test]
async fn an_expired_guardrail_disappears_from_the_status_surface() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    let mut rule = share_rule("proj-a", "cus-a", 80.0);
    rule.threshold = Threshold::Fixed(5.0);
    rule.expires_at = Some(Utc::now() - Duration::hours(1));
    rule.origin = Some("margin_policy:pol-1:cus-a".into());
    store.create_limit_rule(&rule).unwrap();

    let statuses = evaluate_project_limits(&state, "proj-a")
        .await
        .ok()
        .expect("limits evaluate");
    assert!(
        statuses.is_empty(),
        "an expired guardrail must not be reported as a live cap: {statuses:?}"
    );
}

/// The escalation half, driven by a real forecast: ten days of rising spend under a $60/day cap
/// produce a `budget_breach` ETA, and a rule carrying an escalation tightens — then reverses when
/// the forecast calms down.
#[tokio::test]
async fn the_sweep_escalates_a_rule_on_a_breach_eta_and_reverses_when_calm() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    for d in 0..10 {
        store
            .insert_event(&crate::forecast_sweep::tests::event(
                "proj-a",
                9 - d,
                1.0 + d as f64 * 4.0,
            ))
            .unwrap();
    }
    let rule = LimitRule {
        id: new_id(),
        project_id: "proj-a".into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: Threshold::Fixed(60.0),
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
        escalation: Some(Escalation {
            on_eta_days: 30.0, // generous, so the fixture's ETA certainly reaches it
            to: LimitAction::Throttle,
            for_hours: 6,
        }),
        escalated_until: None,
        origin: None,
        expires_at: None,
    };
    store.create_limit_rule(&rule).unwrap();

    let (_, acted) = crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    assert_eq!(acted.escalated, 1, "the breach ETA escalated the rule");
    let after = store.get_limit_rule(&rule.id).unwrap().unwrap();
    assert!(after.escalated_until.is_some());
    assert_eq!(
        after.action,
        LimitAction::Alert,
        "the CONFIGURED action is never overwritten — that is what makes reversal a field clear"
    );
    assert_eq!(
        after.effective_action(),
        LimitAction::Throttle,
        "but the action in force is the escalated one"
    );

    // Idempotence: a second sweep with the same forecast must not re-stamp (which would make the
    // escalation immortal).
    let (_, again) = crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    assert_eq!(again.escalated, 0, "a standing escalation is left alone");

    // Now raise the bar out of reach: the forecast no longer clears it, so the rule de-escalates.
    let mut calm = store.get_limit_rule(&rule.id).unwrap().unwrap();
    calm.escalation = Some(Escalation {
        on_eta_days: 0.001,
        to: LimitAction::Throttle,
        for_hours: 6,
    });
    store.update_limit_rule(&calm).unwrap();
    let (_, reversed) = crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    assert_eq!(reversed.de_escalated, 1, "and it comes back down");
    assert!(store
        .get_limit_rule(&rule.id)
        .unwrap()
        .unwrap()
        .escalated_until
        .is_none());
}

/// The policy half: a loss-making customer gets a guardrail, the sweep is idempotent across ticks,
/// and the rule carries the origin that lets the reverse pass recognize it later.
#[tokio::test]
async fn a_margin_policy_raises_a_guardrail_for_a_loss_making_customer_exactly_once() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    // Cost with a customer attribution, no revenue: a free-tier sink.
    for d in 0..5 {
        let mut ev = crate::forecast_sweep::tests::event("proj-a", 4 - d, 20.0);
        ev.metadata = serde_json::json!({ "customer_id": "cus-sink" });
        store.insert_event(&ev).unwrap();
    }
    store
        .create_margin_policy(&MarginPolicy {
            id: new_id(),
            project_id: "proj-a".into(),
            trigger: PolicyTrigger::NegativeMargin,
            min_cost_usd: 10.0,
            action: PolicyAction::Warn,
            cooldown_secs: 0, // the cooldown is tested on its own; here we want every tick to act
            expiry_secs: 86_400,
            enabled: true,
        })
        .unwrap();

    let (_, acted) = crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    assert_eq!(acted.rules_created, 1, "the sink got a guardrail");
    let rules = store.list_limit_rules("proj-a", false).unwrap();
    let guard = rules
        .iter()
        .find(|r| r.origin.is_some())
        .expect("the guardrail carries an origin");
    assert_eq!(guard.scope, Some(LimitScope::Customer("cus-sink".into())));
    assert!(guard.expires_at.is_some(), "and it self-expires");

    // A second sweep over the same picture must change nothing — the whole point of an idempotent
    // engine is that a timer does not churn the rule table.
    let (_, again) = crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    assert_eq!(
        (
            again.rules_created,
            again.rules_updated,
            again.rules_removed
        ),
        (0, 0, 0),
        "an unchanged picture produces no writes"
    );
    assert_eq!(
        store.list_limit_rules("proj-a", false).unwrap().len(),
        rules.len()
    );
}

/// The line an operator must be able to trust: automation never touches a rule a human made.
#[tokio::test]
async fn the_sweep_never_edits_or_removes_a_hand_made_rule() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, "proj-a");
    for d in 0..5 {
        let mut ev = crate::forecast_sweep::tests::event("proj-a", 4 - d, 20.0);
        ev.metadata = serde_json::json!({ "customer_id": "cus-sink" });
        store.insert_event(&ev).unwrap();
    }
    // A hand-made cap on the very same customer the policy is about.
    let mut hand = share_rule("proj-a", "cus-sink", 80.0);
    hand.threshold = Threshold::Fixed(3.0);
    hand.action = LimitAction::Alert;
    store.create_limit_rule(&hand).unwrap();
    store
        .create_margin_policy(&MarginPolicy {
            id: new_id(),
            project_id: "proj-a".into(),
            trigger: PolicyTrigger::NegativeMargin,
            min_cost_usd: 10.0,
            action: PolicyAction::Block,
            cooldown_secs: 0,
            expiry_secs: 86_400,
            enabled: true,
        })
        .unwrap();

    crate::forecast_sweep::guardrail_pass(&state, "proj-a")
        .await
        .ok()
        .expect("the guardrail pass succeeds");
    let after = store
        .get_limit_rule(&hand.id)
        .unwrap()
        .expect("the hand-made rule still exists");
    assert_eq!(
        after.threshold,
        Threshold::Fixed(3.0),
        "untouched threshold"
    );
    assert_eq!(after.action, LimitAction::Alert, "untouched action");
    assert!(after.origin.is_none(), "and it never gets claimed");
}
