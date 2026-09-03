use super::*;

fn rule() -> LimitRule {
    LimitRule {
        id: "r1".into(),
        project_id: "p1".into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: Threshold::Fixed(10.0),
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
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
fn validate_rejects_a_blank_scope_value() {
    let mut r = rule();
    r.scope = Some(LimitScope::Model("   ".into()));
    assert!(r.validate().is_err(), "a blank model scope caps nothing");
    r.scope = Some(LimitScope::ApiKey(String::new()));
    assert!(r.validate().is_err());
    r.scope = Some(LimitScope::Customer("cus_1".into()));
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
    r.threshold = Threshold::Fixed(0.0);
    assert!(r.validate().is_err(), "zero threshold is invalid");
    r.threshold = Threshold::Fixed(-1.0);
    assert!(r.validate().is_err(), "negative threshold is invalid");
    r.threshold = Threshold::Fixed(f64::INFINITY);
    assert!(r.validate().is_err(), "non-finite threshold is invalid");
    r.threshold = Threshold::Fixed(f64::NAN);
    assert!(r.validate().is_err(), "NaN threshold is invalid");
    r.threshold = Threshold::Fixed(0.0001);
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
    // ...and the retry hint is the window's, not the 1s shed pause: a retry changes nothing here.
    assert_eq!(s.retry_after_secs(), LimitWindow::Day.retry_after_secs());
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

/// Escalation shadows the configured action only while it is live, and expiry makes a rule inert
/// without deleting it — the two clocks the sweep relies on to be reversible.
#[test]
fn escalation_and_expiry_are_read_off_the_clock_not_written_into_the_rule() {
    let now = chrono::Utc::now();
    let mut r = rule();
    assert_eq!(r.effective_action_at(now), LimitAction::Alert);
    r.escalation = Some(Escalation {
        on_eta_days: 3.0,
        to: LimitAction::Block,
        for_hours: 24,
    });
    assert_eq!(
        r.effective_action_at(now),
        LimitAction::Alert,
        "an escalation the sweep has not stamped does nothing"
    );
    r.escalated_until = Some(now + chrono::Duration::hours(1));
    assert_eq!(r.effective_action_at(now), LimitAction::Block);
    assert_eq!(
        r.effective_action_at(now + chrono::Duration::hours(2)),
        LimitAction::Alert,
        "a lapsed escalation reverts by itself"
    );
    assert_eq!(
        r.action,
        LimitAction::Alert,
        "the configured action was never overwritten"
    );

    assert!(r.is_active_at(now));
    r.expires_at = Some(now + chrono::Duration::minutes(5));
    assert!(r.is_active_at(now));
    assert!(
        !r.is_active_at(now + chrono::Duration::minutes(6)),
        "past expiry the rule is inert"
    );
    r.enabled = false;
    assert!(!r.is_active_at(now));
}
