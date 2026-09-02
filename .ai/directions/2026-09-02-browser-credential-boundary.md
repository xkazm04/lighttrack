---
subject: software-engineering/browser-credential-boundary
project: tracklight
raised_by: intake intake-portkey-0902 (peer comparison)
source: librarian/sources/2026-09-02-portkey-gateway.md
stage: process boot and the operator-API middleware stack, before any route is served
size: 3 files / ~120 lines / M
status: proposed
---

## Why the scope implies it

`scope.does` says tracklight is here to *"serve an operator API"*, and `scope.does_not` narrows the surface to exactly that — *"end-user product UI beyond an operator dashboard"*. An operator API is therefore a declared deliverable, and this instance holds every provider credential it benchmarks with: `ANTHROPIC_API_KEY` (`crates\engine\src\anthropic_api.rs:29`), the Gemini three-name fallback chain (`crates\engine\src\providers.rs:268-278`), `OPENAI_API_KEY` (`:356-365`), plus billing webhook secrets (`crates\billing\src\registry.rs:17-30`) and every project's ingest key material.

The scope admits the subject because the forces are all present at once: a self-hosted process, credentials in memory, and a surface an operator reaches over the network. What is absent is the boundary. `AuthMode::from_env` returns `Dev` for any unset or misspelled value (`crates\api\src\auth.rs:26-29`); in `Dev` a request with **no** bearer token resolves to `Principal::Dev` (`crates\api\src\guards.rs:51-54`) and a request with **any unrecognized** token resolves to `Principal::Dev` as well (`:87-89`); and `ensure_can_admin` treats `Dev` as admin-equivalent on every mutating route (`guards.rs:93-98`). The service's answer to this is a stderr banner (`crates\api\src\auth.rs:40-58`, printed from `crates\api\src\main.rs:340`), and then it starts.

The peer's answer is that a debug surface holding credentials has two legal states, not three: `src/middlewares/adminAuth/index.ts:8-19` in `C:/t/portkey` **throws at startup** — *"Admin UI auth requires conf.json.admin_token. Set admin_token or start the gateway with --headless."* Open-and-reachable is not a configuration you can reach; the third state is no surface at all. The same file allowlists what the surface may emit rather than denylisting what it may not: six provider-option keys survive and everything else becomes `[REDACTED]` (`src/middlewares/log/index.ts:20-37`), with all request headers redacted by key (`:18`). tracklight already applies that rule where it constructs a payload itself — `METADATA_PASSTHROUGH`, five keys, everything else dropped (`crates\api\src\redact.rs:215-221`) — and does not apply it at the emission boundary; `crates\api\src\logging.rs:26-53` configures structured logs with no field pass at all.

## What the first context contains

A `credential-boundary` module in `crates\api\src\`, holding two mechanisms and nothing else.

**The boot gate.** A function called from `crates\api\src\main.rs` before `build_router` (`:365`) that resolves `(auth_mode, admin_key)` and returns `Result`. Unenforced-and-no-key is an error, not a banner. The escape is explicit and named — the shape portkey chose is a `--headless` flag, and the tracklight equivalent is an opt-out env var whose value is a sentence, not `1`, so it cannot be set by accident. `crates\agent\src\config.rs:57-70` is the in-tree precedent: it `bail!`s on an empty `sources` list and eagerly resolves every device-key env var at load "so a missing secret fails at startup, not on the first lease" (`:65-68`). The API server is the binary holding the credentials and it is the one without that check.

**The emission allowlist.** A `sanitize` pass over anything the process emits that it constructed itself — the log fields configured at `crates\api\src\logging.rs:26-53` first, and any future operator stream second. Named keys pass; everything else is replaced. The generalization of `redact.rs:215-221`, applied at the outbound seam instead of the ingest seam.

**What it must NOT absorb.** Not the PII scrubber: `crates\api\src\redact.rs` and `lighttrack-anon` own free text a *caller* sent, where a denylist is the only option and `docs\DECISIONS.md` D14 records the precision cost of that at length. The rule this module owns is the complement — a payload this service builds itself is allowlisted — and stating the discriminator is half its value. Not the auth mechanism: `crates\api\src\guards.rs` keeps principal resolution, constant-time compare (`:58-64`) and the throttle wiring (`crates\api\src\main.rs:515-518`); this module decides only whether the process may boot into the posture those produce. Not `Principal::Dev` itself — zero-config first-run stays, it just stops being reachable over a network without a named opt-out.

## The measurable

**The count of configurations in which the operator API is both reachable and unauthenticated: currently ≥1 (the default), target 0.**

Measured by a case in `crates\api\src\tests_dev_mode.rs` (4 cases today): with `LIGHTTRACK_AUTH_MODE` and `LIGHTTRACK_ADMIN_KEY` both unset, router construction returns `Err` with a named message; with the opt-out set, it returns `Ok` and the existing four cases still pass unchanged.

Second number, for the allowlist half: **fields emitted by default when a new field is added to a logged struct — currently all, target none.** Measured by a fixture in `crates\api\src\redact.rs`'s test module (`:472-852`) that adds a field and asserts it renders redacted until it is named.

## What would make this wrong

**If every tracklight instance is bound to loopback.** The forces here assume "self-hosted defaults to reachable". If the deployed reality is that `deploy/` binds `127.0.0.1` and every remote instance already sets `LIGHTTRACK_AUTH_MODE=enforced`, then the banner at `crates\api\src\auth.rs:40-58` is already sufficient and this proposal converts a warning into friction on the one path — a laptop first run — the warning was written to protect. `deploy/` and the Helm chart's bind address are the files that settle it, and they should be read before this is accepted.

**If the opt-out becomes the default.** A boot gate that everyone silences is a worse artifact than the honest banner it replaced, because the banner at least reads. If the opt-out appears in `deploy/` or in the README quickstart, the change has failed and should be reverted rather than tuned.

**If the allowlist collides with debuggability.** `crates\api\src\logging.rs` exists so an operator can diagnose an ingest problem. If a first pass at the allowlist makes the ingest logs unusable, the answer is that the allowlist was drawn at the wrong altitude — over whole structs rather than over the credential-bearing ones — not that the rule is wrong.
