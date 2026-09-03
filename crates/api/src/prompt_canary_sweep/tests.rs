//! What the canary is allowed to decide, and what it must refuse to decide.
//!
//! The fixture is the shape the design names: a prompt with `production` on v1 and `canary` on v2,
//! real events tagged `name@v<version>` and real verdicts against them. The sweep runs with no
//! router built and no HTTP request made anywhere — the same property `forecast_sweep`'s tests pin.

use std::sync::Arc;

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{
    new_id, CanaryPolicy, LlmEvent, Operation, Prompt, PromptVersion, Score, ScoreKind, Status,
    TokenUsage, REASON_PROMOTE,
};
use lighttrack_store::{ScoreSummaryRow, SqliteStore, Store};

use super::*;
use crate::redact::Redactor;
use crate::tests_ingest::{make_key, setup};

const PROJECT: &str = "proj-a";
const PROMPT: &str = "support-reply";

fn policy(auto_revert: bool) -> CanaryPolicy {
    CanaryPolicy {
        label: "canary".into(),
        production_label: "production".into(),
        min_n: 5,
        window_secs: 3_600,
        max_drop: 0.05,
        auto_revert,
    }
}

/// A registered prompt with two versions, `production` → v1 and `canary` → v2, and a policy.
fn register(store: &SqliteStore, canary: Option<CanaryPolicy>) -> Prompt {
    let now = Utc::now();
    let mut p = Prompt {
        id: new_id(),
        project_id: PROJECT.into(),
        name: PROMPT.into(),
        benchmark_id: None,
        labels: Default::default(),
        canary,
        label_history: Vec::new(),
        created_at: now,
        updated_at: now,
    };
    // v1 promoted to both labels first, so the ledger names v1 as the canary's predecessor — which
    // is what an auto-revert falls back to.
    p.set_label("production", 1, REASON_PROMOTE);
    p.set_label("canary", 1, REASON_PROMOTE);
    p.set_label("canary", 2, REASON_PROMOTE);
    store.create_prompt(&p).unwrap();
    for v in [1u32, 2] {
        store
            .create_prompt_version(&PromptVersion {
                id: new_id(),
                prompt_id: p.id.clone(),
                version: v,
                content: format!("v{v}"),
                config: serde_json::Value::Null,
                note: None,
                created_at: now,
            })
            .unwrap();
    }
    p
}

/// `n` events tagged with `name@v<version>`, each carrying one verdict scored `value` out of 1.
fn traffic(store: &SqliteStore, version: u32, n: usize, value: f64) {
    for _ in 0..n {
        let ev = LlmEvent {
            id: new_id(),
            project_id: PROJECT.into(),
            trace_id: None,
            span_id: None,
            parent_span_id: None,
            ts: Utc::now(),
            received_at: Utc::now(),
            provider: "anthropic".into(),
            model: "claude-haiku-4-5".into(),
            name: None,
            operation: Operation::Chat,
            usage: TokenUsage {
                input: 100,
                output: 50,
                cached_input: None,
                reasoning: None,
            },
            cost_usd: Some(0.01),
            latency_ms: Some(10),
            status: Status::Success,
            error: None,
            input: None,
            output: None,
            tags: vec![],
            source: None,
            metadata: json!({ "prompt": CanaryPolicy::tag(PROMPT, version) }),
        };
        store.insert_event(&ev).unwrap();
        store
            .insert_score(&Score {
                id: new_id(),
                project_id: PROJECT.into(),
                event_id: Some(ev.id.clone()),
                rubric: "quality".into(),
                rubric_id: None,
                kind: ScoreKind::Rubric,
                value,
                max: 1.0,
                pass: Some(value >= 0.7),
                reasoning: None,
                detail: None,
                run_id: None,
                case_index: None,
                scored_by: "test".into(),
                cost_usd: Some(0.001),
                created_at: Utc::now(),
            })
            .unwrap();
    }
}

fn label(store: &SqliteStore, which: &str) -> Option<u32> {
    store
        .get_prompt(PROJECT, PROMPT)
        .unwrap()
        .and_then(|p| p.labels.get(which).copied())
}

#[tokio::test]
async fn a_regressed_canary_is_found_with_no_request_to_any_endpoint() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    register(&store, Some(policy(false)));
    // Production is comfortably good; the canary is comfortably bad. Tight clusters on both sides,
    // so the intervals genuinely do not overlap rather than merely differing in mean.
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 12, 0.50);

    let found = project_pass(&state, PROJECT)
        .await
        .ok()
        .expect("the pass runs");
    assert_eq!(found.len(), 1, "one regression: {found:?}");
    let r = &found[0];
    assert_eq!((r.canary_version, r.production_version), (2, 1));
    assert!(
        r.drop > 0.4,
        "the drop is reported, not just the verdict: {r:?}"
    );
    assert_eq!(r.canary_n, 12);
    assert_eq!(
        r.reverted_to, None,
        "auto_revert is off, so nothing was moved"
    );
    assert_eq!(
        label(&store, "canary"),
        Some(2),
        "…and the served label is untouched"
    );

    // The whole sweep, over every project, raises it too.
    assert!(sweep_once(&state).await > 0);
}

