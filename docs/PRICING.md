# Pricing — where a cost number comes from, and how much to trust it

Every cost, margin, forecast and limit number in LightTrack is a sum over `events.cost_usd`. This
document is about the one question that sum cannot answer on its own: **where did each of those
numbers come from?**

The short version: a stored cost is one of four things, and they are not interchangeable.

## Stamped vs filled vs imputed vs absent

| Basis | `cost_usd` | `metadata.cost_source` | When it is written | Trust |
|---|---|---|---|---|
| **Client-reported** | the caller's own figure | `client` | at ingest, when the event body carried `cost_usd` | As good as the caller. Summed separately (`client_reported_cost_usd`) so you can see how much of a total rests on someone else's arithmetic. |
| **Book (stamped)** | our arithmetic | `book` | at ingest, from the rate **in force at the time** | The reference case. |
| **Book (filled)** | our arithmetic | `book_fill` + `priced_at` | later, by an operator running `?fill_unpriced=1` | Reconstructed. The rate may not be the rate that was in force; that is why it is a separate label and not `book`. |
| **Unpriced** | `NULL` | *(absent)* | never — the model was not in the book | Not a zero. A `SUM` over these understates by an unknown amount. |
| **Imputed** | *(not stored)* | — | at limit-evaluation time only | An estimate, disclosed in `cost_evidence`. Never written to a row. |

Two rules follow from that table, and both are load-bearing:

* **A missing price is never a zero.** An unpriceable call stores `NULL`. A cost figure computed over
  a window containing unpriced calls is a **floor**, not a total, and every surface that reports one
  also reports `unpriced_calls` beside it.
* **A cost is never silently restated.** Correcting a *wrong* rate does not rewrite history — it
  appends a new dated row and applies from there. The only write over stored costs is the forward
  fill, it only ever touches rows that had no cost at all, and it labels what it did.

## Seeing the gap: `GET /v1/costs/unpriced`

```
GET /v1/costs/unpriced?project=<id>&since=<rfc3339>      # READ scope; window defaults to 30 days
```

```json
{
  "since": "2026-08-02T00:00:00Z",
  "models": [
    { "provider": "acme", "model": "zoo-1", "calls": 4000,
      "input_tokens": 12000000, "output_tokens": 900000,
      "first_seen": "2026-08-02T00:00:00Z", "last_seen": "2026-08-30T00:00:00Z" }
  ],
  "unpriced_calls": 4000,
  "notes": "…every cost, margin and limit number over this window is a FLOOR until these are priced…",
  "price_book": { "verified_at": "2026-05-31T00:00:00Z", "stale": true,
                  "stale_after_days": 60, "rows": 41 }
}
```

Rows are ranked by `calls`: the first one is the price worth adding first. `first_seen` / `last_seen`
are **UTC day** granularity — the ledger is folded out of the grouped rollup primitive, and a day is
enough to answer the question the fields exist for ("is this still happening?").

Also available as `lt prices unpriced` and as the MCP read tool `list_unpriced_models`.

## Closing it: `?fill_unpriced=1`

```
PUT /v1/prices/acme/zoo-1?fill_unpriced=1        # admin
{ "input_per_mtok": 2.0, "output_per_mtok": 6.0,
  "verified_at": "2026-09-01", "note": "vendor pricing page" }
→ { …the stored row…, "filled": 4000, "remaining_unpriced": 0 }
```

What the fill does and does not do:

* Only rows with `cost_usd IS NULL` **for that exact `(provider, model)`** are eligible.
* Each row is priced through the *same* `PriceBook::cost_usd_mode` ingest uses, so prompt-length
  tiers (`@in>N`) and batch/flex lanes resolve exactly as they would have at the time.
* Every written row gets `metadata.cost_source = "book_fill"` and `metadata.priced_at`. The caller's
  own metadata (customer, product, prompt tag) is untouched.
* It is **idempotent**: a second run finds nothing left and reports `filled: 0`.
* `remaining_unpriced` is *measured* by re-reading the ledger, not inferred from the fill count — so
  a key the book still cannot price cannot read as "done".
* It is opt-in. Without the flag a `PUT` is exactly the price write it always was.

## The book is a timeline

`model_prices` is keyed `(provider, model, effective_from)` and is **append-only**. A rate correction
adds a row; it does not overwrite the row that priced last quarter's traffic.

* `GET /v1/prices` — the rate **in force now** (the latest `effective_from <= now`, per key). A
  future-dated row is stored but does not price today's traffic.
* `GET /v1/prices/history/:provider/:model` — the whole timeline, newest first. This is what a cost
  number from a past window is defended with.
* `PUT` accepts `effective_from` (default now), `verified_at`, and a free-text `note`. Writing the
  same `(provider, model, effective_from)` again corrects that one point on the timeline.

Existing deployments migrate automatically on the next start: SQLite rebuilds the table (the primary
key changes, which `ALTER` cannot express) carrying each row's `effective_date` across as its
`effective_from`; Postgres renames the column and widens the key in a guarded block; Firestore writes
new documents under a three-part id and reads either date spelling. No row is lost, and `verified_at`
is left `NULL` on migrated rows — nobody vouched for those rates, and claiming otherwise would make
the staleness warning below repeat a lie.

## Freshness

A cost dashboard computed from rates nobody has checked in two years does not *look* wrong. So the
book's age is reported rather than assumed:

* `verified_at` on a row is when a human last checked that rate against the vendor's page. The seed
  (`config/pricing.json`) stamps every row it creates with its own `_meta.last_verified`.
* The book is only as fresh as its **oldest** row. A book where nothing carries a `verified_at` is
  stale, not fresh.
* `LIGHTTRACK_PRICE_STALE_DAYS` (default `60`) is the budget. Past it, the API logs one `warn` line
  at boot, `GET /v1/costs` carries `x-price-book-stale: true` (and `x-price-book-verified-at`), and
  `/v1/costs/unpriced` reports the same posture as a `price_book` object.

The headers on `/v1/costs` rather than a body field are deliberate: that response is a bare array,
and its shape is a contract the renderer and the CLI are written against.

## Backend parity

The unpriced ledger, the forward fill and the price history are the `pricing` surface in
`docs/PARITY.md`. All three backends serve it. Two honest differences:

* **Firestore** applies the unpriced predicate client-side (it cannot query for an absent field
  alongside the window predicates) and fills by per-document `PATCH` rather than one transaction — an
  interrupted fill leaves a resumable state, which is what makes idempotency load-bearing there.
* **Firestore** windows on the client-declared `ts` rather than server arrival, as everywhere else on
  that backend (see the note in its rollup module).

A backend that did not serve the surface would answer **501 `unsupported`** on all three routes —
never an empty ledger, which would read as "nothing is unpriced".
