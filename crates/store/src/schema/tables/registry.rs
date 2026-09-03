//! The prompt registry, the price book, revenue and the margin guardrails.

use super::super::model::{Column as C, Index as I, Kind::*, Table};

pub static PROMPTS: Table = Table::new(
    "prompts",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("name", Text).nn(),
        C::new("benchmark_id", Text).doc("linked benchmark; its regression check gates promotion"),
        C::new("labels", Json)
            .nn()
            .def("'{}'")
            .doc("JSON object: label -> version (e.g. {\"production\": 3})"),
        C::new("created_at", Ts).nn(),
        C::new("updated_at", Ts).nn(),
        C::new("canary", Json)
            .added("M23")
            .doc("JSON CanaryPolicy; NULL = the registry still stops observing at promotion"),
        C::new("label_history", Json).added("M23").doc(
            "JSON array of LabelChange; NULL = no label move recorded here, which is why an \
             auto-revert with no recorded predecessor does nothing instead of guessing.",
        ),
    ],
)
.doc(
    "Named, versioned prompts fetched at runtime by label (e.g. production | staging). Cutting a \
     new version auto-enqueues the linked benchmark; promoting a label is blocked when that \
     benchmark's score regresses against its baseline.",
)
.unique(&["project_id, name"])
.indexes(&[I::new("idx_prompts_project", "project_id, name")]);

pub static PROMPT_VERSIONS: Table = Table::new(
    "prompt_versions",
    &[
        C::new("id", Text).pk(),
        C::new("prompt_id", Text).nn().refs_both("prompts(id)"),
        C::new("version", Int32).nn(),
        C::new("content", Text).nn(),
        C::new("config", Json).doc("JSON (model, params, variable schema)"),
        C::new("note", Text).doc("change note / \"commit message\""),
        C::new("created_at", Ts).nn(),
    ],
)
.doc("Immutable prompt versions (one row per cut). `version` is monotonic per prompt.")
.unique(&["prompt_id, version"])
.indexes(&[I::new("idx_prompt_versions_pid", "prompt_id, version")]);

pub static MODEL_PRICES: Table = Table::new(
    "model_prices",
    &[
        C::new("provider", Text).nn(),
        C::new("model", Text).nn(),
        C::new("input_per_mtok", Real).nn(),
        C::new("output_per_mtok", Real).nn(),
        C::new("cached_input_per_mtok", Real),
        C::new("effective_from", Ts).nn(),
        C::new("source_url", Text),
        C::new("verified_at", Ts),
        C::new("note", Text),
    ],
)
.doc(
    "DB-backed price book (source of truth; config/pricing.json is the seed). M26 made it a dated, \
     append-only timeline: the identity of a rate has to be (provider, model, effective_from), or a \
     correction overwrites the row that priced last quarter's traffic and no June cost number can \
     ever be defended. The migration off the pre-M26 shape is a named step, not an ALTER — SQLite \
     cannot change a primary key, and Postgres needs a guarded rename; see `schema::migrations`.",
)
.pk(&["provider", "model", "effective_from"]);

pub static REVENUE_EVENTS: Table = Table::new(
    "revenue_events",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("source", Text)
            .nn()
            .def("'manual'")
            .doc("stripe | polar | manual"),
        C::new("external_id", Text).doc("provider invoice/charge/order id (idempotency)"),
        C::new("customer_id", Text),
        C::new("product_id", Text),
        C::new("amount_usd", Real)
            .nn()
            .doc("non-negative magnitude; sign derived from kind"),
        C::new("currency", Text).nn().def("'USD'"),
        C::new("kind", Text)
            .nn()
            .def("'one_time'")
            .doc("subscription | one_time | usage | refund"),
        C::new("period_start", Ts).doc("subscription recognition window"),
        C::new("period_end", Ts),
        C::new("ts", Ts).nn(),
        C::new("amount_minor", Int).added("M9").doc(
            "FX provenance: `amount_usd` is derived and a wrong rate makes it wrong. The \
             provider's own minor-unit figure never needs restating, so keeping it — with the \
             rate, the book version behind it, and whether a real conversion happened — turns a \
             rate correction into a reprice instead of a re-ingest. All four are nullable.",
        ),
        C::new("fx_rate", Real).added("M9"),
        C::new("fx_book_version", Text).added("M9"),
        C::new("converted", Bool).added("M9"),
    ],
)
.doc(
    "Normalized revenue: the revenue analog of events' cost. Synced from a billing provider \
     (Stripe/Polar) or posted by hand; netted against LLM cost per customer/product.",
)
.indexes(&[
    I::new("idx_revenue_project_ts", "project_id, ts"),
    I::new("idx_revenue_customer", "customer_id"),
]);

pub static MARGIN_POLICIES: Table = Table::new(
    "margin_policies",
    &[
        C::new("id", Text).pk(),
        C::new("project_id", Text).nn(),
        C::new("trigger_json", Json).nn().doc(
            "PolicyTrigger: {\"below_pct\":20} | \"negative_margin\" | {\"erosion_eta_days\":5}",
        ),
        C::new("min_cost_usd", Real).nn().def("0"),
        C::new("action_json", Json).nn().doc(
            "PolicyAction: \"warn\" | {\"cap_to_revenue\":{\"factor\":0.8}} | \"throttle\" | \"block\"",
        ),
        C::new("cooldown_secs", Int).nn().def("3600"),
        C::new("expiry_secs", Int).nn().def("86400"),
        C::new("enabled", Int).nn().def("1"),
    ],
)
.doc(
    "Standing margin guardrails (M4): the policies the forecast sweep turns into limit rules. \
     `trigger` and `action` are JSON because both are open sum types that gain variants; this is \
     config read once per sweep, never on the ingest path, so schema stability beats shredded \
     columns.",
)
.indexes(&[I::new("idx_margin_policies_project", "project_id")]);
