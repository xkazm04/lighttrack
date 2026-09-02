---
subject: software-engineering/credential-vault
project: tracklight
raised_by: intake intake-portkey-0902 (peer comparison)
source: librarian/sources/2026-09-02-portkey-gateway.md
stage: price resolution — between the outbound provider call and the cost stamped on an event or a benchmark cell
size: 3 files / ~150 lines / M
status: proposed
---

## Why the scope implies it

`scope.does` names two clauses this sits between: *"ingest LLM telemetry"* and *"benchmark providers"*. Both terminate in a dollar figure, and both resolve that figure the same way — through a price book keyed by `(provider, model)` and nothing else. `PriceBook` is keyed `"<provider>/<model>"` (`crates\core\src\pricing.rs:64-70`); the persisted table `model_prices` has primary key `(provider, model)` (`schema\sqlite\001_init.sql:204-212`); the resolution walks effective dates and model variants (`pricing.rs:135-146`, `:195-226`) and never looks at who was calling.

That is correct when everyone pays list. It is silently wrong the moment they do not — and a self-hosted observability product's users are exactly the population that negotiates rates, runs on committed-spend discounts, or holds a provisioned-throughput contract. A leaderboard whose cost column was computed from public pricing, for an operator on a negotiated rate, is wrong by a fixed multiplier, and nothing in the product can see it: the number is internally consistent, the price book row is present and current, and `cost_source` reads `book` exactly as designed (`crates\api\src\redact.rs:213`). `docs\ARCHITECTURE.md` §6 already flags the softer version of this — *"Prices in the repo are approximate — verify against provider pricing pages"* — but that is a caveat about staleness, not about the axis being missing.

The peer models the missing axis directly. `conf.json.integrations[]` maps an opaque slug to `{provider, credentials, rate_limits[], models[{slug, status, pricing_config}]}` (`C:/t/portkey/conf.example.json:19-46`), so a credential carries **its own price book and its own permitted roster**. The enforcement point returns both a terminal response and a `modelPricingConfig` that then rides on the request's log object (`src/handlers/services/preRequestValidatorService.ts:20-31`; `src/handlers/handlerUtils.ts:402-427`, `:414-416`; `src/handlers/services/logsService.ts:37-40`) — the price travels with the credential, all the way to the record.

tracklight already owns the harder half of this shape, pointed inward. `LimitScope::ApiKey(String)` scopes a budget to one credential, keyed by the opaque `api_keys.id` — never the key material, never a prefix, never a hash of the secret — stamped server-side from the authenticated principal and stripped from whatever the body claimed (`crates\core\src\limits.rs:44-52`; `docs\ARCHITECTURE.md` §7a0). That is a better credential-as-identity design than the peer's. It is applied to tracklight's own ingest keys and has never been pointed at the provider credentials tracklight itself holds, which are raw env vars with no identity at all (`crates\engine\src\anthropic_api.rs:29`; `crates\engine\src\providers.rs:268-278`, `:356-365`).

A repo-wide search for a model allowlist attached to a credential returns nothing in either tree's corpus neighbourhood; the design record (§6 promoting questions, row D2) confirms the registry keys price resolution `(provider, model, input tokens, lane)` and never by credential.

## What the first context contains

**A credential identity, and price rows that may key on it.** A small `credentials` concept in `crates\core\src\` — a slug, a provider, and where the secret is read from — deliberately *not* a secret store: the secret keeps living in the environment, and the row holds only the reference. This is the shape `brokered-egress` describes and the shape `LimitScope::ApiKey` already implements on the other side of the service.

**Price resolution gains one optional axis.** `model_prices` grows an optional credential column; `PriceBook`'s lookup tries `(credential, provider, model)` and falls back to `(provider, model)`. Every existing row and every existing caller is the fallback path, unchanged. The precedence is one sentence and belongs in `docs\PRICING.md` next to the variant-row rules (`@in>N`, `@batch`, `@flex`), which are the in-tree precedent for extending resolution without a schema break (`crates\core\src\pricing.rs:66-70`, `:195-226`).

**The roster clause.** A credential may name the models it is permitted to call, with a status per model — the half of the peer's `models[]` that is not pricing. In tracklight this is a benchmark-time check, not a request-time one: `crates\runner\src\bench.rs:33-42` parses the target matrix, and a target naming a model this credential may not call is a pre-flight failure beside the cost estimate (`crates\runner\src\budget.rs:57-95`), not a 401 discovered at case 14 of 60.

**What it must NOT absorb.** Not secret storage or rotation mechanics — the env-var loader stays. Not `LimitScope`: `crates\core\src\limits.rs` owns spend caps on *inbound* traffic and is explicitly unrelated to the benchmark ceiling (`docs\BENCHMARK_FRAMEWORK.md` §5a says so in as many words); this module must not quietly become a second limit engine. Not the price book's existing depth — effective dates, tiers, variants, the admin `PUT` (`crates\api\src\main.rs:410-411`) — all of which stay exactly as they are and are strictly better than what the peer ships.

## The measurable

**The error in a reported cost for an operator on a non-list rate: currently a silent fixed multiplier, target 0 — and, when unknown, named rather than assumed.**

The falsifiable version, because "wrong by an unknown factor" is not measurable on its own: after the change, a benchmark run's report carries which price row resolved each cell — `(credential, provider, model)` or the `(provider, model)` fallback — the same way `cost_source` already distinguishes `client` from `book` (`crates\api\src\redact.rs:213`). The number that moves is **the share of priced cells whose price came from a row the operator asserted, rather than from the seeded public book** (`config\pricing.json`, loaded once at boot per `crates\api\src\main.rs:243-254`). Today that share is structurally 0. An operator who configures nothing keeps 0 and loses nothing.

Measured by a case in the existing `crates\core\src\pricing.rs` test module plus one in `crates\api\src\tests_ingest.rs` (29 cases): an event attributed to a credential with a negotiated row prices against that row; an event with no credential prices against the public row; both are labelled.

## What would make this wrong

**If every tracklight operator pays list.** This is the falsifier and it is cheap to check: ask. If no instance in the fleet has a negotiated rate, the axis is machinery for a case that does not occur, and the honest outcome is `deferred` with "an operator reports a negotiated rate" as the return condition.

**If the roster is the real want and the pricing is not.** The two clauses arrive together in the peer because a hosted product bills by them together. Here they may separate: "this credential may not call the frontier model" is a benchmark pre-flight concern with immediate value, while the price axis needs a second party to have negotiated something. If the review splits them, the roster half is the smaller and should ship first — this proposal should then be re-scoped, not partially executed.

**If the credential axis leaks into the leaderboard's comparability.** The collective model-intelligence network pools runs across contributors (`docs\BENCHMARK_FRAMEWORK.md` §6). A cost column computed from private negotiated rates is not comparable across instances, and publishing it as though it were would corrupt the shared digest — a worse failure than the one this proposal fixes. Whatever lands must decide, before the schema changes, whether the shared digest carries list-priced cost or is dropped; if it cannot, this is wrong as designed.
