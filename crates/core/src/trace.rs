//! Traces: roll a set of events that share a `trace_id` into one end-to-end view.
//!
//! Agentic / multi-step apps make many LLM calls per user request, each captured as its own
//! [`LlmEvent`] carrying `trace_id` / `span_id` / `parent_span_id`. Per-call events alone hide the
//! true cost and latency of the *request*. This module is the pure, I/O-free rollup: given a trace's
//! events it computes the [`TraceTotals`] (cost, tokens, errors) and arranges the spans into a tree
//! by their parent links. Stores fetch the events; this turns them into a [`Trace`].

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

use crate::event::LlmEvent;

/// Canonicalize a `trace_id` / `span_id` / `parent_span_id` so both ingest doors agree on identity.
///
/// A W3C/OTel id is hex and **case-insensitive** — `5B8E…` and `5b8e…` are the same trace — but the
/// OTLP door lower-cased its ids while the SDK door normalized nothing, so one end-to-end trace
/// spanning an OTel service and an SDK service silently rendered as two. Hex ids of the W3C lengths
/// (32 for a trace, 16 for a span) are therefore lower-cased.
///
/// Anything else is a caller's own opaque id (`"req-1"`, `"Order-7"`) and is preserved **verbatim**:
/// case is meaningful there, and folding it would merge distinct traces and mangle an id the operator
/// reads back.
pub fn normalize_trace_ref(id: &str) -> String {
    let is_w3c_hex = matches!(id.len(), 16 | 32) && id.chars().all(|c| c.is_ascii_hexdigit());
    if is_w3c_hex {
        id.to_ascii_lowercase()
    } else {
        id.to_string()
    }
}

/// The single definition of the two numbers the list and the detail view must agree on: a trace's
/// wall-clock duration and its status.
///
/// The list rollup builds this from a SQL aggregate, the detail rollup by folding the events; both
/// then read `duration_ms()` / `status()` from here. Keeping the *rule* in one place is what stops the
/// two views drifting the way they did when the list reported `MAX(ts) - MIN(ts)` (start-to-start) and
/// the detail reported `max(ts + latency) - started_at`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TraceShape {
    /// The first span's start.
    pub started_at: DateTime<Utc>,
    /// The last span's **finish** — `max(ts + latency)`, not `max(ts)`, so a trailing call's compute
    /// time is counted.
    pub last_finish: DateTime<Utc>,
    /// Spans whose status is not `success`.
    pub errors: usize,
}

impl TraceShape {
    /// Fold a trace's events (any order) into its shape. `None` for an empty input.
    pub fn of_events(events: &[LlmEvent]) -> Option<TraceShape> {
        let started_at = events.iter().map(|e| e.ts).min()?;
        let last_finish = events
            .iter()
            .map(|e| e.ts + Duration::milliseconds(e.latency_ms.unwrap_or(0) as i64))
            .max()
            .unwrap_or(started_at);
        let errors = events
            .iter()
            .filter(|e| e.status != crate::event::Status::Success)
            .count();
        Some(TraceShape {
            started_at,
            last_finish,
            errors,
        })
    }

    /// Wall-clock milliseconds from the trace's start to its last finish.
    ///
    /// Both endpoints are truncated to whole milliseconds *before* subtracting: the SQL side can only
    /// offer millisecond precision (it adds an integer `latency_ms` to an epoch-ms timestamp), so
    /// truncating here too makes the two paths produce the identical integer rather than one that
    /// happens to be off by one on sub-millisecond timestamps.
    pub fn duration_ms(&self) -> i64 {
        (self.last_finish.timestamp_millis() - self.started_at.timestamp_millis()).max(0)
    }

    /// `success` unless any span errored, then `error`.
    pub fn status(&self) -> String {
        if self.errors > 0 { "error" } else { "success" }.to_string()
    }
}

