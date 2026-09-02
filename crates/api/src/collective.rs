//! Collective Model Intelligence Network — the opt-in network-effect surface.
//!
//! Three endpoints, mirroring the design in `docs/BENCHMARK_FRAMEWORK.md`:
//! - `GET  /v1/collective/digest` — build *this* instance's privacy-safe digest from its own benchmark
//!   run scorecards (admin; a preview of what it would contribute). Never reads `events`.
//! - `POST /v1/collective/ingest` — a hub receives a digest from a contributor and stores it (gated by
//!   `LIGHTTRACK_COLLECTIVE_ACCEPT`; off by default).
//! - `GET  /v1/collective/leaderboard` — the merged public leaderboard across all contributors.
//!
//! Privacy lives in `core::collective`: digests are aggregate-only and k-anonymized; the contributor
//! id is an opaque, non-reversible hash so a hub can update a source idempotently without learning who
//! it is.
//!
//! ## Where things live
//! One module per route, each paired with the pure policy it delegates to:
//! - [`config`] — the env-derived [`Collective`] knobs every handler reads.
//! - [`identity`] — who a contribution belongs to (shared by the two write routes).
//! - [`digest`] (`GET /digest`) + [`scorecard`] — which projects consent, and how one run becomes a stat.
//! - [`ingest`] (`POST /ingest`) + [`sanitize`] — the store transaction, and what a hub will believe.
//! - [`leaderboard`] (`GET /leaderboard`) — merge, k-anonymity over sources, then filters.
//! - [`withdraw`] (`DELETE /contribution`) — the right to revoke a contribution, plus
//!   [`withdraw_all`] (`?all=1`), the contributor-side fan-out across every ledgered hub.
//! - [`contribute`] (`POST /contribute`) + [`ledger`] (`GET /contributions`) — the push that
//!   records itself, and the record.

mod config;
mod contribute;
mod digest;
mod identity;
mod ingest;
mod leaderboard;
mod ledger;
mod sanitize;
mod scorecard;
mod withdraw;
mod withdraw_all;

pub(crate) use config::Collective;
pub(crate) use contribute::post_contribute;
pub(crate) use digest::get_digest;
pub(crate) use ingest::post_ingest;
pub(crate) use leaderboard::get_leaderboard;
pub(crate) use ledger::get_contributions;
pub(crate) use withdraw::delete_contribution;
