# Currency handling & FX rates

Billing providers (Stripe, Polar) report amounts in a currency's **minor unit** (Stripe `amount_paid`,
Polar `total_amount`). Before that number can be summed as USD revenue and netted against LLM cost,
LightTrack normalizes it in two steps — both in `crates/billing/src/fx.rs`:

1. **Minor → major**, by the *currency's* ISO-4217 decimal places — never a blanket `/100`.
   - Zero-decimal currencies (JPY, KRW, VND, CLP, …): minor unit **is** the major unit, divisor `1`.
   - Three-decimal currencies (KWD, BHD, OMR, …): divisor `1000`.
   - Everything else: divisor `100`.
   Dividing JPY by 100 understated yen revenue 100×; that is the bug this fixes.
2. **Major → USD**, at a rate from the FX book. USD is the base (implicit `1.0`).

`RevenueEvent.amount_usd` is therefore genuine USD. The original `currency` label is **preserved
untouched** on the record (a EUR invoice stays `currency = "EUR"`), so the source of truth is not lost.

## The rate book: `config/fx_rates.json`

Same "seed a static book from JSON at startup" pattern as `config/pricing.json`.

```json
{
  "base": "USD",
  "rates": { "EUR": 1.09, "JPY": 0.0064, "GBP": 1.27 }
}
```

- Each rate is the **USD value of one major unit** of the currency, so `amount_usd = major * rate`.
- Path override: `LIGHTTRACK_FX_RATES`. Missing/unparseable file → USD-only table (loudly logged).
- Loaded **once** per process (`shared_fx()`), shared by the billing adapters (at ingest) and the
  `/v1/margin` surface (to detect unconverted currencies) so both agree on one book.

These rates are a **periodic manual snapshot, not a live feed** — margins are only as fresh as this
file. This is deliberate: revenue recognition should be deterministic and auditable, not silently
re-priced by an external feed between reports.

### How to update the rates

1. Pull the reference rates (ECB euro reference rates, or the IMF representative rates — see the
   `_meta.sources` in the file) for the day you want to reconcile against.
2. Convert each to "USD per 1 unit of the currency" if the source quotes it the other way
   (`usd_per_unit = 1 / units_per_usd`).
3. Edit `config/fx_rates.json`, bump `_meta.last_verified`, and restart the API (rates load at boot).
4. Add any currency your customers actually pay in — see the unconverted-currency warning below.

## Unconverted currencies (no silent 1:1)

A non-USD currency **absent from the rate book** is deliberately *not* treated as USD silently. Instead:

- The event is still stored (revenue is never dropped), with `amount_usd` computed at **1:1** on its
  major units, and the original `currency` kept.
- The row records that it happened: `converted: false`, plus the provider's own `amount_minor`, the
  `fx_rate` used (none, here) and the `fx_book_version` in force. `GET /v1/margin` reads the caveat
  off the **rows**, not off the live rate book — so adding a rate later no longer makes the warning
  disappear while the stored figures stay wrong:

  ```json
  {
    "total_revenue_usd": 1234.5,
    "unconverted_currencies": ["GBP", "SEK"],
    "currency_note": "unconverted currencies present (stored 1:1, USD figures approximate): GBP, SEK. Add rates to config/fx_rates.json, then restate the stored rows with POST /v1/revenue/reprice - adding a rate alone fixes future syncs, not these rows."
  }
  ```

  The Markdown renderer shows this as a `⚠️` caveat under the totals. This makes a missing rate a
  loud, visible caveat rather than a quiet mis-statement of revenue.

## Fixing a missing rate after the fact

"Add the rate and re-ingest" was the old answer and it is not one: a Stripe webhook cannot be
replayed on demand, and the rows are already someone's recognized revenue. Because each row keeps the
provider's own minor-unit figure, the correction is applied where the mistake is.

1. Add the rate to `config/fx_rates.json` and restart (this fixes **future** syncs only).
2. Preview: `lt reprice --currency GBP` → `{"matched": 42, "changed": 40, "dry_run": true}`.
3. Apply: `lt reprice --currency GBP --apply`.

Or over HTTP, admin key required — it is never exposed over MCP:

```
POST /v1/revenue/reprice?currency=GBP           # previews; dry_run defaults to true
POST /v1/revenue/reprice?currency=GBP&dry_run=0 # writes
```

Three properties worth knowing before you run it:

- **Only 1:1-fallback rows are touched.** A row that converted genuinely is recognized revenue at a
  rate that was correct when it was taken; re-basing it would restate history.
- **`matched` and `changed` differ, and the gap is the point.** A row that took the fallback but
  carries no `amount_minor` (a manual post, or a row written before this provenance existed) matches
  the correction and cannot take it — there is nothing to re-multiply, and deriving a figure from the
  bad `amount_usd` would launder the error into a confident number. Those rows stay flagged.
- **The rows are stamped** with the `fx_book_version` that repriced them, so the next question —
  "which rows already have the fixed rate?" — is answerable.

## Redelivery does not silently restate

A billing provider retries any non-2xx, and a retry re-runs the conversion against *today's* rate
book. The upsert therefore restates `amount_usd` (and the rate, book version and `converted` flag)
only when the provider's own `amount_minor` actually changed — i.e. when it is a different charge.
An identical redelivery keeps the conversion the row already has. Correcting a rate is a deliberate
act (above), not a side effect of a webhook being retried.
