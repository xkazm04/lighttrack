//! Cross-backend parity for the one thing a model-scoped cap's ledger must agree with its gate
//! about: **membership**.
//!
//! [`LimitScope::matches`] decides a `Model` rule *applies* by canonical family — a `gpt-4o` cap
//! catches `gpt-4o-2024-08-06` and `openai/gpt-4o`. Every backend's `usage_since_scoped` used to
//! decide the number that rule is *evaluated against* with raw string equality, so a cap over
//! dated-snapshot traffic read `current ≈ 0` forever while still reporting as enforcing.
//!
//! This section pins the fold on every backend. A backend that cannot serve it must answer
//! [`StoreError::Unsupported`] (→ HTTP 501) — **a short count is the bug, a 501 is honest.**

use chrono::Utc;

use lighttrack_core::{new_id, LimitScope};

use super::fixtures::sample_event;
use crate::{Result, Store, StoreError, Surface};

pub(super) fn model_family_usage(store: &dyn Store) -> Result<()> {
    let pid = &new_id();
    let since = Utc::now() - chrono::Duration::hours(1);

    // Four OpenAI rows: the exact cap value, the provider-prefixed spelling, a dated snapshot of
    // the same family, and a genuinely different family that must stay outside the scope.
    for (model, cost) in [
        ("gpt-4o", 1.0),
        ("openai/gpt-4o", 2.0),
        ("gpt-4o-2024-08-06", 4.0),
        ("gpt-4o-mini", 8.0),
    ] {
        let mut ev = sample_event(pid, model, 10, 5, cost);
        ev.provider = "openai".into();
        store.insert_event(&ev)?;
    }

    let scope = LimitScope::Model("gpt-4o".into());
    let scoped = match store.usage_since_scoped(pid, since, &scope) {
        Ok(u) => u,
        // The fold needs the grouped rollup. A backend that cannot serve it says so rather than
        // returning the three-row total minus the rows it could not see.
        Err(StoreError::Unsupported(why)) => {
            assert!(
                !store.capabilities().has(Surface::Rollup),
                "a backend declaring Surface::Rollup must serve the model-family fold, not refuse \
                 it: {why}"
            );
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    assert_eq!(
        scoped.calls, 3,
        "a `gpt-4o` cap counts gpt-4o + openai/gpt-4o + gpt-4o-2024-08-06 — the same three rows \
         LimitScope::matches would let it reject (got {} calls)",
        scoped.calls
    );
    assert!(
        (scoped.cost_usd - 7.0).abs() < 1e-9,
        "the family total is 1+2+4 = 7.0, not the raw-equality 1.0 (got {})",
        scoped.cost_usd
    );

    // A different canonical family is still outside the scope: the fold widens membership to the
    // family, it does not widen it to a prefix match.
    let mini = store.usage_since_scoped(pid, since, &LimitScope::Model("gpt-4o-mini".into()))?;
    assert_eq!(scoped.calls + mini.calls, 4, "the two families partition");
    assert!(
        (mini.cost_usd - 8.0).abs() < 1e-9,
        "gpt-4o-mini is its own family (got {})",
        mini.cost_usd
    );

    // Exact-value dimensions keep exact semantics — a prefix is not membership there.
    let mut tagged = sample_event(pid, "gpt-4o", 1, 1, 0.5);
    tagged.provider = "openai".into();
    tagged.name = Some("summarize-v2".into());
    tagged.metadata = serde_json::json!({ "customer_id": "cus_123x" });
    store.insert_event(&tagged)?;
    let name = store.usage_since_scoped(pid, since, &LimitScope::Name("summarize".into()))?;
    assert_eq!(
        name.calls, 0,
        "`summarize` must not count `summarize-v2`: a use-case cap is exact"
    );
    let cust = store.usage_since_scoped(pid, since, &LimitScope::Customer("cus_123".into()))?;
    assert_eq!(
        cust.calls, 0,
        "`cus_123` must not count `cus_123x`: a customer cap is exact"
    );

    Ok(())
}