/// What a whole-trace verdict actually judged, recorded on the verdict itself.
///
/// A trace has no end marker: a span that lands after the judge ran is folded straight into the next
/// read (new totals, a new node in the tree) while the verdict stays put and silently stops
/// describing the trace. This is the receipt that makes that detectable — the trace's size and the
/// fingerprint of the exchange the judge actually read, so a later read can compare and say so.
///
/// The digest covers the **root exchange** (the root span's id + input + output) rather than every
/// span, deliberately:
/// - it is what a whole-trace judge is handed, so it changes exactly when the judged text changes;
/// - it survives the [`Trace::spans_truncated`] cap — the detail read keeps the *oldest* spans, so
///   the root is always present and a clipped trace fingerprints identically to the whole one. A
///   truncated trace must never be mistaken for a changed one.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TraceCoverage {
    /// The trace's true span count when judged — `spans_total`, so a clipped read still records the
    /// real number.
    #[serde(default)]
    pub spans: usize,
    /// The event whose exchange was judged (the trace's entry-point span).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub root_event_id: String,
    /// Fingerprint of that exchange. Changes iff the text the judge saw changed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub digest: String,
    /// The read this verdict was formed over was clipped by the span cap — provenance only, never a
    /// drift signal.
    #[serde(default, skip_serializing_if = "is_false")]
    pub truncated: bool,
}

/// How far a trace has moved from the verdict that covers it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TraceDrift {
    /// The trace is exactly what was judged.
    None,
    /// Same judged exchange, but the trace has more (or fewer) spans than the verdict covers. The
    /// verdict is stale to a reader; re-judging would re-send byte-identical text, so it does not
    /// justify spending on a fresh judge call.
    Grown,
    /// The judged exchange itself differs — the verdict describes text that is no longer there. This
    /// is the only drift that earns a re-score.
    Changed,
}

impl TraceCoverage {
    /// Compare the coverage a verdict recorded against the trace as it now reads.
    pub fn drift(&self, current: &TraceCoverage) -> TraceDrift {
        // An unknown digest (a verdict written before coverage existed, or by a third party) can only
        // be compared on size — never claim a content change we cannot see.
        if !self.digest.is_empty() && self.digest != current.digest {
            return TraceDrift::Changed;
        }
        if self.spans != current.spans {
            return TraceDrift::Grown;
        }
        TraceDrift::None
    }
}

impl TraceDrift {
    /// The wire word for this drift, or `None` when the verdict still covers the trace.
    pub fn reason(self) -> Option<&'static str> {
        match self {
            TraceDrift::None => None,
            TraceDrift::Grown => Some("grown"),
            TraceDrift::Changed => Some("changed"),
        }
    }
}

/// FNV-1a 64-bit, hex. Small, dependency-free and stable across processes and releases — a digest
/// that changed with the toolchain would re-score every trace once, which costs real money.
fn fingerprint(parts: &[&str]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for b in bytes {
            h ^= *b as u64;
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
    };
    for (i, p) in parts.iter().enumerate() {
        if i > 0 {
            mix(&[0x1f]); // unit separator: "ab"+"c" must not fingerprint as "a"+"bc"
        }
        mix(p.as_bytes());
    }
    format!("{h:016x}")
}

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
    /// Summed per-span latency — the trace's total *compute* time, distinct from the wall-clock
    /// `duration_ms` (which spans idle gaps but counts overlapping work once).
    #[serde(default)]
    pub total_latency_ms: u64,
}

/// One node of a trace's span tree: an event and the spans whose parent it is.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSpan {
    pub event: LlmEvent,
    /// Milliseconds from the trace's start to this span's start — its offset on the waterfall.
    #[serde(default)]
    pub offset_ms: i64,
    /// This span's own latency (compute time), mirrored from the event so a consumer can place the
    /// bar `[offset_ms, offset_ms + latency_ms]` without digging into `event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u64>,
    /// True when an earlier span in the trace already claimed this `span_id`. Two events reported the
    /// same id, so they are genuinely distinct calls that would otherwise render as two identical-
    /// looking bullets; only the first owns the id for parent linkage.
    #[serde(default, skip_serializing_if = "is_false")]
    pub duplicate_span_id: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<TraceSpan>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// A compact per-trace rollup — the list view. No span payloads, so listing many traces stays cheap;