#[tokio::test]
async fn auto_revert_moves_the_label_back_to_the_version_the_ledger_names() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    register(&store, Some(policy(true)));
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 12, 0.50);

    let found = project_pass(&state, PROJECT)
        .await
        .ok()
        .expect("the pass runs");
    assert_eq!(found[0].reverted_to, Some(1));
    assert_eq!(
        label(&store, "canary"),
        Some(1),
        "the canary label is back on the version it replaced"
    );
    assert_eq!(
        label(&store, "production"),
        Some(1),
        "production was never touched"
    );

    let after = store.get_prompt(PROJECT, PROMPT).unwrap().unwrap();
    assert_eq!(
        after.label_history.last().unwrap().reason.as_deref(),
        Some(REASON_CANARY_REGRESSED),
        "the ledger records who moved it and why"
    );

    // With the labels equal again there is nothing left to compare, so the next pass is silent —
    // a reverted canary must not keep re-firing on the same traffic.
    assert!(project_pass(&state, PROJECT).await.ok().unwrap().is_empty());
}

#[tokio::test]
async fn a_prompt_with_no_policy_is_never_touched_however_bad_it_looks() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    register(&store, None);
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 12, 0.10);

    assert!(project_pass(&state, PROJECT).await.ok().unwrap().is_empty());
    assert_eq!(label(&store, "canary"), Some(2));
    assert_eq!(sweep_once(&state).await, 0);
}

/// The evidence floor. Two verdicts against two hundred is the case where a bare mean comparison
/// would confidently roll back a perfectly good version.
#[tokio::test]
async fn a_thin_canary_decides_nothing_and_moves_nothing() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    register(&store, Some(policy(true)));
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 2, 0.10);

    assert!(
        project_pass(&state, PROJECT).await.ok().unwrap().is_empty(),
        "below min_n on the canary side there is nothing to conclude"
    );
    assert_eq!(label(&store, "canary"), Some(2), "and nothing was reverted");
}

#[tokio::test]
async fn a_quiet_project_produces_nothing_and_a_disabled_one_is_skipped() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    assert_eq!(sweep_once(&state).await, 0);

    register(&store, Some(policy(true)));
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 12, 0.50);
    let mut p = store.get_project(PROJECT).unwrap().unwrap();
    p.enabled = false;
    store.update_project(&p).unwrap();
    assert_eq!(
        sweep_once(&state).await,
        0,
        "a disabled project's canary is not swept"
    );
    assert_eq!(label(&store, "canary"), Some(2), "…and not reverted either");
}

#[test]
fn the_sweep_is_off_unless_explicitly_configured() {
    // Env-driven, and this test process sets nothing: a canary that can move a served label is not
    // something a deployment acquires by upgrading.
    assert!(SweepConfig::from_env().is_none());
    assert!(describe(None).starts_with("off"));
    assert!(describe(Some(SweepConfig {
        interval: Duration::from_secs(300)
    }))
    .contains("every 300s"));
}

fn row(n: u64, mean: f64, half: f64) -> ScoreSummaryRow {
    ScoreSummaryRow {
        key: None,
        n,
        mean,
        pass_rate: mean,
        ci95_low: mean - half,
        ci95_high: mean + half,
        cost_usd: 0.0,
    }
}

/// The gate itself, at the boundaries. Both conditions are required, and each rejects a case the
/// other would let through.
#[test]
fn the_gate_needs_evidence_separation_and_a_drop_worth_acting_on() {
    let p = policy(false);
    let production = row(100, 0.90, 0.02);

    // Clean regression: enough verdicts, intervals apart, drop past the band.
    assert!(regression(&p, &row(20, 0.50, 0.03), &production).is_some());

    // Overlapping intervals — a real difference in means that the evidence cannot separate. This is
    // the case a bare mean comparison would roll back on.
    assert_eq!(regression(&p, &row(20, 0.70, 0.30), &production), None);

    // Separated but trivial: statistically real, 1% worse. Not worth moving what production serves.
    assert_eq!(regression(&p, &row(20, 0.89, 0.001), &production), None);

    // Below min_n on either side.
    assert_eq!(regression(&p, &row(4, 0.10, 0.01), &production), None);
    assert_eq!(
        regression(&p, &row(20, 0.10, 0.01), &row(4, 0.90, 0.01)),
        None
    );

    // Better, not worse: the canary above production never regresses, whatever the intervals do.
    assert_eq!(regression(&p, &row(20, 0.99, 0.001), &production), None);

    // A zero production mean has no relative band — dividing by it would make every canary
    // infinitely worse and roll back on the first tick.
    assert_eq!(regression(&p, &row(20, 0.0, 0.0), &row(20, 0.0, 0.0)), None);
}

/// `notify_prompt_canary` goes through the shared cooldown, so a sweep every few minutes cannot
/// turn one ongoing regression into a stream of notifications.
#[tokio::test]
async fn a_repeat_sweep_is_suppressed_by_the_shared_cooldown() {
    let (state, store) = setup(Redactor::off());
    make_key(&store, PROJECT);
    register(&store, Some(policy(false)));
    traffic(&store, 1, 12, 0.90);
    traffic(&store, 2, 12, 0.50);

    let found = project_pass(&state, PROJECT).await.ok().unwrap();
    let key = found[0].dedup_key();
    assert!(
        !key.contains("sweep"),
        "the key must not fork by trigger, or enabling the sweep would double the volume: {key}"
    );
    let alerts: &Arc<crate::alerts::Alerter> = &state.alerts;
    assert!(alerts.should_send_key(&key), "first presentation sends");
    assert!(
        !alerts.should_send_key(&key),
        "a repeat sweep inside the cooldown is suppressed: {key}"
    );
    assert!(
        alerts.should_send_key("prompt-canary:proj-a:other-prompt:canary"),
        "an unrelated prompt is unaffected"
    );
}
