//! The Collective Model Intelligence Network: this instance's privacy-safe digest, the hub's ingest
//! door, the merged leaderboard, and the two sides of consent — contribute, and withdraw.

use crate::dsl::*;
use crate::types::*;
use Access::*;
use KeyScope::*;

pub(crate) const ENDPOINTS: &[Endpoint] = &[
    Endpoint {
        id: "get_collective_digest",
        method: Method::Get,
        path: "/v1/collective/digest",
        access: Key(Read),
        params: &[qt(
            "min_cases",
            JsonTy::Integer,
            "k-anonymity floor: only (model, task) buckets with ≥ this many cases are published",
        )],
        response: TypeRef::Named("CollectiveDigest"),
        mcp: Some(McpTool {
            name: "get_collective_digest",
            description: "This instance's privacy-safe, k-anonymized model digest — the aggregate scorecards it would contribute to a collective hub (admin key required). Never reads raw events.",
            args: &["min_cases"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["collective", "digest"]),
        render_kind: Some("get_collective_digest"),
        doc: "Preview this instance's contributable digest, built only from consenting projects' runs.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_collective_ingest",
        method: Method::Post,
        path: "/v1/collective/ingest",
        access: Key(Ingest),
        // Hub side of a server-to-server push: `POST /v1/collective/contribute` on a *contributing
        // instance* is what calls this, with the hub-issued key. No human or agent drives it.
        machine: true,
        mutating: true,
        body: Some(TypeRef::Named("CollectiveDigest")),
        response: TypeRef::Untyped(
            "{ contributor_id, accepted, skipped, dropped_under_min, rejected_implausible } — the \
             identity is the HUB's (derived from the credential, never from the body), and every \
             drop is disclosed back rather than silently absorbed.",
        ),
        doc: "Hub door: accept a contributor's digest and replace that source's stored entry set.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_collective_leaderboard",
        method: Method::Get,
        path: "/v1/collective/leaderboard",
        access: Key(Read),
        params: &[
            q("task_type", "filter to one task bucket (qa, summarization, coding, …)"),
            q("provider", "filter to one provider (anthropic, openai, …)"),
            q("judge", "keep rows scored (at least partly) by one judge family"),
            q(
                "determinism",
                "rigor filter: every source ran at this level (exact|best-effort|sampled)",
            ),
            qt("frozen_dataset", JsonTy::Boolean, "rigor filter: every source used a frozen dataset"),
            qt("significance_tested", JsonTy::Boolean, "rigor filter: every verdict was significance-tested"),
        ],
        response: TypeRef::Untyped(
            "{ contributors, n_models, n_rows, held_back, task_type?, rows: [LeaderboardRow] } — \
             counts are computed over the FILTERED rows, and `held_back` discloses the rows \
             withheld for having fewer than the hub's `min_contributors` distinct sources.",
        ),
        mcp: Some(McpTool {
            name: "get_collective_leaderboard",
            description: "The collective real-world model leaderboard: quality × cost × latency per (provider, model, task type), merged across contributing LightTrack instances. Optionally filter by task_type or provider.",
            args: &["task_type", "provider", "determinism", "frozen_dataset", "significance_tested"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["collective", "leaderboard"]),
        render_kind: Some("get_collective_leaderboard"),
        doc: "The merged cross-contributor model leaderboard, k-anonymized over sources.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "withdraw_contribution",
        method: Method::Delete,
        path: "/v1/collective/contribution",
        access: Key(Ingest),
        mutating: true,
        idempotent: true,
        params: &[
            q("contributor", "admin-only: withdraw a NAMED source (the one that lost its key)"),
            q("all", "flip the route around: withdraw what WE sent to every ledgered hub (admin)"),
            q("hub", "with `all`: a hub base URL to consider, repeatable"),
            q("hub_key_ref", "with `all`: the NAME of the env var holding the hub key, never the key"),
        ],
        response: TypeRef::Untyped(
            "{ contributor_id, deleted } for the hub-side self-delete; with `all=1`, the \
             contributor-side fan-out's per-hub report instead.",
        ),
        cli: Some(&["collective", "withdraw"]),
        doc: "Withdraw a source's contributed entries — consent stays revocable, not one-way.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "post_contribute",
        method: Method::Post,
        path: "/v1/collective/contribute",
        access: Admin,
        mutating: true,
        params: &[
            br("hub", JsonTy::String, "the hub's absolute http(s) base URL"),
            b(
                "hub_key_ref",
                JsonTy::String,
                "the NAME of a server-side env var holding the hub key — never the key itself",
            ),
            b("min_cases", JsonTy::Integer, "k-anonymity floor for the digest being built"),
            b("force", JsonTy::Boolean, "push even when the digest is unchanged since the last ack"),
        ],
        response: TypeRef::Untyped(
            "{ outcome: sent|rejected|failed|skipped, hub_url_hash, entries, projects_included, \
             projects_excluded, digest_sha256, contribution_id?, ack?, reason? } — a `skipped` push \
             writes no ledger row, because nothing left the building.",
        ),
        cli: Some(&["collective", "contribute"]),
        doc: "Build this instance's digest, hash-gate it, push it to a hub, and record the attempt.",
        ..Endpoint::DEFAULT
    },
    Endpoint {
        id: "get_contributions",
        method: Method::Get,
        path: "/v1/collective/contributions",
        access: Admin,
        paged: true,
        params: &[
            qt("limit", JsonTy::Integer, "max rows, newest first (default from server)"),
            q("cursor", "keyset cursor from a prior page's X-Next-Cursor"),
        ],
        response: TypeRef::ArrayOf("ContributionRecord"),
        mcp: Some(McpTool {
            name: "get_collective_contributions",
            description: "The contribution ledger (admin key required): every digest this instance has PUSHED to a collective hub — when, to which hub (an opaque hash, never the URL), how many buckets, how many projects consented vs were withheld, the digest's content hash, and the hub's verbatim ack. `status` is sent | rejected | failed: a rejection is a hub that answered and declined (its own min_interval, a bad credential), a failure is a push that never got an answer. Two rows sharing a `digest_sha256` were the same measurement re-sent. The digest BODY is never stored here — only the hash and the counts.",
            args: &["limit", "cursor"],
            ..McpTool::DEFAULT
        }),
        cli: Some(&["collective", "history"]),
        render_kind: Some("get_collective_contributions"),
        doc: "The contributor-side ledger: what we pushed, to which hub, and what it said back.",
        ..Endpoint::DEFAULT
    },
];