/// backends build these straight from a `GROUP BY trace_id` aggregate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceSummary {
    pub trace_id: String,
    pub project_id: String,
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    /// Wall-clock milliseconds from the first span's start to the last span's *finish* — the same
    /// [`TraceShape`] definition the detail view reports, so list and detail cannot disagree. (It was
    /// `MAX(ts) - MIN(ts)`, start-to-start, which under-reported whenever the last span had latency.)
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
    /// Wall-clock milliseconds from the first span's start to the last span's *finish*
    /// (`max(ts + latency)`), so a trailing call's latency is counted. May exceed
    /// `ended_at - started_at` (which is start-to-last-start) by that final span's compute time.
    /// Defined once in [`TraceShape`] — the list rollup reports the identical number.
    pub duration_ms: i64,
    /// `success` unless any span errored, then `error`.
    pub status: String,
    pub totals: TraceTotals,
    /// Distinct models touched, in first-seen (chronological) order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
    /// How many spans the trace has in total — including any dropped by the fetch cap.
    #[serde(default)]
    pub spans_total: usize,
    /// How many spans this payload actually carries (equals `totals.spans`).
    #[serde(default)]
    pub spans_logged: usize,
    /// True when the fetch cap clipped the trace. Every derived number here — `totals`, `models`,
    /// `duration_ms`, `status` — then describes the retained spans only, so a clipped trace must
    /// never be read as a complete one.
    #[serde(default)]
    pub spans_truncated: bool,
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
    pub fn from_events(events: Vec<LlmEvent>) -> Option<Trace> {
        let total = events.len();
        Trace::from_events_bounded(events, total)
    }

    /// As [`Trace::from_events`], but told how many spans the trace really has (`spans_total`) when
    /// the caller fetched a bounded window of them. Everything derived — totals, models, duration,
    /// status — then covers the retained spans only, and `spans_truncated` says so.
    pub fn from_events_bounded(mut events: Vec<LlmEvent>, spans_total: usize) -> Option<Trace> {
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
        let ended_at = events.iter().map(|e| e.ts).max().unwrap_or(events[0].ts);
        // Duration and status come from the shared TraceShape, the same rule the list rollup applies.
        let shape = TraceShape::of_events(&events)?;

        let totals = totals_of(&events);
        let models = distinct_models(&events);
        let spans_logged = events.len();
        let spans = build_forest(events, shape.started_at);

        Some(Trace {
            trace_id,
            project_id,
            started_at: shape.started_at,
            ended_at,
            duration_ms: shape.duration_ms(),
            status: shape.status(),
            totals,
            models,
            spans_total: spans_total.max(spans_logged),
            spans_logged,
            spans_truncated: spans_total > spans_logged,
            spans,
        })
    }

    /// The id of the event at the root of the trace (the entry-point span). Used to anchor a
    /// whole-trace score when the caller doesn't name a specific call. `None` only for an empty trace.
    pub fn root_event_id(&self) -> Option<&str> {
        self.spans.first().map(|s| s.event.id.as_str())
    }

    /// The [`TraceCoverage`] a verdict judged over this trace would record — and, on a later read,
    /// what a stored verdict's coverage is compared against.
    pub fn coverage(&self) -> TraceCoverage {
        let root = self.spans.first().map(|s| &s.event);
        let root_event_id = root.map(|e| e.id.clone()).unwrap_or_default();
        let payload = |v: Option<&serde_json::Value>| match v {
            Some(v) if !v.is_null() => v.to_string(),
            _ => String::new(),
        };
        let digest = fingerprint(&[
            &root_event_id,
            &payload(root.and_then(|e| e.input.as_ref())),
            &payload(root.and_then(|e| e.output.as_ref())),
        ]);
        TraceCoverage {
            spans: self.spans_total,
            root_event_id,
            digest,
            truncated: self.spans_truncated,
        }
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
        t.total_latency_ms += e.latency_ms.unwrap_or(0);
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
/// `trace_start` anchors each span's `offset_ms` on the waterfall.
fn build_forest(events: Vec<LlmEvent>, trace_start: DateTime<Utc>) -> Vec<TraceSpan> {
    // span_id -> index of the event that owns it (first occurrence wins on duplicates). A later event
    // reusing the id is flagged: it still renders as its own node (it IS a distinct call), but says so
    // rather than reading as a second copy of the same span.
    let mut owner: HashMap<&str, usize> = HashMap::new();
    let mut duplicate = vec![false; events.len()];
    for (i, e) in events.iter().enumerate() {
        if let Some(sid) = e.span_id.as_deref() {
            match owner.entry(sid) {
                std::collections::hash_map::Entry::Occupied(_) => duplicate[i] = true,
                std::collections::hash_map::Entry::Vacant(v) => {
                    v.insert(i);
                }
            }
        }
    }

    let mut children: HashMap<usize, Vec<usize>> = HashMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, e) in events.iter().enumerate() {
        match e
            .parent_span_id
            .as_deref()
            .and_then(|p| owner.get(p).copied())
        {
            // A real parent that isn't the node itself: nest under it.
            Some(p) if p != i => children.entry(p).or_default().push(i),
            // No parent, dangling parent, or self-parent: a root span.
            _ => roots.push(i),
        }
    }

    let mut slots: Vec<Option<LlmEvent>> = events.into_iter().map(Some).collect();
    let mut visited: HashSet<usize> = HashSet::new();
    let mut forest = Vec::with_capacity(roots.len());
    let ctx = ForestCtx {
        children: &children,
        duplicate: &duplicate,
        trace_start,
    };
    for r in roots {
        if let Some(node) = take_subtree(r, &mut slots, &mut visited, &ctx) {
            forest.push(node);
        }
    }
    // Any event not reachable from a root (a parent cycle) is promoted to a root so none is lost.
    for i in 0..slots.len() {
        if slots[i].is_some() {
            if let Some(node) = take_subtree(i, &mut slots, &mut visited, &ctx) {
                forest.push(node);
            }
        }
    }
    forest
}

