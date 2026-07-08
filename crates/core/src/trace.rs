//! Traces: roll a set of events that share a `trace_id` into one end-to-end view.
//!
//! Agentic / multi-step apps make many LLM calls per user request, each captured as its own
//! [`LlmEvent`] carrying `trace_id` / `span_id` / `parent_span_id`. Per-call events alone hide the
//! true cost and latency of the *request*. This module is the pure, I/O-free rollup: given a trace's
//! events it computes the [`TraceTotals`] (cost, tokens, errors) and arranges the spans into a tree
//! by their parent links. Stores fetch the events; this turns them into a [`Trace`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::event::LlmEvent;

/// Aggregate totals over every span in a trace.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceTotals {
    /// Number of events (spans) in the trace.
    pub spans: usize,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    /// Spans whose status is not `success` (errors + timeouts).
    pub errors: usize,
}

/// One node of a trace's span tree: an event and the spans whose parent it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpan {
    pub event: LlmEvent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TraceSpan>,
}

/// A compact per-trace rollup — the list view. No span payloads, so listing many traces stays cheap;
/// backends build these straight from a `GROUP BY trace_id` aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub project_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// Wall-clock span from the first to the last recorded event, in milliseconds.
    pub duration_ms: i64,
    pub spans: usize,
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
    pub errors: usize,
    /// `success` unless any span errored, then `error`.
    pub status: String,
    /// Distinct models touched in the trace.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

/// A full trace: the [`TraceTotals`] plus the span tree, for the detail view.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Trace {
    pub trace_id: String,
    pub project_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// Wall-clock span from the first to the last recorded event, in milliseconds.
    pub duration_ms: i64,
    /// `success` unless any span errored, then `error`.
    pub status: String,
    pub totals: TraceTotals,
    /// Distinct models touched, in first-seen (chronological) order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// Root spans (those with no parent within this trace), each carrying its subtree.
    pub spans: Vec<TraceSpan>,
}

impl Trace {
    /// Roll a trace's `events` into totals + a span tree. Returns `None` for an empty input.
    ///
    /// Identity (`trace_id`, `project_id`) and the time window are taken from the events themselves.
    /// Span nesting follows `parent_span_id` → `span_id`; an event whose parent is absent from the
    /// trace (or unset) is a root. Robust to malformed input: cycles and self-parents never drop or
    /// duplicate a span — every event appears exactly once.
    pub fn from_events(mut events: Vec<LlmEvent>) -> Option<Trace> {
        if events.is_empty() {
            return None;
        }
        // Oldest first: drives chronological child ordering and first-seen model order.
        events.sort_by(|a, b| a.ts.cmp(&b.ts));

        let trace_id = events
            .iter()
            .find_map(|e| e.trace_id.clone())
            .unwrap_or_default();
        let project_id = events[0].project_id.clone();
        let started_at = events.first().map(|e| e.ts).unwrap_or_else(Utc::now);
        let ended_at = events.iter().map(|e| e.ts).max().unwrap_or(started_at);
        let duration_ms = (ended_at - started_at).num_milliseconds().max(0);

        let totals = totals_of(&events);
        let models = distinct_models(&events);
        let status = if totals.errors > 0 { "error" } else { "success" }.to_string();
        let spans = build_forest(events);

        Some(Trace {
            trace_id,
            project_id,
            started_at,
            ended_at,
            duration_ms,
            status,
            totals,
            models,
            spans,
        })
    }

    /// The id of the event at the root of the trace (the entry-point span). Used to anchor a
    /// whole-trace score when the caller doesn't name a specific call. `None` only for an empty trace.
    pub fn root_event_id(&self) -> Option<&str> {
        self.spans.first().map(|s| s.event.id.as_str())
    }
}

fn totals_of(events: &[LlmEvent]) -> TraceTotals {
    let mut t = TraceTotals {
        spans: events.len(),
        ..Default::default()
    };
    for e in events {
        t.cost_usd += e.cost_usd.unwrap_or(0.0);
        t.input_tokens += e.usage.input;
        t.output_tokens += e.usage.output;
        if e.status != crate::event::Status::Success {
            t.errors += 1;
        }
    }
    t.total_tokens = t.input_tokens + t.output_tokens;
    t
}

/// Distinct model names in first-seen order (events are already sorted oldest-first).
fn distinct_models(events: &[LlmEvent]) -> Vec<String> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut out = Vec::new();
    for e in events {
        if seen.insert(e.model.as_str()) {
            out.push(e.model.clone());
        }
    }
    out
}

/// Arrange events (already sorted oldest-first) into a forest of [`TraceSpan`]s by parent links.
fn build_forest(events: Vec<LlmEvent>) -> Vec<TraceSpan> {
    // span_id -> index of the event that owns it (first occurrence wins on duplicates).
    let mut owner: HashMap<&str, usize> = HashMap::new();
    for (i, e) in events.iter().enumerate() {
        if let Some(sid) = e.span_id.as_deref() {
            owner.entry(sid).or_insert(i);
        }
    }

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        match e.parent_span_id.as_deref().and_then(|p| owner.get(p).copied()) {
            // A real parent that isn't the node itself: nest under it.
            Some(p) if p != i => children.entry(p).or_default().push(i),
            // No parent, dangling parent, or self-parent: a root span.
            _ => roots.push(i),
        }
    }

    let mut slots: Vec<Option<LlmEvent>> = events.into_iter().map(Some).collect();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut forest = Vec::with_capacity(roots.len());
    for r in roots {
        if let Some(node) = take_subtree(r, &mut slots, &children, &mut visited) {
            forest.push(node);
        }
    }
    // Any event not reachable from a root (a parent cycle) is promoted to a root so none is lost.
    for i in 0..slots.len() {
        if slots[i].is_some() {
            if let Some(node) = take_subtree(i, &mut slots, &children, &mut visited) {
                forest.push(node);
            }
        }
    }
    forest
}

