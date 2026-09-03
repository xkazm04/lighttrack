//! Forward fill on Firestore: price the event documents a missing rate left with no `cost_usd`.
//!
//! Same contract as the SQL backends — only unpriced documents for one `(provider, model)`, priced
//! through the caller's book so tiers and lanes resolve identically, each write stamped
//! `metadata.cost_source = "book_fill"` and `metadata.priced_at`.
//!
//! Two honest differences from the SQL backends, both of them properties of this backend rather than
//! of this feature: the eligibility filter (`cost_usd` absent) is applied **client-side**, because
//! Firestore cannot query for a missing field alongside two equality predicates without a dedicated
//! index; and the writes are `PATCH`es of two fields per document rather than one transaction, so an
//! interrupted fill leaves some rows priced and the rest still NULL. That is a resumable state, not
//! a corrupt one — re-running the fill finishes the job, which is exactly what makes idempotency
//! load-bearing here.

use serde_json::Value;

use lighttrack_core::{PricingMode, TokenUsage};
use lighttrack_store::pricing::PriceFill;
use lighttrack_store::Result;

use crate::codec::*;
use crate::rest::Rest;

const COLL: &str = "events";

pub(crate) fn fill(rest: &Rest, f: &PriceFill<'_>) -> Result<u64> {
    let filters = vec![
        ("provider", "EQUAL", serde_json::json!(f.provider)),
        ("model", "EQUAL", serde_json::json!(f.model)),
    ];
    let docs = rest.query(COLL, &filters, None, None)?;

    let mut filled = 0u64;
    for m in &docs {
        // Already costed — by the caller or by the book at ingest. Never touched, whatever the new
        // rate says: a fill closes a gap, it does not restate history.
        if ff64(m, "cost_usd").is_some() {
            continue;
        }
        let Some(id) = fstr(m, "id") else { continue };
        let usage = TokenUsage {
            input: fi64(m, "input_tokens").unwrap_or(0).max(0) as u64,
            output: fi64(m, "output_tokens").unwrap_or(0).max(0) as u64,
            cached_input: fi64(m, "cached_input_tokens").map(|v| v.max(0) as u64),
            reasoning: fi64(m, "reasoning_tokens").map(|v| v.max(0) as u64),
        };
        let tags: Vec<String> = fjson(m, "tags")
            .ok()
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();
        let mut metadata: Value = fjson(m, "metadata").unwrap_or(Value::Null);
        let Some(cost) = f.cost_for(&usage, PricingMode::from_hints(&metadata, &tags)) else {
            continue;
        };
        f.stamp(&mut metadata);

        let mut fields = Fields::new();
        fields.insert("cost_usd".into(), serde_json::json!(cost));
        fields.insert("metadata".into(), serde_json::json!(metadata.to_string()));
        rest.patch_fields(COLL, &id, &fields, &["cost_usd", "metadata"])?;
        filled += 1;
    }
    Ok(filled)
}
