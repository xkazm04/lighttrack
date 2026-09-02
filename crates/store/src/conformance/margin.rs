//! `Surface::MarginBreakdowns`: the token and per-customer cost splits behind the pricing what-if
//! and the "where is this customer's spend going" drill-down.
//!
//! Two properties, both of which the trait defaults used to violate silently: the breakdown is
//! **scoped to the customer asked for** (leaking another tenant's spend into it is worse than
//! refusing), and its parts **sum to that customer's window total** — a breakdown that quietly drops
//! a bucket is a number someone reprices on.

use chrono::{Duration, Utc};

use lighttrack_core::new_id;

use super::fixtures::tagged_event;
use crate::{Result, Store};

pub(super) fn margin(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let now = Utc::now();
    let since = now - Duration::hours(1);
    let until = now + Duration::hours(1);

    let mk = |customer: &str, model: &str, name: &str, cost: f64| -> Result<()> {
        let mut e = tagged_event(&pid, customer, cost);
        e.model = model.into();
        e.name = Some(name.into());
        e.usage.input = 100;
        e.usage.output = 50;
        store.insert_event(&e)
    };
    mk("cus-m-a", "m-1", "summarize", 1.0)?;
    mk("cus-m-a", "m-2", "classify", 2.0)?;
    mk("cus-m-b", "m-1", "summarize", 4.0)?;

    // --- tokens by dimension ---
    let tokens = store.tokens_by_dimension(Some(&pid), "customer", since, until)?;
    let a = tokens
        .iter()
        .find(|r| r.key.as_deref() == Some("cus-m-a"))
        .expect("the tagged customer has a token bucket");
    assert_eq!(
        a.tokens, 300,
        "prompt+completion tokens for the customer's two calls"
    );
    assert!(
        tokens.iter().any(|r| r.key.as_deref() == Some("cus-m-b")),
        "every dimension value with traffic appears"
    );
    assert_eq!(
        tokens.iter().map(|r| r.tokens).sum::<i64>(),
        450,
        "the parts sum to the window's tokens — a dropped bucket reprices the wrong volume"
    );

    // --- one customer's cost by model ---
    let by_model = store.customer_cost_by_model(Some(&pid), "cus-m-a", since, until)?;
    let total: f64 = by_model.iter().map(|r| r.cost_usd).sum();
    assert!(
        (total - 3.0).abs() < 1e-9,
        "the breakdown sums to this customer's window cost, and excludes cus-m-b (got {total})"
    );
    assert_eq!(by_model.len(), 2, "one row per model: {by_model:?}");
    assert!(
        by_model
            .iter()
            .all(|r| r.key.contains("m-1") || r.key.contains("m-2")),
        "rows are keyed by the model (provider/model): {by_model:?}"
    );
    assert_eq!(
        by_model.iter().map(|r| r.calls).sum::<i64>(),
        2,
        "call counts are the customer's, not the project's"
    );

    // --- the same cost, split by use-case name ---
    let by_name = store.customer_cost_by_name(Some(&pid), "cus-m-a", since, until)?;
    let named: f64 = by_name.iter().map(|r| r.cost_usd).sum();
    assert!(
        (named - total).abs() < 1e-9,
        "the two splits of one customer's spend agree on the total ({named} vs {total})"
    );
    assert!(
        by_name.iter().any(|r| r.key == "summarize"),
        "rows are keyed by the use-case name: {by_name:?}"
    );

    // A customer with no traffic is an empty breakdown, not someone else's.
    assert!(
        store
            .customer_cost_by_model(Some(&pid), "cus-nobody", since, until)?
            .is_empty(),
        "an unknown customer's breakdown is empty — never the project-wide total"
    );
    Ok(())
}
