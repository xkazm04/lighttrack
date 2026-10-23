use super::*;

fn cost(provider: &str, model: &str, c: f64) -> CostRow {
    CostRow {
        project_id: "p".into(),
        provider: provider.into(),
        model: model.into(),
        calls: 1,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: c,
        unpriced_calls: 0,
    }
}

fn uc(name: Option<&str>, provider: &str, model: &str, c: f64) -> UseCaseCostRow {
    UseCaseCostRow {
        name: name.map(|s| s.to_string()),
        provider: provider.into(),
        model: model.into(),
        calls: 1,
        input_tokens: 0,
        output_tokens: 0,
        cost_usd: c,
        unpriced_calls: 0,
    }
}

#[test]
fn unscoped_ranks_top_three_models_with_shares_and_usecase_annotation() {
    let costs = vec![
        cost("openai", "gpt-4o", 6.0),
        cost("anthropic", "claude-sonnet", 3.0),
        cost("openai", "gpt-4o-mini", 1.0),
        cost("openai", "tiny", 0.0), // zero spend dropped
    ];
    let ucs = vec![
        uc(Some("summarize"), "openai", "gpt-4o", 5.0),
        uc(Some("classify"), "openai", "gpt-4o", 1.0),
        uc(None, "anthropic", "claude-sonnet", 3.0),
    ];
    let a = compose(&costs, &ucs, None);
    assert_eq!(a.scope_note, None);
    assert_eq!(a.contributors.len(), 3);
    // total = 10 → shares 60/30/10; gpt-4o annotated with its dominant named use-case.
    assert_eq!(a.contributors[0].label, "gpt-4o (summarize)");
    assert!((a.contributors[0].share_pct - 60.0).abs() < 1e-9);
    assert_eq!(a.contributors[1].label, "claude-sonnet");
    assert!((a.contributors[1].share_pct - 30.0).abs() < 1e-9);
    assert_eq!(a.contributors[2].label, "gpt-4o-mini");
}

#[test]
fn provider_scope_does_not_annotate_from_another_provider() {
    let costs = vec![cost("openai", "shared-model", 10.0)];
    let ucs = vec![
        uc(Some("openai-task"), "openai", "shared-model", 1.0),
        uc(Some("foreign-task"), "openrouter", "shared-model", 99.0),
    ];

    let a = compose(&costs, &ucs, Some(&LimitScope::Provider("openai".into())));

    assert_eq!(a.contributors[0].label, "shared-model (openai-task)");
}

#[test]
fn dominant_usecase_is_the_aggregate_across_matching_providers() {
    let costs = vec![
        cost("openai", "shared-model", 10.0),
        cost("openrouter", "shared-model", 10.0),
    ];
    let ucs = vec![
        uc(Some("aggregate-winner"), "openai", "shared-model", 4.0),
        uc(Some("aggregate-winner"), "openrouter", "shared-model", 4.0),
        uc(Some("single-winner"), "openai", "shared-model", 6.0),
    ];

    let a = compose(&costs, &ucs, None);

    assert_eq!(a.contributors[0].label, "shared-model (aggregate-winner)");
}

#[test]
fn model_scope_attributes_within_the_model_by_usecase() {
    let ucs = vec![
        uc(Some("summarize"), "openai", "gpt-4o", 7.0),
        uc(Some("chat"), "openai", "gpt-4o", 3.0),
        uc(Some("other"), "openai", "gpt-4o-mini", 99.0), // different model → excluded
    ];
    let a = compose(&[], &ucs, Some(&LimitScope::Model("gpt-4o".into())));
    assert_eq!(a.scope_note.as_deref(), Some("within scope model=gpt-4o"));
    assert_eq!(a.contributors.len(), 2);
    assert_eq!(a.contributors[0].label, "summarize");
    assert!((a.contributors[0].share_pct - 70.0).abs() < 1e-9); // within-scope total = 10
}

/// The admission path treats a dated release as the capped model (`LimitScope::matches`), so the
/// alert's attribution must too — or the traffic that tripped the cap is the traffic the alert
/// says does not exist.
#[test]
fn model_scope_folds_in_the_dated_variants_the_cap_itself_caught() {
    let ucs = vec![
        uc(Some("summarize"), "openai", "gpt-4o-2024-08-06", 7.0),
        uc(Some("chat"), "openai", "gpt-4o", 3.0),
        uc(Some("other"), "openai", "gpt-4o-mini", 99.0), // a different family → excluded
    ];
    let a = compose(&[], &ucs, Some(&LimitScope::Model("gpt-4o".into())));
    assert_eq!(a.contributors.len(), 2, "{a:?}");
    assert_eq!(a.contributors[0].label, "summarize");
    assert!((a.contributors[0].share_pct - 70.0).abs() < 1e-9);
}

#[test]
fn name_scope_attributes_within_the_usecase_by_model() {
    let ucs = vec![
        uc(Some("summarize"), "openai", "gpt-4o", 8.0),
        uc(Some("summarize"), "anthropic", "claude-sonnet", 2.0),
        uc(Some("classify"), "openai", "gpt-4o", 50.0), // different use-case → excluded
    ];
    let a = compose(&[], &ucs, Some(&LimitScope::Name("summarize".into())));
    assert_eq!(a.scope_note.as_deref(), Some("within scope name=summarize"));
    assert_eq!(a.contributors[0].label, "gpt-4o");
    assert!((a.contributors[0].share_pct - 80.0).abs() < 1e-9);
}

#[test]
fn empty_rollups_degrade_silently() {
    let a = compose(&[], &[], None);
    assert!(a.is_empty());
    assert!(a.message_tail().is_none());
    // A scoped breach with no spend still states the scope rather than going blank.
    let scoped = compose(&[], &[], Some(&LimitScope::Provider("openai".into())));
    assert!(!scoped.is_empty());
    assert!(scoped.contributors.is_empty());
    assert!(scoped.message_tail().unwrap().contains("no attributable"));
}

#[test]
fn message_tail_lists_contributors() {
    let costs = vec![cost("openai", "gpt-4o", 3.0), cost("openai", "mini", 1.0)];
    let tail = compose(&costs, &[], None).message_tail().unwrap();
    assert!(tail.contains("gpt-4o 75%"), "got: {tail}");
    assert!(tail.contains("mini 25%"), "got: {tail}");
}
