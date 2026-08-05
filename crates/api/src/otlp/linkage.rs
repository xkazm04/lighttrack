//! Keep an OTel trace's *shape* when its non-GenAI spans are dropped.
//!
//! A typical export nests LLM spans under HTTP-handler / tool / DB spans that carry no `gen_ai.*`
//! attributes. Those are refused (`not_genai`) — the event table is for LLM calls, and inventing
//! phantom `LlmEvent`s for them would corrupt cost, token and span accounting. But their *linkage* is
//! real: with the parents gone, every GenAI span whose direct OTel parent was non-GenAI looked
//! parentless, and `Trace::build_forest` treats a missing parent as a root. One connected upstream
//! trace therefore fragmented into N roots here.
//!
//! So the link is preserved without the span: walk the original parent chain (which we still have,
//! flattened, for the whole export) past the dropped spans up to the nearest ancestor that *did*
//! become an event, and reparent onto it. The rewritten span keeps its original parent under
//! `metadata.otel.otlp_parent_span_id`, so the exporter's own topology is never lost.
//!
//! Only this export's spans are visible, so a parent exported in a different batch stays dangling —
//! a root, exactly as before.

use std::collections::{HashMap, HashSet};

use lighttrack_core::LlmEvent;

use super::proto::FlatSpan;
use super::semconv::nonempty;

/// Guard against a malformed export whose parent links form a cycle or an absurd chain.
const MAX_ANCESTOR_HOPS: usize = 64;

/// Rewire each mapped event's `parent_span_id` past the spans that were dropped as non-GenAI.
///
/// `events[k]` is the event mapped from `spans[span_of_event[k]]`.
pub(super) fn reparent_past_dropped_spans(
    spans: &[FlatSpan<'_>],
    events: &mut [LlmEvent],
    span_of_event: &[usize],
) {
    // The full OTel topology, including the spans that never became events.
    let mut parent_of: HashMap<String, String> = HashMap::new();
    for fs in spans {
        if let (Some(id), Some(parent)) = (
            nonempty(fs.span.span_id.as_deref()),
            nonempty(fs.span.parent_span_id.as_deref()),
        ) {
            parent_of.insert(id, parent);
        }
    }
    let mapped: HashSet<String> = span_of_event
        .iter()
        .filter_map(|&i| nonempty(spans[i].span.span_id.as_deref()))
        .collect();

    for ev in events.iter_mut() {
        let Some(parent) = ev.parent_span_id.clone() else {
            continue;
        };
        if mapped.contains(&parent) {
            continue; // the parent became an event: the link already holds
        }
        let Some(ancestor) = nearest_mapped_ancestor(&parent, &parent_of, &mapped) else {
            continue; // no GenAI ancestor in this export — leave the exporter's parent as it was
        };
        record_original_parent(ev, &parent);
        ev.parent_span_id = Some(ancestor);
    }
}

/// Climb `parent_of` from `from` until a span that became an event is found.
fn nearest_mapped_ancestor(
    from: &str,
    parent_of: &HashMap<String, String>,
    mapped: &HashSet<String>,
) -> Option<String> {
    let mut cur = from.to_string();
    let mut seen: HashSet<String> = HashSet::new();
    for _ in 0..MAX_ANCESTOR_HOPS {
        if !seen.insert(cur.clone()) {
            return None; // cycle
        }
        let next = parent_of.get(&cur)?.clone();
        if mapped.contains(&next) {
            return Some(next);
        }
        cur = next;
    }
    None
}

/// Keep the exporter's own parent id under `metadata.otel`, beside the rest of the OTel provenance,
/// so a synthesized link is visible as synthesized rather than passing for what was exported.
fn record_original_parent(ev: &mut LlmEvent, original: &str) {
    let Some(map) = ev.metadata.as_object_mut() else {
        return;
    };
    let otel = map
        .entry("otel".to_string())
        .or_insert_with(|| serde_json::Value::Object(Default::default()));
    if let Some(otel) = otel.as_object_mut() {
        otel.insert(
            "otlp_parent_span_id".to_string(),
            serde_json::Value::String(original.to_string()),
        );
    }
}
