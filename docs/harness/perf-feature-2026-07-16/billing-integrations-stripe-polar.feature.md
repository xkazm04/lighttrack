# Feature Scout — Billing Integrations (Stripe/Polar)

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Polar has no reconcile/backfill path — webhook gaps are permanent
- **Severity**: High
- **Category**: adapter-parity
- **File**: `crates/runner/src/billing.rs:27-31,39-88`
- **Scenario**: LightTrack is down (deploy, crash, cert expiry) for 20 minutes while a Polar customer's subscription renews. Polar retries a webhook a handful of times, then gives up. The operator runs `lt-runner billing sync polar --project X` to backfill — and gets `unsupported billing provider for sync: polar`.
- **Root cause**: `sync()` matches only `"stripe"`; every other provider is an error. The Polar adapter is fully built (`polar::normalize_order`, `normalize_refund`, etc. are `pub`) and reachable from the webhook, but there is **no runner path that pulls Polar orders and re-normalizes them**. Polar revenue is webhook-only, so any missed delivery is unrecoverable — the exact failure mode the Stripe sync loop exists to prevent.
- **Impact**: Polar revenue silently under-counts whenever a webhook is missed, and margin (`revenue − LLM cost`) is overstated with no way to reconcile. The product's headline number quietly drifts for every Polar customer. Stripe customers get a safety net Polar customers don't.
- **Fix sketch**: Add `sync_polar()` mirroring `sync_stripe()`: page Polar's `GET /v1/orders` (filter `is_paid=true`, `modified_at`/`created_at >= since`) with `POLAR_API_KEY`, feed each order to `polar::normalize_order` (already `pub`), stamp `project_id`, POST to `/v1/revenue`. Also page refunded orders → `normalize_order_refund`. Wire `"polar" => sync_polar(...)` into the match.
- **Trade-offs**: Needs a Polar API token in the runner env; network-bound so uncovered in CI (same caveat the module already documents for Stripe).

## 2. Stripe backfill pulls only paid invoices — refunds are never reconciled
- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/runner/src/billing.rs:44-84`
- **Scenario**: A webhook delivery for a `charge.refunded` is lost (downtime / transient 500). The operator reconciles with `lt-runner billing sync stripe`. The invoice re-imports, but the $400 refund never does — the customer shows full revenue and the refund is invisible until a human notices margin looks too good.
- **Root cause**: `sync_stripe` queries only `GET /v1/invoices?status=paid` and calls only `normalize_invoice`. The webhook path handles `charge.refunded` via `stripe::normalize_refund` (also `pub`), but the backfill loop pulls no refund/charge data at all. So reconciliation is asymmetric: it can recover a **missed payment** but never a **missed refund** — and a missed refund overstates revenue, the more dangerous direction for a margin product.
- **Impact**: Any refund that slips the webhook is permanently lost from LightTrack's revenue ledger. Reconciliation, whose whole purpose is a trustworthy month-end truth, systematically overstates margin.
- **Fix sketch**: In the same loop (or a second pass), page `GET /v1/charges?created[gte]=since` (or `/v1/refunds`) and run each through `stripe::normalize_refund`; POST the negative records to `/v1/revenue` (idempotent by `stripe:refund:{id}`). Track a separate `refunds` counter in the summary line.
- **Trade-offs**: Roughly doubles Stripe API calls during a sync; none material — sync is an occasional reconcile job, not a hot path.

## 3. Disputes / chargebacks are unhandled on both adapters — clawed-back revenue is never reversed
- **Severity**: Medium
- **Category**: capability-gap
- **File**: `crates/billing/src/stripe.rs:93-104`; `crates/billing/src/polar.rs:122-134`
- **Scenario**: A customer files a card chargeback. Stripe sends `charge.dispute.created` / `charge.dispute.funds_withdrawn`; the money leaves the merchant account. LightTrack's `normalize` match has no arm for `charge.dispute.*`, so it returns an empty vec, 200s, and the provider stops retrying. The recognized revenue is never reversed.
- **Root cause**: Stripe `normalize` tracks only `invoice.paid|payment_succeeded` and `charge.refunded`; Polar `normalize` tracks only `order.paid`, `order.refunded`, `refund.created`. Disputes/chargebacks — economically a clawback identical to a refund — fall through the `_ => Vec::new()` arm on both. A chargeback is real revenue removed from the merchant, so the gap directly inflates the margin numerator.
- **Impact**: For any customer base with card disputes (inevitable at scale), margin is overstated by the disputed sum with no signal. The "revenue" half of the headline number is knowingly incomplete on both providers.
- **Fix sketch**: Add a `charge.dispute.funds_withdrawn` (and reversal `funds_reinstated`) arm to Stripe `normalize` emitting a `RevenueKind::Refund` record keyed `stripe:dispute:{id}`; add Polar's dispute/chargeback event type similarly once its payload shape is confirmed. Reuse the existing refund-event shaping.
- **Trade-offs**: Dispute payloads carry the charge id, not always the customer — customer attribution may need a lookup or be left to the same fallback chain refunds use. Confirm exact Stripe/Polar event names against live payloads before wiring.
