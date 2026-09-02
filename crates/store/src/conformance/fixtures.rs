//! Event builders shared by every section, so each one asserts against the same shape.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{
    new_id, Alert, AlertChannel, AlertKind, ChannelKind, CollectiveEntry, Coverage, LimitAction,
    LimitMetric, LimitRule, LimitWindow, LlmEvent, MarginPolicy, Operation, PolicyAction,
    PolicyTrigger, Project, Redaction, Severity, Status, Threshold, TokenUsage,
};

pub(super) fn sample_event(pid: &str, model: &str, inp: u64, out: u64, cost: f64) -> LlmEvent {
    LlmEvent {
        id: new_id(),
        project_id: pid.into(),
        trace_id: Some("trace".into()),
        span_id: None,
        parent_span_id: None,
        ts: Utc::now(),
        received_at: Utc::now(),
        provider: "anthropic".into(),
        model: model.into(),
        name: None,
        operation: Operation::Chat,
        usage: TokenUsage {
            input: inp,
            output: out,
            cached_input: None,
            reasoning: None,
        },
        cost_usd: Some(cost),
        latency_ms: Some(42),
        status: Status::Success,
        error: None,
        input: Some(json!({ "q": "hi" })),
        output: Some(json!({ "a": "yo" })),
        tags: vec!["conf".into()],
        source: Some("conformance".into()),
        metadata: json!({ "k": "v" }),
    }
}

/// A monitored event attributed to a billing `customer` (the linkage `cost_by_dimension` groups on,
/// read from `metadata.customer_id`).
pub(super) fn tagged_event(pid: &str, customer: &str, cost: f64) -> LlmEvent {
    let mut ev = sample_event(pid, "claude-haiku-4-5", 10, 5, cost);
    ev.metadata = json!({ "customer_id": customer });
    ev
}

pub(super) fn sample_project() -> Project {
    Project {
        id: new_id(),
        name: "refusal-probe".into(),
        enabled: true,
        redaction: Redaction::None,
        collective_opt_in: false,
        require_trusted_judge: false,
        archived_at: None,
        created_at: Utc::now(),
    }
}

pub(super) fn sample_rule() -> LimitRule {
    LimitRule {
        id: new_id(),
        project_id: new_id(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: Threshold::Fixed(1.0),
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

pub(super) fn sample_policy() -> MarginPolicy {
    MarginPolicy {
        id: new_id(),
        project_id: new_id(),
        trigger: PolicyTrigger::NegativeMargin,
        min_cost_usd: 1.0,
        action: PolicyAction::Warn,
        cooldown_secs: 3600,
        expiry_secs: 86_400,
        enabled: true,
    }
}

pub(super) fn sample_entry() -> CollectiveEntry {
    CollectiveEntry {
        contributor_id: new_id(),
        provider: "anthropic".into(),
        model: "claude-haiku-4-5".into(),
        task_type: "qa".into(),
        quality: 0.9,
        pass_rate: 0.8,
        avg_cost_usd: 0.003,
        p50_latency_ms: Some(900),
        p95_latency_ms: Some(2000),
        n_runs: 2,
        n_cases: 12,
        quality_variance: Some(0.01),
        judge_provider: Some("openai".into()),
        rubric_fingerprint: Some("fp-1".into()),
        determinism: None,
        frozen_dataset: Coverage::Unknown,
        significance_tested: Coverage::Unknown,
        received_at: Utc::now(),
    }
}

/// One fired alert for `pid`, keyed on `dedup_key`. Shared by the ledger section and the refusal
/// walk so both assert against exactly the row the API would write.
pub(super) fn sample_alert(pid: &str, kind: AlertKind, dedup_key: &str) -> Alert {
    Alert::new(
        kind,
        Some(pid.to_string()),
        dedup_key.to_string(),
        json!({ "text": "conformance", "n": 1 }),
    )
}

/// One routing destination. `project` is `None` for a global channel — the shape the env-configured
/// destinations have always had.
pub(super) fn sample_alert_channel(project: Option<&str>) -> AlertChannel {
    AlertChannel {
        id: new_id(),
        project_id: project.map(str::to_string),
        kind: ChannelKind::Webhook,
        target: "https://receiver.invalid/hook".into(),
        // A plausible derived signing key: 32 bytes of sha256, hex.
        secret_hash: Some("0".repeat(64)),
        prev_secret_hash: None,
        min_severity: Severity::Warning,
        kinds: vec![AlertKind::LimitBreach],
        enabled: true,
        created_at: Utc::now(),
    }
}
