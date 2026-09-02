//! Cost, price-book, margin and revenue verbs, plus the two deployment status doors.

use clap::{Args, Subcommand};

/// `lt costs`' own window, flattened so the bare rollup keeps working beside `lt costs prompts`.
#[derive(Args)]
pub(crate) struct CostsArgs {
    #[arg(long)]
    pub(crate) project: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum CostsCmd {
    /// Cost grouped by the `metadata.prompt` version tag — did v4 cost less than v3?
    Prompts {
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default: 30 days before `--until`).
        #[arg(long)]
        since: Option<String>,
        /// RFC3339 window end (default: now).
        #[arg(long)]
        until: Option<String>,
    },
}

/// `lt margin`'s own window, flattened so the bare report keeps working beside its subcommands.
#[derive(Args)]
pub(crate) struct MarginArgs {
    #[arg(long, default_value = "customer")]
    pub(crate) by: String,
    #[arg(long)]
    pub(crate) project: Option<String>,
    /// RFC3339 window start (default 30d ago).
    #[arg(long)]
    pub(crate) since: Option<String>,
    /// RFC3339 window end (default now).
    #[arg(long)]
    pub(crate) until: Option<String>,
}

#[derive(Subcommand)]
pub(crate) enum MarginCmd {
    /// Per-day revenue/cost/margin for the top keys of a dimension — is the erosion new?
    Trend {
        /// Group dimension: `customer` | `product`.
        #[arg(long, default_value = "customer")]
        by: String,
        #[arg(long)]
        project: Option<String>,
        /// Trailing window length (default 30, clamped to 1..=365).
        #[arg(long)]
        days: Option<i64>,
        /// Max keys by absolute total margin (default 20).
        #[arg(long)]
        top: Option<i64>,
    },
    /// One customer's window, with the cost split by model and by use-case, dearest first.
    Customer {
        id: String,
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default 30d ago).
        #[arg(long)]
        since: Option<String>,
        /// RFC3339 window end (default now).
        #[arg(long)]
        until: Option<String>,
    },
    /// Pricing what-if: margin recomputed under a hypothetical price model. Nothing is stored.
    Simulate {
        /// Group dimension: `customer` | `product`.
        #[arg(long, default_value = "customer")]
        by: String,
        #[arg(long)]
        project: Option<String>,
        /// Hypothetical charge per 1M prompt+completion tokens.
        #[arg(long = "price-per-mtok")]
        price_per_mtok: Option<f64>,
        /// Hypothetical flat charge per key per 30-day month.
        #[arg(long = "flat-monthly")]
        flat_monthly: Option<f64>,
        #[arg(long)]
        since: Option<String>,
        #[arg(long)]
        until: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum RevenueCmd {
    /// Record one revenue event by hand, so margin has the other half of the subtraction.
    Record {
        #[arg(long)]
        project: String,
        /// Non-negative magnitude; `--type refund` is what makes it subtract.
        #[arg(long)]
        amount: f64,
        /// The billing customer it is attributed to (joins to events' `metadata.customer_id`).
        #[arg(long)]
        customer: Option<String>,
        /// The billing product it is attributed to (joins to events' `metadata.product_id`).
        #[arg(long)]
        product: Option<String>,
        /// subscription | one_time (default) | usage | refund.
        #[arg(long = "type", default_value = "one_time")]
        kind: String,
        /// ISO-4217 code of `--amount` (default USD).
        #[arg(long, default_value = "USD")]
        currency: String,
        /// The billing system's own id for this record; the same one upserts rather than duplicates.
        #[arg(long = "external-id")]
        external_id: Option<String>,
        /// Where it came from (default `manual`).
        #[arg(long, default_value = "manual")]
        source: String,
        /// RFC3339 recognition instant (default now).
        #[arg(long)]
        ts: Option<String>,
    },
}

#[derive(Subcommand)]
pub(crate) enum PricesCmd {
    /// The rates in force today, one row per model.
    List,
    /// Models carrying traffic the price book could not cost, loudest first.
    ///
    /// While this list is non-empty, every cost, margin, forecast and limit number over the window
    /// is a FLOOR — those calls are stored with no cost at all, never a zero. Close a row with
    /// `PUT /v1/prices/<provider>/<model>?fill_unpriced=1`.
    Unpriced {
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default: 30 days ago).
        #[arg(long)]
        since: Option<String>,
    },
    /// Every stored rate for one model, newest first — what a call in a past window really cost.
    History { provider: String, model: String },
}

#[derive(Subcommand)]
pub(crate) enum IngestCmd {
    /// Load shedding on the ingest doors: in-flight depth, plus the shed and timeout counters.
    Status,
}

#[derive(Subcommand)]
pub(crate) enum StorageCmd {
    /// Disk per table and index, per-family latency, and which maintenance passes were DEFERRED.
    Status,
}
