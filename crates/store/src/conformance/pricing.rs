//! `Surface::Pricing`: the unpriced ledger, the forward fill, and the dated price book.
//!
//! The bar is the loop, not the three methods separately — see the gap, add the price, watch the
//! gap close. A backend that listed the gap but filled nothing, or filled rows without labelling
//! them, would leave an operator worse informed than the honest NULL did.

use chrono::{Duration, Utc};

use lighttrack_core::{new_id, ModelPrice, ModelPriceRow, PriceBook};

use super::fixtures::sample_event;
use crate::pricing::{PriceFill, FILL_SOURCE};
use crate::Scope;
use crate::{Result, Store};

pub(super) fn pricing(store: &dyn Store) -> Result<()> {
    unpriced_ledger_and_fill(store)?;
    dated_book(store)
}

/// A price book that costs `<provider>/<model>` at $1 per Mtok in, $0 out.
fn book_for(provider: &str, model: &str) -> PriceBook {
    let mut m = std::collections::HashMap::new();
    m.insert(
        PriceBook::key(provider, model),
        ModelPrice {
            input_per_mtok: 1.0,
            output_per_mtok: 0.0,
            cached_input_per_mtok: None,
            aliases: Vec::new(),
        },
    );
    PriceBook::new(m)
}

fn unpriced_ledger_and_fill(store: &dyn Store) -> Result<()> {
    let pid = new_id();
    let model = format!("unpriced-{}", new_id());
    let since = Utc::now() - Duration::days(1);

    // Two unpriceable calls (cost_usd = NULL — the null-cost invariant) and one the caller costed
    // itself. The third is the row a fill must never touch.
    for (i, cost) in [None, None, Some(7.0)].into_iter().enumerate() {
        let mut ev = sample_event(&pid, &model, 1_000_000, 0, 0.0);
        ev.provider = "conformance".into();
        ev.cost_usd = cost;
        if cost.is_some() {
            ev.metadata = serde_json::json!({ "cost_source": "client", "n": i });
        }
        store.insert_event(&ev)?;
    }

    let ledger = store.list_unpriced(Scope::Project(&pid), since)?;
    let row = ledger
        .iter()
        .find(|r| r.model == model)
        .expect("the unpriced key is listed");
    assert_eq!(row.provider, "conformance");
    assert_eq!(row.calls, 2, "the client-costed call is not unpriced");
    assert_eq!(
        row.input_tokens, 2_000_000,
        "token sums cover the unpriced rows only"
    );
    assert!(row.first_seen <= row.last_seen);
    assert!(
        row.first_seen >= since,
        "the ledger stays inside its window"
    );

    // …add the price, and fill.
    let book = book_for("conformance", &model);
    let filled = store.fill_unpriced_cost(&PriceFill::new("conformance", &model, &book))?;
    assert_eq!(filled, 2, "both unpriced rows were priced");

    // Idempotency: the gap is closed, so a second fill finds nothing.
    assert_eq!(
        store.fill_unpriced_cost(&PriceFill::new("conformance", &model, &book))?,
        0,
        "a fill is idempotent — the second pass has nothing left to price"
    );

    // …and the ledger agrees, which is the whole loop.
    assert!(
        !store
            .list_unpriced(Scope::Project(&pid), since)?
            .iter()
            .any(|r| r.model == model),
        "the key is gone from the ledger once it is priced"
    );

    // Provenance: a filled row says it was filled, and a client-costed row is untouched.
    let mut filled_rows = 0;
    for ev in store.list_events(Scope::Project(&pid), 100)? {
        if ev.model != model {
            continue;
        }
        match ev.metadata.get("cost_source").and_then(|v| v.as_str()) {
            Some("client") => assert_eq!(
                ev.cost_usd,
                Some(7.0),
                "a caller-reported cost is never repriced"
            ),
            Some(s) => {
                assert_eq!(s, FILL_SOURCE, "a filled row is labelled as filled");
                assert_eq!(ev.cost_usd, Some(1.0));
                assert!(
                    ev.metadata.get("priced_at").is_some(),
                    "a filled row records WHEN it was priced"
                );
                filled_rows += 1;
            }
            None => panic!("a row was left with no cost provenance at all"),
        }
    }
    assert_eq!(filled_rows, 2);
    Ok(())
}

/// The book is a timeline: a later rate appends, `list_prices` shows the current one, and the
/// history shows both. Overwriting is exactly what M26 removed — a cost number from June cannot be
/// defended against a table that only remembers today's rate.
fn dated_book(store: &dyn Store) -> Result<()> {
    let model = format!("dated-{}", new_id());
    let at = |days: i64, rate: f64| ModelPriceRow {
        provider: "conformance".into(),
        model: model.clone(),
        input_per_mtok: rate,
        output_per_mtok: 0.0,
        cached_input_per_mtok: None,
        effective_from: Utc::now() - Duration::days(days),
        source_url: None,
        verified_at: Some(Utc::now() - Duration::days(days)),
        note: Some(format!("rate {rate}")),
    };
    store.upsert_price(&at(30, 1.0))?;
    store.upsert_price(&at(2, 3.0))?;

    let history = store.list_price_history("conformance", &model)?;
    assert_eq!(history.len(), 2, "the older rate is kept, not overwritten");
    assert!(
        history[0].effective_from > history[1].effective_from,
        "history is newest-first"
    );
    assert_eq!(history[0].note.as_deref(), Some("rate 3"));
    assert!(
        history[0].verified_at.is_some(),
        "verified_at survives the round-trip"
    );

    let current: Vec<ModelPriceRow> = store
        .list_prices()?
        .into_iter()
        .filter(|p| p.model == model)
        .collect();
    assert_eq!(current.len(), 1, "the current book carries one row per key");
    assert!(
        (current[0].input_per_mtok - 3.0).abs() < 1e-9,
        "the later rate"
    );

    // A future-dated rate is stored but must not price today.
    let mut future = at(0, 99.0);
    future.effective_from = Utc::now() + Duration::days(30);
    store.upsert_price(&future)?;
    let current = store
        .list_prices()?
        .into_iter()
        .find(|p| p.model == model)
        .expect("still current");
    assert!(
        (current.input_per_mtok - 3.0).abs() < 1e-9,
        "a rate that has not taken effect must not be charged"
    );
    assert_eq!(
        store.list_price_history("conformance", &model)?.len(),
        3,
        "…but it is stored, and visible on the timeline"
    );
    Ok(())
}
