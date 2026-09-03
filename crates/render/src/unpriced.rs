//! `list_unpriced_models` — the traffic nothing could cost, and how stale the rates that *did*
//! apply are.
//!
//! Rendered as a worklist rather than a table of facts: the rows arrive ranked by call count, so
//! the first line is the price worth adding first, and the footer carries the exact command that
//! closes it. A ledger an operator reads and then has to go and look up how to act on is a ledger
//! that stays full.

use serde_json::Value;

use crate::md::{commafy, opt_b, opt_s, s, short_ts, trunc, u, Align, Table};

pub(crate) fn ledger(v: &Value) -> Option<String> {
    let rows = v.get("models")?.as_array()?;
    let mut out = String::new();

    if rows.is_empty() {
        out.push_str(
            "**Every call in the window was priced.** No `(provider, model)` pair carried \
                      traffic the price book could not cost.\n",
        );
    } else {
        let mut t = Table::new(&[
            ("Provider", Align::Left),
            ("Model", Align::Left),
            ("Calls", Align::Right),
            ("In tok", Align::Right),
            ("Out tok", Align::Right),
            ("First seen", Align::Left),
            ("Last seen", Align::Left),
        ]);
        for r in rows {
            t.row(vec![
                trunc(s(r, "provider"), 20),
                trunc(s(r, "model"), 34),
                commafy(u(r, "calls")),
                commafy(u(r, "input_tokens")),
                commafy(u(r, "output_tokens")),
                short_ts(s(r, "first_seen")),
                short_ts(s(r, "last_seen")),
            ]);
        }
        out.push_str(&t.render());
        out.push_str(&format!(
            "\n\n**{} unpriced calls** across {} model(s). Every cost, margin and limit number over \
             this window is a FLOOR.\n",
            commafy(u(v, "unpriced_calls")),
            rows.len()
        ));
        let top = &rows[0];
        out.push_str(&format!(
            "\nClose the biggest gap first:\n```\nPUT /v1/prices/{}/{}?fill_unpriced=1\n```\n",
            s(top, "provider"),
            s(top, "model"),
        ));
    }

    // The book's own freshness belongs beside the gap: a fully-priced window computed from rates
    // nobody has checked in a year is the same problem wearing a better disguise.
    if let Some(book) = v.get("price_book") {
        let verified = opt_s(book, "verified_at")
            .map(short_ts)
            .unwrap_or_else(|| "never".into());
        let mark = if opt_b(book, "stale").unwrap_or(false) {
            "⚠ STALE"
        } else {
            "ok"
        };
        out.push_str(&format!(
            "\n_Price book: {} rows, oldest verification {verified} — {mark}._\n",
            u(book, "rows"),
        ));
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn body(models: Value) -> Value {
        json!({
            "since": "2026-08-01T00:00:00Z",
            "models": models,
            "unpriced_calls": 4200,
            "notes": "…",
            "price_book": { "verified_at": "2026-01-02T00:00:00Z", "stale": true, "rows": 41 },
        })
    }

    #[test]
    fn the_worklist_leads_with_the_command_that_closes_the_top_row() {
        let md = ledger(&body(json!([
            { "provider": "acme", "model": "zoo-1", "calls": 4000, "input_tokens": 10,
              "output_tokens": 5, "first_seen": "2026-08-02T00:00:00Z",
              "last_seen": "2026-08-30T00:00:00Z" },
            { "provider": "acme", "model": "zoo-2", "calls": 200, "input_tokens": 1,
              "output_tokens": 1, "first_seen": "2026-08-02T00:00:00Z",
              "last_seen": "2026-08-02T00:00:00Z" },
        ])))
        .expect("renders");
        assert!(md.contains("zoo-1"), "{md}");
        assert!(
            md.contains("PUT /v1/prices/acme/zoo-1?fill_unpriced=1"),
            "the fix is named, not left as an exercise: {md}"
        );
        assert!(md.contains("FLOOR"), "{md}");
        assert!(
            md.contains("STALE"),
            "the book's own age travels with it: {md}"
        );
    }

    /// An empty ledger must read as "everything is priced", never as a blank page that could equally
    /// mean the query failed.
    #[test]
    fn an_empty_ledger_says_so_in_words() {
        let md = ledger(&body(json!([]))).expect("renders");
        assert!(md.contains("Every call in the window was priced"), "{md}");
        assert!(md.contains("Price book"), "{md}");
    }

    #[test]
    fn a_wrong_shape_falls_through_to_raw_json() {
        assert!(ledger(&json!({ "nope": 1 })).is_none());
    }
}
