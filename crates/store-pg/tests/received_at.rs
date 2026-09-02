//! Windowed accounting on Postgres keys on the server-stamped `received_at`, not the client's `ts`.
//! Env-gated exactly like the conformance suite (`LIGHTTRACK_TEST_DATABASE_URL`).
//!
//! Without this, a client with a skewed — or deliberately backdated — clock slides its spend outside
//! the window a cap is evaluated over and buys unmetered traffic. SQLite closed that; this pins the
//! Postgres port, which is the backend that carries production traffic.

use chrono::{Duration, Utc};
use lighttrack_core::{
    new_id, LimitAction, LimitMetric, LimitRule, LimitWindow, LlmEvent, Operation, Status,
    Threshold, TokenUsage,
};
use lighttrack_store::Store;
use lighttrack_store_pg::PgStore;

fn event(pid: &str) -> LlmEvent {
    let now = Utc::now();
    LlmEvent {
        id: new_id(),
        project_id: pid.into(),
        trace_id: None,
        span_id: None,
        parent_span_id: None,
        ts: now,
        received_at: now,
        provider: "anthropic".into(),
        model: "claude-haiku-4-5".into(),
        name: None,
        operation: Operation::Chat,
        usage: TokenUsage {
            input: 1,
            output: 1,
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(1.0),
        latency_ms: None,
        status: Status::Success,
        error: None,
        input: None,
        output: None,
        tags: vec![],
        source: Some("received-at-test".into()),
        metadata: serde_json::Value::Null,
    }
}

#[test]
fn windowed_accounting_ignores_a_backdated_client_clock() {
    let url = match std::env::var("LIGHTTRACK_TEST_DATABASE_URL") {
        Ok(u) if !u.is_empty() => u,
        _ => {
            eprintln!("skipping: set LIGHTTRACK_TEST_DATABASE_URL=postgres://… to run");
            return;
        }
    };
    let store = PgStore::connect(&url).expect("connect postgres");
    let pid = new_id();
    let now = Utc::now();

    // Backdated a month by the client, but it arrived now: it counts.
    let mut backdated = event(&pid);
    backdated.ts = now - Duration::days(30);
    store.insert_event(&backdated).expect("insert backdated");

    // Genuinely old traffic (arrived a month ago), replayed with a fresh `ts`: it does not.
    let mut old_arrival = event(&pid);
    old_arrival.received_at = now - Duration::days(30);
    store
        .insert_event(&old_arrival)
        .expect("insert old arrival");

    let u = store
        .usage_since(&pid, now - Duration::hours(1))
        .expect("usage");
    assert_eq!(u.calls, 1, "the window follows received_at, not ts");
    assert!((u.cost_usd - 1.0).abs() < 1e-9);
    assert_eq!(
        store
            .get_event(&backdated.id)
            .expect("get")
            .expect("present")
            .ts
            .timestamp(),
        backdated.ts.timestamp(),
        "the client's ts is preserved verbatim, only the accounting ignores it"
    );

    // And admission enforces on the same clock: with one call already in the window, a cap of 2 must
    // reject the next event however far back the client dates it.
    store
        .create_limit_rule(&LimitRule {
            id: new_id(),
            project_id: pid.clone(),
            metric: LimitMetric::Calls,
            window: LimitWindow::Day,
            threshold: Threshold::Fixed(2.0),
            action: LimitAction::Block,
            enabled: true,
            warn_at: None,
            scope: None,
            escalation: None,
            escalated_until: None,
            origin: None,
            expires_at: None,
        })
        .expect("rule");
    let mut sneaky = event(&pid);
    sneaky.ts = now - Duration::days(29);
    let out = store.insert_event_checked(&sneaky).expect("admission");
    assert!(
        !out.admitted,
        "a backdated `ts` cannot buy room under the cap"
    );
}