fn take_subtree(
    idx: usize,
    slots: &mut [Option<LlmEvent>],
    children: &HashMap<usize, Vec<usize>>,
    visited: &mut HashSet<usize>,
) -> Option<TraceSpan> {
    if !visited.insert(idx) {
        return None; // cycle guard
    }
    let event = slots[idx].take()?;
    let kids = children.get(&idx).map(Vec::as_slice).unwrap_or(&[]);
    let mut child_nodes = Vec::with_capacity(kids.len());
    for &c in kids {
        if let Some(node) = take_subtree(c, slots, children, visited) {
            child_nodes.push(node);
        }
    }
    Some(TraceSpan { event, children: child_nodes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Provider, Status, TokenUsage};
    use chrono::Duration;
    use serde_json::Value;

    fn ev(span: &str, parent: Option<&str>, secs: i64, cost: f64, status: Status) -> LlmEvent {
        LlmEvent {
            id: format!("e-{span}"),
            project_id: "p1".into(),
            trace_id: Some("t1".into()),
            span_id: Some(span.into()),
            parent_span_id: parent.map(str::to_string),
            ts: Utc::now() + Duration::seconds(secs),
            provider: Provider::Anthropic,
            model: format!("m-{span}"),
            name: None,
            operation: Default::default(),
            usage: TokenUsage { input: 10, output: 5, cached_input: None, reasoning: None },
            cost_usd: Some(cost),
            latency_ms: Some(100),
            status,
            error: None,
            input: None,
            output: None,
            tags: vec![],
            source: None,
            metadata: Value::Null,
        }
    }

    #[test]
    fn empty_trace_is_none() {
        assert!(Trace::from_events(vec![]).is_none());
    }

    #[test]
    fn totals_sum_across_spans() {
        let evs = vec![
            ev("a", None, 0, 0.001, Status::Success),
            ev("b", Some("a"), 1, 0.002, Status::Success),
            ev("c", Some("a"), 2, 0.004, Status::Error),
        ];
        let t = Trace::from_events(evs).unwrap();
        assert_eq!(t.totals.spans, 3);
        assert!((t.totals.cost_usd - 0.007).abs() < 1e-9);
        assert_eq!(t.totals.input_tokens, 30);
        assert_eq!(t.totals.output_tokens, 15);
        assert_eq!(t.totals.total_tokens, 45);
        assert_eq!(t.totals.errors, 1);
        assert_eq!(t.status, "error", "any errored span flips the trace to error");
        assert_eq!(t.trace_id, "t1");
        assert_eq!(t.project_id, "p1");
    }

    #[test]
    fn builds_parent_child_tree() {
        // a -> {b -> d, c}
        let evs = vec![
            ev("a", None, 0, 0.0, Status::Success),
            ev("b", Some("a"), 1, 0.0, Status::Success),
            ev("c", Some("a"), 2, 0.0, Status::Success),
            ev("d", Some("b"), 3, 0.0, Status::Success),
        ];
        let t = Trace::from_events(evs).unwrap();
        assert_eq!(t.spans.len(), 1, "single root");
        let root = &t.spans[0];
        assert_eq!(root.event.span_id.as_deref(), Some("a"));
        assert_eq!(root.children.len(), 2, "b and c under a");
        // Children keep chronological order: b (t+1) before c (t+2).
        assert_eq!(root.children[0].event.span_id.as_deref(), Some("b"));
        assert_eq!(root.children[1].event.span_id.as_deref(), Some("c"));
        assert_eq!(root.children[0].children.len(), 1, "d nests under b");
        assert_eq!(root.children[0].children[0].event.span_id.as_deref(), Some("d"));
        assert_eq!(t.root_event_id(), Some("e-a"));
    }

    #[test]
    fn dangling_parent_becomes_root() {
        // b's parent "ghost" isn't in the trace -> b is a root alongside a.
        let evs = vec![
            ev("a", None, 0, 0.0, Status::Success),
            ev("b", Some("ghost"), 1, 0.0, Status::Success),
        ];
        let t = Trace::from_events(evs).unwrap();
        assert_eq!(t.spans.len(), 2, "dangling-parent span is treated as a root");
    }

    #[test]
    fn cycle_does_not_drop_or_loop() {
        // a <-> b mutual parents: neither is a natural root, but both must still appear once.
        let evs = vec![
            ev("a", Some("b"), 0, 0.0, Status::Success),
            ev("b", Some("a"), 1, 0.0, Status::Success),
        ];
        let t = Trace::from_events(evs).unwrap();
        let count = count_nodes(&t.spans);
        assert_eq!(count, 2, "every span surfaces exactly once despite the cycle");
    }

    #[test]
    fn distinct_models_in_first_seen_order() {
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.model = "first".into();
        let mut b = ev("b", Some("a"), 1, 0.0, Status::Success);
        b.model = "second".into();
        let mut c = ev("c", Some("a"), 2, 0.0, Status::Success);
        c.model = "first".into();
        let t = Trace::from_events(vec![c, a, b]).unwrap(); // unsorted input
        assert_eq!(t.models, vec!["first".to_string(), "second".to_string()]);
    }

    fn count_nodes(spans: &[TraceSpan]) -> usize {
        spans.iter().map(|s| 1 + count_nodes(&s.children)).sum()
    }
}
