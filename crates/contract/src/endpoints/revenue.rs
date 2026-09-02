//! Revenue ingest, the FX restatement, the margin rollups (window / trend / per-customer /
//! simulated), and the billing providers' signed webhook door.

use crate::dsl::*;
use crate::types::*;
use Access::*;

/// `?by=` takes the same two dimensions everywhere margin is grouped.
const BY_DIM: &[&str] = &["customer", "product"];

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "post_revenue",
        method: Method::Post,
        path: "/v1/revenue",
        access: Admin,
        mutating: true,
        body: Some(TypeRef::Named("RevenueEvent")),
        response: TypeRef::Named("RevenueEvent"),
        cli: Some(&["revenue", "record"]),
        doc: "Record one revenue event (manual post, or a billing sync) for profit tracking.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "reprice_revenue",
        method: Method::Post,
        path: "/v1/revenue/reprice",
        access: Admin,
        mutating: true,
        params: &[
            qr("currency", "ISO-4217 code to restate, e.g. `GBP`"),
            q("project", "scope to one project; absent restates every project"),
            qt(
                "rate",
                JsonTy::Number,
                "USD per major unit; absent takes the server's current FX book rate",
            ),
            q(
                "dry_run",
                "`1`/`true` (the DEFAULT) previews and writes nothing; `0` applies the restatement",
            ),
        ],
        response: TypeRef::Untyped(
            "{ currency, rate, book_version, matched, changed, dry_run } — `matched` counts the \
             1:1-fallback rows in that currency, `changed` the subset that carried a minor-unit \
             figure to re-multiply.",
        ),
        // Deliberately not exposed over MCP: this restates stored money in bulk.
        cli: Some(&["reprice"]),
        doc: "Restate revenue stored at the 1:1 FX fallback once a real rate exists; previews by default.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_margin",
        method: Method::Get,
        path: "/v1/margin",
        access: Admin,
        params: &[
            qe("by", BY_DIM, "group dimension (default customer)"),
            q("project", ""),
            q("since", "RFC3339 window start (default 30d ago)"),
            q("until", "RFC3339 window end (default now)"),
            qt(
                "below",
                JsonTy::Number,
                "at-risk cohort: keep only rows whose margin% is under this (e.g. 0 = loss-making)",
            ),
        ],
        response: TypeRef::Untyped(
            "{ dimension, since, until, total_revenue_usd, total_cost_usd, total_margin_usd, \
             unconverted_currencies?, currency_note?, below?, rows: [{key, revenue_usd, \
             llm_cost_usd, gross_margin_usd, margin_pct, calls, guardrail?}] } — when `below` is \
             echoed, the totals are the filtered cohort's, not the window's.",
        ),
        mcp: Some(McpTool {
            name: "get_margin",
            description: "Profit rollup: revenue − LLM cost grouped by customer or product over a window (default last 30 days). Most-unprofitable first.",
            args: &["by", "project", "since", "until"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["margin"]),
        render_kind: Some("get_margin"),
        doc: "Revenue minus LLM cost by customer or product over a window.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_margin_trend",
        method: Method::Get,
        path: "/v1/margin/trend",
        access: Admin,
        params: &[
            qe("by", BY_DIM, "group dimension (default customer)"),
            q("project", ""),
            qt("days", JsonTy::Integer, "trailing window length (default 30, clamped to 1..=365)"),
            qt("top", JsonTy::Integer, "max keys by |total margin| (default 20)"),
        ],
        response: TypeRef::Untyped(
            "{ dimension, since, until, days, key_count, top_n, totals: {…dense daily series…}, \
             series: [{key, …per-day revenue/cost/margin…}] } — `key_count` is the pre-cap key \
             count, so a client can say \"showing top_n of key_count\".",
        ),
        cli: Some(&["margin", "trend"]),
        render_kind: Some("get_margin_trend"),
        doc: "Per-day revenue/cost/margin series for the top keys of a dimension.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_customer_margin",
        method: Method::Get,
        path: "/v1/margin/customer/:id",
        access: Admin,
        params: &[
            p("id", "customer id"),
            q("project", ""),
            q("since", "RFC3339 window start (default 30d ago)"),
            q("until", "RFC3339 window end (default now)"),
        ],
        response: TypeRef::Untyped(
            "{ customer_id, since, until, revenue_usd, cost_usd, margin_usd, margin_pct, \
             by_model: [{key, cost_usd, calls}], by_name: […] } — one customer's window, with the \
             cost split by `provider/model` and by use-case name, dearest first.",
        ),
        cli: Some(&["margin", "customer"]),
        render_kind: Some("get_margin_customer"),
        doc: "One customer's windowed revenue and cost, split by model and by use-case.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_margin_simulate",
        method: Method::Get,
        path: "/v1/margin/simulate",
        access: Admin,
        params: &[
            qe("by", BY_DIM, "group dimension (default customer)"),
            q("project", ""),
            qt("price_per_mtok", JsonTy::Number, "hypothetical charge per 1M prompt+completion tokens"),
            qt("flat_monthly", JsonTy::Number, "hypothetical flat charge per key per 30-day month"),
            q("since", "RFC3339 window start (default 30d ago)"),
            q("until", "RFC3339 window end (default now)"),
        ],
        response: TypeRef::Untyped(
            "{ simulated: true, dimension, since, until, assumptions, total_actual_margin_usd, \
             total_simulated_margin_usd, total_margin_delta_usd, unconverted_currencies?, \
             currency_note?, rows: [{key, actual_margin_usd, simulated_margin_usd, …}] } — \
             read-only; at least one of the two price parameters is required (else 400).",
        ),
        cli: Some(&["margin", "simulate"]),
        render_kind: Some("get_margin_simulate"),
        doc: "Pricing what-if: margin recomputed under a hypothetical price model; nothing is stored.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "billing_webhook",
        method: Method::Post,
        path: "/v1/billing/:provider/webhook",
        // The provider's HMAC signature IS the credential here — verified by the configured
        // `BillingSource`, never by a LightTrack bearer key. `ROUTE_SCOPES` says `Admin` only
        // because its two-column shape cannot say "authenticated by someone else's secret".
        access: Unauthenticated,
        machine: true,
        mutating: true,
        params: &[
            p("provider", "configured billing provider, e.g. `stripe` or `polar`"),
            qr(
                "project",
                "the LightTrack project the revenue lands in; configure one endpoint per project",
            ),
        ],
        response: TypeRef::Empty,
        doc: "Signed Stripe/Polar webhook door: a verified delivery becomes revenue in one atomic batch.",
        ..Endpoint::DEFAULT
    },
];