/// The read-only side tables `take_subtree` needs, bundled so the recursion keeps a short signature.
struct ForestCtx<'a> {
    children: &'a HashMap<usize, Vec<usize>>,
    duplicate: &'a [bool],
    trace_start: DateTime<Utc>,
}

fn take_subtree(
    idx: usize,
    slots: &mut [Option<LlmEvent>],
    visited: &mut HashSet<usize>,
    ctx: &ForestCtx<'_>,
) -> Option<TraceSpan> {
    if !visited.insert(idx) {
        return None; // cycle guard
    }
    let event = slots[idx].take()?;
    let offset_ms = (event.ts - ctx.trace_start).num_milliseconds().max(0);
    let latency_ms = event.latency_ms;
    let kids = ctx.children.get(&idx).map(Vec::as_slice).unwrap_or(&[]);
    let mut child_nodes = Vec::with_capacity(kids.len());
    for &c in kids {
        if let Some(node) = take_subtree(c, slots, visited, ctx) {
            child_nodes.push(node);
        }
    }
    Some(TraceSpan {
        event,
        offset_ms,
        latency_ms,
        duplicate_span_id: ctx.duplicate.get(idx).copied().unwrap_or(false),
        children: child_nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Provider, Status, TokenUsage};
    use chrono::Duration;
    use serde_json::Value;

    /// A fixed base instant: offsets are exact whole seconds, so the millisecond-truncated duration
    /// rule (see `TraceShape::duration_ms`) is deterministic instead of riding `Utc::now()`'s
    /// sub-millisecond drift between calls.
    fn base() -> DateTime<Utc> {
        chrono::TimeZone::with_ymd_and_hms(&Utc, 2026, 6, 21, 12, 0, 0).unwrap()
    }

    fn ev(span: &str, parent: Option<&str>, secs: i64, cost: f64, status: Status) -> LlmEvent {
        LlmEvent {
            id: format!("e-{span}"),
            project_id: "p1".into(),
            trace_id: Some("t1".into()),
            span_id: Some(span.into()),
            parent_span_id: parent.map(str::to_string),
            ts: base() + Duration::seconds(secs),
            received_at: base(),
            provider: Provider::Anthropic,
            model: format!("m-{span}"),
            name: None,
            operation: Default::default(),
            usage: TokenUsage {
                input: 10,
                output: 5,
                cached_input: None,
                reasoning: None,
            },
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
        assert_eq!(
            t.status, "error",
            "any errored span flips the trace to error"
        );
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
        assert_eq!(
            root.children[0].children[0].event.span_id.as_deref(),
            Some("d")
        );
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
        assert_eq!(
            t.spans.len(),
            2,
            "dangling-parent span is treated as a root"
        );
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
        assert_eq!(
            count, 2,
            "every span surfaces exactly once despite the cycle"
        );
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

    #[test]
    fn honest_duration_counts_final_span_latency() {
        // Two spans 2s apart, each 100ms latency. Wall-by-starts is 2000ms; honest end is the last
        // span's start (+2000ms) plus its 100ms latency = 2100ms.
        let evs = vec![
            ev("a", None, 0, 0.0, Status::Success),
            ev("b", Some("a"), 2, 0.0, Status::Success),
        ];
        let t = Trace::from_events(evs).unwrap();
        assert_eq!(
            t.duration_ms, 2100,
            "final span's latency is no longer dropped"
        );
        assert_eq!(
            t.totals.total_latency_ms, 200,
            "compute time sums per-span latency"
        );
    }

    #[test]
    fn bounded_fetch_reports_the_truncation() {
        // Three spans exist; the caller was only handed the first two.
        let evs = vec![
            ev("a", None, 0, 0.001, Status::Success),
            ev("b", Some("a"), 1, 0.002, Status::Success),
        ];
        let t = Trace::from_events_bounded(evs, 3).unwrap();
        assert!(t.spans_truncated, "a clipped trace must say so");
        assert_eq!(t.spans_total, 3);
        assert_eq!(t.spans_logged, 2);
        assert_eq!(
            t.totals.spans, 2,
            "derived numbers cover the retained spans only"
        );

        // The untruncated case carries the same three fields, saying "complete".
        let whole = Trace::from_events(vec![ev("a", None, 0, 0.0, Status::Success)]).unwrap();
        assert!(!whole.spans_truncated);
        assert_eq!((whole.spans_total, whole.spans_logged), (1, 1));
    }

    #[test]
    fn duplicate_span_ids_are_marked_not_silently_doubled() {
        // Two distinct calls report the same span_id. Both must surface (they are different events),
        // the second flagged, and only the first owns the id for parent linkage.
        let mut dup = ev("a", None, 1, 0.0, Status::Success);
        dup.id = "e-a2".into();
        let evs = vec![
            ev("a", None, 0, 0.0, Status::Success),
            dup,
            ev("c", Some("a"), 2, 0.0, Status::Success),
        ];
        let t = Trace::from_events(evs).unwrap();
        assert_eq!(count_nodes(&t.spans), 3, "no span dropped or duplicated");
        let flagged: Vec<&str> = flatten(&t.spans)
            .into_iter()
            .filter(|s| s.duplicate_span_id)
            .map(|s| s.event.id.as_str())
            .collect();
        assert_eq!(
            flagged,
            vec!["e-a2"],
            "only the later claimant is flagged: {flagged:?}"
        );
        // c parents under the FIRST "a", not the duplicate.
        let first = t.spans.iter().find(|s| s.event.id == "e-a").unwrap();
        assert_eq!(first.children.len(), 1);
        assert_eq!(first.children[0].event.span_id.as_deref(), Some("c"));
    }

    fn flatten(spans: &[TraceSpan]) -> Vec<&TraceSpan> {
        spans
            .iter()
            .flat_map(|s| {
                let mut v = vec![s];
                v.extend(flatten(&s.children));
                v
            })
            .collect()
    }

    #[test]
    fn missing_latency_is_treated_as_zero() {
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.latency_ms = None;
        let mut b = ev("b", Some("a"), 1, 0.0, Status::Success);
        b.latency_ms = None;
        let t = Trace::from_events(vec![a, b]).unwrap();
        assert_eq!(
            t.duration_ms, 1000,
            "no latency -> plain wall clock, no panic"
        );
        assert_eq!(t.totals.total_latency_ms, 0);
        assert_eq!(t.spans[0].latency_ms, None);
    }

    #[test]
    fn overlapping_spans_end_at_last_finish() {
        // a starts at 0 and runs 5s; b starts at +1s and runs 0.5s. Wall duration is the later
        // finish (5000ms), while compute time counts both (5500ms) even though they overlap.
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.latency_ms = Some(5000);
        let mut b = ev("b", Some("a"), 1, 0.0, Status::Success);
        b.latency_ms = Some(500);
        let t = Trace::from_events(vec![a, b]).unwrap();
        assert_eq!(
            t.duration_ms, 5000,
            "duration is the last-finishing span, not the last-started"
        );
        assert_eq!(t.totals.total_latency_ms, 5500);
    }

    #[test]
    fn single_span_offset_zero_and_latency_surfaced() {
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.latency_ms = Some(350);
        let t = Trace::from_events(vec![a]).unwrap();
        assert_eq!(t.duration_ms, 350);
        assert_eq!(t.spans.len(), 1);
        assert_eq!(t.spans[0].offset_ms, 0, "root sits at the trace start");
        assert_eq!(
            t.spans[0].latency_ms,
            Some(350),
            "latency mirrored onto the node"
        );
    }

    #[test]
    fn out_of_order_input_offsets_from_true_start() {
        // Feed events unsorted; offsets must anchor to the earliest ts regardless of input order.
        let a = ev("a", None, 0, 0.0, Status::Success); // +0s
        let b = ev("b", Some("a"), 3, 0.0, Status::Success); // +3s
        let c = ev("c", Some("a"), 1, 0.0, Status::Success); // +1s
        let t = Trace::from_events(vec![b, c, a]).unwrap();
        let root = &t.spans[0];
        assert_eq!(root.offset_ms, 0);
        // Children kept chronological: c (+1s) before b (+3s), with matching offsets.
        assert_eq!(root.children[0].offset_ms, 1000);
        assert_eq!(root.children[1].offset_ms, 3000);
    }

    #[test]
    fn coverage_records_what_was_judged() {
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.input = Some(serde_json::json!("what is 2+2?"));
        a.output = Some(serde_json::json!("4"));
        let t = Trace::from_events(vec![a, ev("b", Some("a"), 1, 0.0, Status::Success)]).unwrap();
        let cov = t.coverage();
        assert_eq!(cov.spans, 2);
        assert_eq!(cov.root_event_id, "e-a");
        assert!(!cov.truncated);
        assert_eq!(
            cov.digest.len(),
            16,
            "fixed-width hex digest: {}",
            cov.digest
        );
        // Same trace -> same digest, so a quiet trace never looks changed.
        assert_eq!(cov, t.coverage());
    }

    #[test]
    fn a_late_span_is_stale_but_a_changed_exchange_is_material() {
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.input = Some(serde_json::json!("q"));
        a.output = Some(serde_json::json!("first answer"));
        let scored = Trace::from_events(vec![a.clone()]).unwrap().coverage();

        // Nothing moved.
        assert_eq!(scored.drift(&scored), TraceDrift::None);
        assert_eq!(TraceDrift::None.reason(), None);

        // A trailing span lands: the trace is no longer what was judged, but the judged exchange is
        // byte-identical -> stale to a reader, NOT worth paying a judge for.
        let grown =
            Trace::from_events(vec![a.clone(), ev("b", Some("a"), 1, 0.0, Status::Success)])
                .unwrap()
                .coverage();
        assert_eq!(scored.drift(&grown), TraceDrift::Grown);
        assert_eq!(TraceDrift::Grown.reason(), Some("grown"));

        // The root exchange itself changes (the real root landed late, as OTel parents do, or the
        // streamed output filled in) -> the verdict describes text that is not there. Material.
        let mut edited = a.clone();
        edited.output = Some(serde_json::json!("a different answer"));
        let changed = Trace::from_events(vec![edited]).unwrap().coverage();
        assert_eq!(scored.drift(&changed), TraceDrift::Changed);
        assert_eq!(TraceDrift::Changed.reason(), Some("changed"));
    }

    #[test]
    fn a_truncated_read_is_not_a_changed_trace() {
        // The detail read keeps the OLDEST spans, so the root survives the cap. A trace scored whole
        // and later read clipped must fingerprint identically and report the same span count.
        let mut a = ev("a", None, 0, 0.0, Status::Success);
        a.input = Some(serde_json::json!("q"));
        a.output = Some(serde_json::json!("out"));
        let whole = Trace::from_events(vec![
            a.clone(),
            ev("b", Some("a"), 1, 0.0, Status::Success),
            ev("c", Some("a"), 2, 0.0, Status::Success),
        ])
        .unwrap()
        .coverage();
        let clipped =
            Trace::from_events_bounded(vec![a, ev("b", Some("a"), 1, 0.0, Status::Success)], 3)
                .unwrap()
                .coverage();

        assert_eq!(
            whole.digest, clipped.digest,
            "the cap must not read as a content change"
        );
        assert_eq!(
            clipped.spans, 3,
            "spans_total is the true count, not the clipped one"
        );
        assert!(clipped.truncated, "the clipped read is recorded as such");
        assert_eq!(whole.drift(&clipped), TraceDrift::None);
    }

    #[test]
    fn an_unknown_digest_is_never_called_a_content_change() {
        // A verdict written before coverage existed (or by a third party) records no digest: it may
        // be reported as stale on size, but never as materially changed — we would be spending a
        // judge call on a guess.
        let legacy = TraceCoverage {
            spans: 2,
            ..Default::default()
        };
        let now = TraceCoverage {
            spans: 2,
            root_event_id: "e-a".into(),
            digest: "deadbeefdeadbeef".into(),
            truncated: false,
        };
        assert_eq!(legacy.drift(&now), TraceDrift::None);
        assert_eq!(
            legacy.drift(&TraceCoverage { spans: 3, ..now }),
            TraceDrift::Grown
        );
    }

    #[test]
    fn fingerprint_separates_its_parts() {
        assert_ne!(fingerprint(&["ab", "c"]), fingerprint(&["a", "bc"]));
        assert_eq!(fingerprint(&["a", "b"]), fingerprint(&["a", "b"]));
    }

    fn count_nodes(spans: &[TraceSpan]) -> usize {
        spans.iter().map(|s| 1 + count_nodes(&s.children)).sum()
    }
}
