//! Cost and pricing: the fixed cost rollups, the unpriced-traffic ledger, per-version quality, the
//! `/v1/rollup` primitive they are all a grouping of, the price book, and the forecast.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "get_costs",
        method: Method::Get,
        path: "/v1/costs",
        access: Key(Read),
        params: &[
            q("project", ""),
            q("since", "RFC3339 window start (inclusive); omit for full history"),
            q("until", "RFC3339 window end (exclusive)"),
        ],
        response: TypeRef::Untyped(
            "[{ project_id, provider, model, calls, input_tokens, output_tokens, cost_usd, \
             unpriced_calls }] — cost grouped by project×provider×model. The price book's freshness \
             rides in `x-price-book-verified-at` / `x-price-book-stale` because the body is a bare \
             array the render layer is written against.",
        ),
        mcp: Some(McpTool {
            name: "get_cost_summary",
            description: "Cost/usage rollup grouped by project + provider + model. Optionally filter by project.",
            args: &["project"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["costs"]),
        render_kind: Some("get_cost_summary"),
        doc: "Cost and usage grouped by project, provider and model over an optional window.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_prompt_costs",
        method: Method::Get,
        path: "/v1/costs/prompts",
        access: Key(Read),
        params: &[
            q("project", ""),
            q("since", "RFC3339 window start (inclusive); defaults to 30 days before `until`"),
            q("until", "RFC3339 window end (exclusive); defaults to now"),
        ],
        response: TypeRef::ArrayOf("CostByDimension"),
        cli: Some(&["costs", "prompts"]),
        doc: "Cost grouped by the `metadata.prompt` version tag — did v4 cost less than v3?",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_unpriced_models",
        method: Method::Get,
        path: "/v1/costs/unpriced",
        access: Key(Read),
        params: &[
            q("project", ""),
            q("since", "RFC3339 window start (default: 30 days ago)"),
        ],
        response: TypeRef::Untyped(
            "{ since, models: [{provider, model, calls, …}], unpriced_calls, notes, price_book: \
             {verified_at, stale, …} } — the pairs carrying traffic the book cannot cost, ranked by \
             calls, beside the freshness of the rates that did apply.",
        ),
        mcp: Some(McpTool {
            name: "list_unpriced_models",
            description: "Which (provider, model) pairs carried traffic the price book could NOT cost, ranked by call count. Those calls are stored with no cost at all — never a zero — so while this list is non-empty EVERY cost, margin, forecast and limit number over the window is a floor, not a total. Check it before reporting a spend figure. Closing a row is `PUT /v1/prices/{provider}/{model}?fill_unpriced=1` (admin, not exposed here). The response also carries `price_book.stale`: rates nobody has re-verified recently are their own reason to distrust a cost number.",
            args: &["project", "since"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["prices", "unpriced"]),
        render_kind: Some("list_unpriced_models"),
        doc: "Which models carried traffic the price book could not cost, loudest first.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_prompt_quality",
        method: Method::Get,
        path: "/v1/quality/prompts",
        access: Key(Read),
        params: &[
            Param {
                            name: "project",
                            doc: "",
                            // An MCP caller has no project key to derive this from.
                            mcp_required: Some(true),
                            ..Param::DEFAULT
                        },
            q("since", "RFC3339 lower bound on the VERDICT time (default 7 days ago)"),
            q("until", "RFC3339 upper bound on the verdict time"),
            q(
                "rubric_id",
                "narrow to one rubric — the only way two versions are compared on the same criteria",
            ),
        ],
        response: TypeRef::Untyped(
            "[{ tag, name?, version?, n, mean, pass_rate, ci95_low, ci95_high, cost_usd }] — one \
             row per served version, newest first, with the untagged bucket last.",
        ),
        mcp: Some(McpTool {
            name: "get_prompt_quality",
            description: "How each SERVED prompt version is actually scoring in production: mean, pass rate, ~95% interval and n per `metadata.prompt` tag. The quality half of the cost-per-version read — use it to decide whether a promotion held up.",
            args: &["project", "since", "until", "rubric_id"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["prompts", "quality"]),
        render_kind: Some("get_prompt_quality"),
        doc: "How each served prompt version is scoring in production, with n and a ~95% interval.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_usecases",
        method: Method::Get,
        path: "/v1/usecases",
        access: Key(Read),
        params: &[
            Param {
                            name: "project",
                            doc: "",
                            // An MCP caller has no project key to derive this from.
                            mcp_required: Some(true),
                            ..Param::DEFAULT
                        },
            q("since", "RFC3339 window start (inclusive); omit for full history"),
        ],
        response: TypeRef::Untyped(
            "[{ name, provider, model, calls, input_tokens, output_tokens, cost_usd, \
             unpriced_calls }] — usage and cost per use-case; `name` is null for unnamed calls.",
        ),
        mcp: Some(McpTool {
            name: "get_usecases",
            description: "Use-case cost rollup: usage + cost grouped by (name, provider, model) for a project, optionally windowed from `since`. A call's use-case is its `name`, or its model when unnamed.",
            args: &["project", "since"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["usecases"]),
        render_kind: Some("get_usecases"),
        doc: "Usage and cost grouped by use-case name, provider and model.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_rollup",
        method: Method::Get,
        path: "/v1/rollup",
        access: Key(Read),
        params: &[
            q("project", ""),
            q(
                "by",
                "comma-separated dimensions, 1..=3 (default provider,model): \
                 project|provider|model|name|api_key|customer|product|prompt|day",
            ),
            q("since", "RFC3339 window start (default 30d ago)"),
            q("until", "RFC3339 window end (exclusive)"),
            qe(
                "time",
                &["ts", "received_at"],
                "which timestamp the window and `day` bucket read (default ts; accounting reads use received_at)",
            ),
            q(
                "filter",
                "comma-separated `dimension:value` equality predicates, e.g. customer:acme,model:gpt-5.4",
            ),
        ],
        response: TypeRef::Untyped(
            "{ group_by: [dimension], time_key, rows: [{keys, calls, tokens, cost_usd, \
             unpriced_calls, …}] } — the grouping is echoed so `keys` reads positionally.",
        ),
        mcp: Some(McpTool {
            name: "query_rollup",
            description: "THE grouped cost/usage question: totals over a window, grouped by 1-3 of project/provider/model/name/api_key/customer/product/prompt/day, with optional equality filters. Every fixed cost surface (costs, usecases, margin, forecast) is one grouping of this — use it for anything they do not already answer, e.g. \"cost per customer per day\" or \"which model drives this product's spend\". Rows carry `unpriced_calls`: when it is non-zero the cost is a FLOOR, not a total, because those calls had no price in the book.",
            args: &["project", "by", "since", "until", "time", "filter"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["rollup"]),
        render_kind: Some("query_rollup"),
        doc: "The grouped cost/usage primitive every fixed cost surface is one grouping of.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_prices",
        method: Method::Get,
        path: "/v1/prices",
        access: Key(Read),
        response: TypeRef::ArrayOf("ModelPriceRow"),
        mcp: Some(McpTool {
            name: "list_prices",
            description: "List the DB-backed model price book (the rate currently in force per model). Rows carry `effective_from` and `verified_at`: the book is a dated timeline, not one row per model.",
            ..McpTool::DEFAULT
        }),
        cli: Some(&["prices", "list"]),
        render_kind: Some("list_prices"),
        doc: "The rate in force today for every model in the DB-backed price book.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "list_price_history",
        method: Method::Get,
        path: "/v1/prices/history/:provider/:model",
        access: Key(Read),
        params: &[p("provider", "provider id"), p("model", "model id")],
        response: TypeRef::ArrayOf("ModelPriceRow"),
        mcp: Some(McpTool {
            name: "list_price_history",
            description: "Every stored rate for one model, newest first — the price timeline. Use it to answer what a call in a PAST window actually cost, which the current book cannot tell you.",
            args: &["provider", "model"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["prices", "history"]),
        render_kind: Some("list_price_history"),
        doc: "One model's price timeline, newest first — what a call in a past window really cost.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "put_price",
        method: Method::Put,
        path: "/v1/prices/:provider/:model",
        access: Admin,
        mutating: true,
        idempotent: true,
        params: &[
            p("provider", "provider id"),
            p("model", "model id"),
            br("input_per_mtok", JsonTy::Number, "input rate, USD per million tokens"),
            br("output_per_mtok", JsonTy::Number, "output rate, USD per million tokens"),
            b("cached_input_per_mtok", JsonTy::Number, "cached-input rate, USD per million tokens"),
            b("source_url", JsonTy::String, "where the rate was read from"),
            b(
                "effective_from",
                JsonTy::String,
                "when the rate takes effect (date or RFC3339); defaults to now",
            ),
            b("verified_at", JsonTy::String, "when a human last checked it against the vendor"),
            b("note", JsonTy::String, "free-text: why the rate changed, a ticket id, a caveat"),
            q(
                "fill_unpriced",
                "`1`/`true` to price this key's stored `cost_usd IS NULL` rows from the new rate",
            ),
            q("since", "how far back the post-fill recount looks (default 90 days)"),
        ],
        response: TypeRef::Untyped(
            "The stored ModelPriceRow, flattened, plus `filled` and `remaining_unpriced` when a fill \
             ran — `null` (absent) when none was asked for, which is not the same as `0`.",
        ),
        mcp: Some(McpTool {
            name: "put_price",
            description: "Upsert a model's price (per-million-token rates); hot-swaps the live price book. Idempotent.",
            read_only: false,
            idempotent: true,
            args: &[
                "provider",
                "model",
                "input_per_mtok",
                "output_per_mtok",
                "cached_input_per_mtok",
                "source_url",
            ],
        }),
        cli: Some(&["prices", "set"]),
        doc: "Upsert one model's rate, hot-swapping the live book and optionally pricing its history.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_forecast",
        method: Method::Get,
        path: "/v1/forecast",
        access: Key(Read),
        params: &[
            Param {
                            name: "project",
                            doc: "required for an admin key; a project key derives it",
                            // An MCP caller has no project key to derive this from.
                            mcp_required: Some(true),
                            ..Param::DEFAULT
                        },
            qe("by", &["customer", "product"], "margin dimension (default customer)"),
            qt("horizon", JsonTy::Integer, "days to project ahead (default 14, 1..=90)"),
            qt(
                "lookback",
                JsonTy::Integer,
                "trailing days of history to fit (default 14, clamped to 4..=90 — below the evidence floor a trend cannot be presented)",
            ),
        ],
        response: TypeRef::Untyped(
            "{ project_id, generated_at, dimension, horizon_days, lookback_days, spend, budgets, \
             margins, alerts, refused: [{subject, reason}] } — a projection under the evidence \
             floor is withheld and named in `refused[]` rather than guessed.",
        ),
        mcp: Some(McpTool {
            name: "get_forecast",
            description: "Predictive cost/margin forecast for a project: projected spend, per-budget breach ETAs (\"will breach in ~N days\"), per-customer/product margin-erosion crossovers (\"turns unprofitable next week\"), and the pre-emptive alerts derived from them. Fits an EWMA/linear trend over the recent daily counters. The forecast REFUSES rather than guesses: a projection built on too little history is withheld (its ETA is null) and named in `refused[]` with the reason (\"4 observed days needed, 2 seen\"), so an empty `alerts` with a non-empty `refused` means 'not enough evidence', not 'all is well'. `confidence` is the fit's r², withheld under the same floor.",
            args: &["project", "by", "horizon", "lookback"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["forecast"]),
        render_kind: Some("get_forecast"),
        doc: "Projected spend, budget-breach ETAs and margin-erosion crossovers for a project.",
        ..Endpoint::DEFAULT
    },
];
