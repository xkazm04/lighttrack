//! `metadata` is a TEXT column read as `jsonb`, and `''::jsonb` **raises** — so every extraction has
//! to map the empty string to NULL first. Env-gated exactly like the conformance suite
//! (`LIGHTTRACK_TEST_DATABASE_URL`).
//!
//! Nothing we write puts `''` there (the ingest path binds NULL or serde output), so this is the
//! defensive half of the contract: a hand-edited, imported or legacy row must skew nothing and fail
//! nothing. Before the guard reached `revenue.rs`, one such row made the *whole* margin/cost-by-
//! dimension read error out while the events path beside it kept answering.

use chrono::{Duration, Utc};
use lighttrack_core::{new_id, LlmEvent, Operation, Status, TokenUsage};
use lighttrack_store::Store;
use lighttrack_store_pg::PgStore;

fn event(pid: &str, metadata: serde_json::Value) -> LlmEvent {
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
        cost_usd: Some(2.0),
        latency_ms: None,
        status: Status::Success,
        error: None,
        input: None,
        output: None,
        tags: vec![],
        source: Some("metadata-guard-test".into()),
        metadata,
    }
}

/// Set one row's `metadata` to the empty string. The store's own API cannot produce this state, and
/// that is the point — the guard exists for rows it did not write.
fn blank_metadata(url: &str, event_id: &str) {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(async {
            let pool = sqlx::postgres::PgPool::connect(url).await.expect("connect");
            sqlx::query("UPDATE events SET metadata = '' WHERE id = $1")
                .bind(event_id)
                .execute(&pool)
                .await
                .expect("blank the metadata");
        });
}

#[test]
fn an_empty_metadata_string_does_not_break_the_margin_read() {
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

    let billed = event(&pid, serde_json::json!({ "customer_id": "acme" }));
    store.insert_event(&billed).expect("insert billed");
    let unbilled = event(&pid, serde_json::Value::Null);
    store.insert_event(&unbilled).expect("insert unbilled");

    // The row no writer of ours produces but any hand edit, import or older binary might. Written
    // with raw SQL for that reason — `insert_event` binds NULL or serde output, never `''`.
    let stray = event(&pid, serde_json::Value::Null);
    store.insert_event(&stray).expect("insert stray");
    blank_metadata(&url, &stray.id);

    let rows = store
        .cost_by_dimension(
            Some(&pid),
            "customer",
            now - Duration::hours(1),
            now + Duration::hours(1),
        )
        .expect("cost_by_dimension tolerates an unparseable metadata cell");

    let acme = rows
        .iter()
        .find(|r| r.key.as_deref() == Some("acme"))
        .expect("the billed customer is still attributed");
    assert_eq!(acme.calls, 1);
    assert!((acme.cost_usd - 2.0).abs() < 1e-9);
    // NULL metadata and blank metadata both land in the unattributed bucket rather than raising.
    let unattributed = rows
        .iter()
        .find(|r| r.key.is_none())
        .expect("unattributable spend is a bucket, not an error");
    assert_eq!(unattributed.calls, 2, "{rows:?}");
    assert!((unattributed.cost_usd - 4.0).abs() < 1e-9);
}
