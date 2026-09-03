# tracklight vs. `Portkey-AI/gateway` — peer design comparison

- **Source**: `Portkey-AI/gateway`, clone `C:/t/portkey`, pinned `669825cbe89ee51569918b8f78a9db486fd69dd4`
- **Design record**: `librarian/sources/2026-09-02-portkey-gateway.md` (intake run `intake-portkey-0902`, §3 design entries A1–A5 / B1–B3 / C1–C3 / D1–D3, §8 leads, §9 reusable engineering, §10 peer check)
- **Why this peer**: portkey is a self-hosted, multi-provider LLM infrastructure service with an operator surface that holds every provider credential in the installation. tracklight is the same class of system on the other side of one seam — a gateway *emits* what an observability service *ingests*, and tracklight additionally *calls* providers itself in the benchmark matrix. Every hard problem in portkey's request path has a mirror here.
- **Verdicts** come from the closed set `adopt` / `adapt` / `keep ours` / `different forces`. A `keep ours` carries its reason exactly as an `adopt` does.
- Nothing here is a task. This is input for the owner's direction pass; the four proposals it implies sit beside it in this directory.

**Verdict tally: 32 points — 8 `adopt`, 9 `adapt`, 11 `keep ours`, 4 `different forces`.**

---

## 1. Provider adapter interface and normalization — for accounting vs. for serving

This is the section with the discriminator in it, and the discriminator is the finding.

**1.1 — The adapter is a typed interface there and a `match` arm here.**
Portkey: an adapter is `{api, <endpoint>Config, <endpoint>ResponseTransform}` over three typed interfaces — `ParameterConfig` / `ProviderConfig` (`C:/t/portkey/src/providers/types.ts:19-42`), `ProviderAPIConfig` (`:45-81`), 78 directories behind them, one of which is 42 lines total (`C:/t/portkey/src/providers/anthropic/api.ts:1-42`).
tracklight: outbound dispatch is a four-arm string match — `C:\Users\kazda\kiro\tracklight\crates\engine\src\providers.rs:201-213`, ending `other => Err(EngineError::Other(format!("unknown provider '{other}'")))` at `:212`. Three hand-rolled generation functions behind it (`providers.rs:221`, `:269`, `:357`).
**Verdict: `keep ours`.** The declarative request map earns its keep at N=78, where 74 adapters are renames-plus-clamps. At N=3, with a `BenchTarget` that carries only `{provider, model, system_prompt, label}` (`crates\core\src\score.rs:186-195`), a param-map layer is indirection over three call sites. The transferable half is 1.2, not the map.

**1.2 — The typed escape hatch, and the terminal state for an unknown provider.**
Portkey: `RequestHandler` (`src/providers/types.ts:127-136`) lets Bedrock's SigV4 and Vertex's service-account auth bypass the transform path entirely rather than distorting it — the misfit is a declared shape, not a special case inside the generic path.
tracklight: `generate_once`'s `other =>` arm (`providers.rs:212`) is already the terminal state, and the `"anthropic" if anthropic_api::available()` guard at `:205` is already an escape hatch (API key present → Messages API; absent → the `claude -p` CLI, which has no sampling knobs at all).
**Verdict: `keep ours`.** Both properties portkey's interface buys are present here in eight lines. Nothing to import.

**1.3 — Two provider vocabularies that never meet.**
Portkey has one `providerOptions.provider` string threaded from header to adapter to log object.
tracklight has two disconnected type systems: `enum Provider { OpenAi, Anthropic, Google, Unknown }` for *ingest* (`crates\core\src\event.rs:10-17`, with `from_wire()` at `:26-33`), and a bare `&str` for *outbound* (`crates\engine\src\providers.rs:201`). A benchmark target's `provider: String` (`crates\core\src\score.rs:187`) is matched against the second and priced against the first (`crates\core\src\pricing.rs:64-70`, keyed `"<provider>/<model>"`).
**Verdict: `adapt`.** Not the interface — the *vocabulary*. A `BenchTarget` naming a provider the price book spells differently prices at `$0` and is reported as an unpriced model; `Provider::from_wire` already owns the canonicalization and the outbound path does not call it. This is a one-function change, and the test in §Tests below is what would prove it matters.

**1.4 — Normalization strictness as a per-request caller switch.** *(the discriminator)*
Portkey: `strict_open_ai_compliance` is a config-tree field read at 165 sites. Un-strict is the **passthrough** branch — `if (!strictOpenAiCompliance) return finishReason;` (`C:/t/portkey/src/providers/utils.ts:73-84`) — and the native structure rides *alongside* the normalized fields (`src/providers/anthropic/chatComplete.ts:597-601`, `content_blocks` beside `content`).
tracklight: `GenOutcome` (`crates\engine\src\lib.rs:261-273`) and `LlmEvent` (`crates\core\src\event.rs:122-189`) are single canonical models with no lossless side-channel and no caller switch.
**Verdict: `different forces` — and this is the sentence the corpus is missing.** Portkey's normalized payload *is the product*: a caller who chose the gateway to reach Anthropic's thinking blocks is destroyed by the loss, so lossiness has to be the caller's choice. tracklight's normalized payload is a *record* — every downstream capability (cost rollups, limit admission, leaderboards, the collective digest) consumes exactly one internal event model, and a caller-selectable schema there would produce two invoices for one call. Do **not** import C1. The finding is that "normalization for accounting" and "normalization for proxying" are opposite designs and the corpus states only the first.

**1.5 — Usage extraction is per-provider on both sides.**
Portkey: no generic walker; the shared cross-provider code is a pair of lookup tables (`src/providers/utils.ts:73-100`).
tracklight: Gemini's `usageMetadata.promptTokenCount` / `candidatesTokenCount` at `crates\engine\src\providers.rs:335-346`, OpenAI's `usage.prompt_tokens` / `completion_tokens` at `:411-426` — two hand-written extractors, no shared guesser.
**Verdict: `keep ours`.** Two independent systems converged on "one small explicit extractor per provider family". This is confirmation, not a transfer.

**1.6 — Provenance of the normalization is recorded here and nowhere there.**
tracklight stamps `Determinism::{Exact, BestEffort, Sampled}` on every outcome (`crates\engine\src\lib.rs:94-139`) — *what the provider actually honoured*, not what was asked for, because "a verdict is a measurement, so agreement should signal an ambiguous case, not sampling noise". Portkey records nothing equivalent; `strict_open_ai_compliance` silently collapses an unmappable finish reason to `stop` (`src/providers/utils.ts:73-84`) with no marker that a collapse happened.
**Verdict: `keep ours` — and this belongs on the inverse list.** tracklight's rule ("record what we got instead of claiming what we wanted") is the one portkey's strict mode violates.

---

## 2. Retries, budgets, retry-after

**2.1 — The ladder's defaults run opposite, and both are right.**
Portkey: `attempts ?? 0` — retries are **off** until an operator opts in, and `onStatusCodes` is the empty array unless attempts > 0 (`C:/t/portkey/src/handlers/services/requestContext.ts:148-155`).
tracklight: `MAX_TRIES: u32 = 3`, a compile-time constant, always on (`crates\engine\src\retry.rs:11`, loop at `:35-47`).
**Verdict: `different forces`.** A component that fans in every caller must not retry by default — its default *is* the fleet's amplifier. A benchmark worker calling providers on its own behalf, with a per-run dollar ceiling above it, is not that component. The honest note is that tracklight's 3 is not *configurable*, so an operator running a 10×60 matrix during a provider incident cannot turn it down; that is a knob, not a redesign.

**2.2 — Classification is by typed variant on both sides.**
Portkey branches on integer status lists. tracklight: `EngineError::is_retryable()` matches `RateLimited | ServerError | Timeout` (`crates\engine\src\retry.rs:18-25`), and the header comment states the rule — "never by string-matching provider messages" (`retry.rs:3-4`), with the status→variant map at `crates\engine\src\providers.rs:44-64` and the transport→variant map at `:67-78`.
**Verdict: `keep ours`.** tracklight's is the stronger form: portkey's `onStatusCodes` is operator-supplied integers, so `400` in a retry list retries a policy refusal.

**2.3 — tracklight emits the `Retry-After` header it refuses to read.** *(the strongest retry finding)*
Portkey: reads a stated delay from three ordered spellings with two unit systems — `['retry-after-ms', 'x-ms-retry-after-ms', 'retry-after']` (`C:/t/portkey/src/globals.ts:7`), the third `*1000` at `src/handlers/retryHandler.ts:118`; the stated delay then **zeroes the remaining ladder** (`retryHandler.ts:104-146`).
tracklight: `with_retry` sleeps a computed ladder and never inspects a response header — `crates\engine\src\retry.rs:35-47` takes `impl FnMut() -> Result<T>`, so the response is gone by the time the retry decision is made. Meanwhile tracklight *serves* `Retry-After` to its own clients on ingest overload (`crates\api\src\shed.rs:187-199`) and on limit breach (`crates\core\src\limits.rs:388-390`), and `ApiError` carries a `retry_after` field for exactly that (`crates\api\src\error.rs:121-138`).
**Verdict: `adopt`.** The dependency's own schedule outranks a guessed ladder; tracklight already believes this — it asserts it to its own callers — and does not apply it to the providers it calls. A 429 from Anthropic mid-matrix currently gets 200ms, 400ms, give up.

**2.4 — What happens when the stated wait exceeds the budget.**
Portkey: a 60-second whole-request budget (`src/globals.ts:5`); a stated delay `>=` the budget or `>` what remains **ends the ladder** rather than truncating the wait, spends zero further attempts, and reports its own terminal state (`retryHandler.ts:104-146`; surfaced as `retryCount = -1` at `src/handlers/handlerUtils.ts:1283-1288`).
tracklight: has the budget one level up — `Budget` with `exhausted()` / `spend()` / `halt()` (`crates\runner\src\budget.rs:100-153`), checked at a case boundary before spending (`crates\runner\src\compare.rs:107-113`) — but it is a *dollar* ceiling, and there is no wall-clock deadline the ladder is checked against at all.
**Verdict: `adapt`.** The collision is the same shape one level up: a stated wait that does not fit the remaining *run*. `crates\runner\src\budget.rs` already owns "spending is asked for, not discovered afterwards"; the wall-clock twin is the same structure. Note the fleet already solved the exact request-level version — `C:\Users\kazda\kiro\pumper\crates\engine-http\src\lib.rs:917-930` refuses to truncate a stated wait and returns a distinct `budget_exhausted` error instead, with the reasoning written out. That is the reference implementation to copy from, not portkey's.

**2.5 — Jitter.**
Portkey passes `randomize: false` to async-retry (`src/handlers/retryHandler.ts:169`) — no jitter at all, on the component that is by construction its fleet's correlator.
tracklight jitters every scheduled delay, without a `rand` dependency (`crates\engine\src\retry.rs:50-66`).
**Verdict: `keep ours` — inverse list.** The source is wrong here and tracklight is right; the design record banks this as a disproof-by-counterexample (§8 leads).

**2.6 — A blocking sleep inside a blocking client.**
tracklight's ladder is `std::thread::sleep` (`crates\engine\src\retry.rs:41`) under a process-wide `reqwest::blocking::Client` with a 10s connect / 30s request timeout (`crates\engine\src\providers.rs:22-40`). Portkey's is async with an `AbortController` (`src/handlers/retryHandler.ts:9-14`) because it is holding a client socket open.
**Verdict: `keep ours`.** A worker thread that owns its own cell has nobody waiting on the socket. This is also why 2.4's collision matters less here than there — and why it still matters: a 10-target matrix parks 10 threads.

**2.7 — Terminal states are already first-class here.**
Portkey enumerates a retry outcome as a status plus a `retryCount = -1` sentinel. tracklight's `halt_status()` returns `"cancelled" | "partial" | verdict_status` (`crates\runner\src\compare.rs:210-216`), and a halted run is `partial`, never `passed`, with `budget_halted` / `skipped_cases` / `cases_planned` on the report (`compare.rs:649-656`) and gate exit 4 for unverified (`docs\BENCHMARK_FRAMEWORK.md` §5a).
**Verdict: `keep ours`.** tracklight spells stopping better than the source does.

---

## 3. Breakers over a provider matrix

**3.1 — The breaker as a filter over the candidate list.**
Portkey: before the strategy switch, targets marked open are pruned from the candidate array — `C:/t/portkey/src/handlers/handlerUtils.ts:646-658`.
tracklight: **absent.** The benchmark matrix flattens every `(target × case)` cell into one `parallel_map` (`crates\runner\src\compare.rs:368-390`) and attempts every cell; a cell that errors sets `error_msg` and increments `errored` (`compare.rs:135`, `:182`, `:459-462`) and nothing changes for the next case against the same target. A provider that is down at case 1 is called again at cases 2..60.
**Verdict: `adopt`.** This is the decision tracklight will meet on the first incident-day benchmark, and it currently spends the whole matrix discovering it. It would live in `crates\runner\src\compare.rs` beside `compute_cell` (`:71-206`).

**3.2 — An all-open list is not an empty list.**
Portkey: the filter applies **only if at least one healthy target remains** — `if (healthyTargets.length) { currentTarget.targets = healthyTargets; }` (`handlerUtils.ts:655-657`). Refusing to route when every candidate looks sick converts a partial outage into a total one.
tracklight already holds this exact reasoning, one crate over: `investigate_sem: Arc::new(Semaphore::new(max_concurrent_investigations.max(1)))`, with the comment "At least one permit, so a misconfigured `0` doesn't wedge every investigation" (`crates\responder\src\breaker.rs:56-57`).
**Verdict: `adopt`, and the reason is already written in this tree.** Whatever 3.1 becomes has to carry the degenerate case with it, or it ships the outage it was built to survive.

**3.3 — The breaker tracklight does have is over a loop, not a candidate list.**
`crates\responder\src\breaker.rs:1-11` is admission control for the responder's two paid stages: in-flight dedup, per-project cooldown, rolling-hour spawn cap, global concurrency semaphore, RAII guard.
**Verdict: `keep ours`.** N breakers gating one call is a different shape from one breaker feeding N candidates, and the responder's version is the right one for the responder. Importing 3.1 must not disturb it.

**3.4 — A candidate's identity survives the pruning.**
Portkey threads `originalIndex` through the filter (`handlerUtils.ts:648-652`) and reads it back when building the leaf's jsonPath (`:665`, `:673`), so a leaf's log address is stable whether or not siblings were pruned.
tracklight's cells are addressed by `(target, case)` coordinates that a health filter would not renumber (`compare.rs:368-390`), and the leaderboard row is keyed by target label (`compare.rs:34-36`).
**Verdict: `keep ours`.** The hazard portkey solves does not exist in a coordinate-addressed matrix — worth stating so 3.1 is not implemented as an array filter that reintroduces it.

---

## 4. Streaming

**4.1 — The frame delimiter as a `(provider, endpoint)` lookup.**
Portkey: `getStreamModeSplitPattern(proxyProvider, requestURL)` (`C:/t/portkey/src/utils.ts:14-45`) — `\n\n` by default, `\r\n\r\n` for Anthropic `/complete` and Vertex's Google publishers, `\n` for Cohere's non-chat endpoints and DeepInfra, `\r\n` for Google; and Bedrock is not SSE at all, read by a hand-rolled binary reader over length-prefixed frames (`src/handlers/streamHandler.ts:38-130`).
tracklight: **streaming is absent end to end.** No `text/event-stream` anywhere; every provider response is buffered whole by `read_bounded` (`crates\engine\src\providers.rs:82-99`).
**Verdict: `different forces`.** A benchmark measures a settled answer — latency, tokens, a verdict — and a partial stream is not a smaller measurement, it is no measurement. Streaming the judge would buy nothing and cost the framing table.

**4.2 — Where a stream would appear here anyway.**
Not on the provider side: on the operator side. Portkey's `/log/stream` (`src/middlewares/log/index.ts`) is one producer to one surface. tracklight's nearest route is `GET /v1/events`, a paginated JSON query (`crates\api\src\main.rs:375-380`), and `GET /v1/ingest/status`, point-in-time counters (`:387`).
**Verdict: `adapt`.** If §6.1 lands, its carrier is a single-producer SSE in `crates\api` — which needs the corpus's *stateless-per-frame* parser, not portkey's framing table. Naming that keeps the two apart.

**4.3 — Bounded bodies.**
Portkey caps only what it *logs*: `MAX_RESPONSE_LENGTH = 100000` (`src/middlewares/log/index.ts:5`). tracklight caps the real path: `MAX_BODY_BYTES = 32 * 1024 * 1024` with a hard error past it (`crates\engine\src\providers.rs:25`, `:82-99`).
**Verdict: `keep ours`.**

---

## 5. Guardrail / check cadence and fail direction

**5.1 — A check that errored is not a check that failed.**
Portkey: `checkResults.every(r => r.verdict || (r.error && !r.fail_on_error))` (`C:/t/portkey/src/middlewares/hooks/index.ts:420-424`), with `successfulChecks` / `failedChecks` / `erroredChecks` reported as three distinct lists (`:465-470`).
tracklight already spells three outcomes, in two places: `parse_failures` counted separately from scores (`crates\engine\src\lib.rs:243`, incremented at `crates\engine\src\judge.rs:344-349`), and at the matrix level `errored` (a cell with no candidate scores at all) counted separately from `cand_passes` (`crates\runner\src\compare.rs:459-462` vs `:466`), with `errored_cases` on the report (`compare.rs:641`).
**Verdict: `keep ours`.** Same rule, arrived at independently, and tracklight carries it further — `compare.rs:63` states that a *skipped* cell is a third thing again: "nothing failed, the operator's budget ran out".

**5.2 — Which way an error votes.**
Portkey: per check, defaulting to **fail-open** — `fail_on_error: check.parameters?.failOnError || false` (`hooks/index.ts:303`).
tracklight: fail-**closed**, structurally. All samples unparseable is a hard `Err`, never a phantom 0.0 (`crates\engine\src\judge.rs:356-362`); an unparseable verdict stops sampling that cell rather than scoring it (`compare.rs:177-185`).
**Verdict: `keep ours`.** A scoring service that fails open manufactures a passing grade — the one output it exists to be trusted about. Note for the corpus: the design record (§8) records that the registry holds three different answers to this question and none of them cites the others; tracklight is a fourth data point and the cleanest one, because the fail direction here is not a policy choice, it is entailed by what the product asserts.

**5.3 — Per-request checks vs. per-attempt checks.**
Portkey: input hooks are skipped when the span has a parent, so a fallback does not re-run them; output hooks are skipped on non-200; a hook-triggered retry carries `retryAttemptsMade` forward so it draws down the *same* budget (`src/middlewares/hooks/index.ts:448-463`; `src/handlers/handlerUtils.ts:1256-1279`).
tracklight: the fence/nonce injection check runs per judge call (`crates\engine\src\judge.rs:241-243`, `injection_suspected`), and the rubric is per case — so the per-request/per-attempt asymmetry exists here but is not named or costed. With `--gen-samples 10 × judge_samples`, a per-call check is multiplied by the sampling fan-out.
**Verdict: `adapt`.** Not the mechanism — the *question*. `docs\BENCHMARK_FRAMEWORK.md` §3c already reasons about batched judging as "throughput bought with measurement fidelity"; the same paragraph is where the check-cadence cost belongs.

**5.4 — A check declares the phase it may run at.**
Portkey: 21 built-in checks each declare `supportedHooks` and the runtime enforces it (`C:/t/portkey/plugins/default/manifest.json`) — schema/JSON checks are after-request only, authorization checks before-request only.
tracklight: a rubric dimension declares `kind` (`llm` vs. deterministic) and an optional `floor`, and "a dimension with no `floor` cannot fail a case on its own" (`docs\BENCHMARK_FRAMEWORK.md` §3a).
**Verdict: `adapt`.** The declared-eligibility dimension is the transferable half; tracklight declares *severity* per dimension and not *phase*, and the deterministic scorers (`crates\engine\src\scorers.rs`) are the ones that could run before a paid call and currently do not.

**5.5 — The verdict rides in the status space.**
Portkey mints 446 (a denying guardrail failed) and 246 (a non-denying guardrail failed on a cache hit) *outside* the registered HTTP code space, deliberately, because the consumer that must branch on it — `strategy.on_status_codes` — is a list of integers and cannot parse a body (`src/handlers/handlerUtils.ts:1316-1335`; `src/handlers/services/cacheService.ts:113-118`).
tracklight has the same mechanism in its own status space: `--gate` and `GET /v1/benchmarks/:id/gate` map `partial` / `aborted` to exit 4, *unverified*, distinct from pass and from fail (`docs\BENCHMARK_FRAMEWORK.md` §5a).
**Verdict: `keep ours`.** Same law, same shape, different carrier. Worth citing as a second sighting rather than importing.

---

## 6. Credentials, slugs, priced rosters, the operator surface and its debug stream

**6.1 — The debug surface refuses to boot without a secret; here it warns and starts.** *(the strongest transfer in the run)*
Portkey: the admin middleware **throws at startup** when `conf.json.admin_token` is absent — `"Admin UI auth requires conf.json.admin_token. Set admin_token or start the gateway with --headless."` (`C:/t/portkey/src/middlewares/adminAuth/index.ts:8-19`). There is no configuration in which the debug surface is reachable and open; the third state is `--headless`, i.e. no surface at all.
tracklight: `AuthMode::from_env` defaults to `Dev` for anything unrecognized (`crates\api\src\auth.rs:26-29`); in `Dev`, a request with **no** bearer token authenticates as `Principal::Dev` (`crates\api\src\guards.rs:51-54`) and a request with **any unrecognized** token also does (`:87-89`); `Principal::Dev` is admin-equivalent everywhere (`guards.rs:93-98`). The response is a stderr banner (`crates\api\src\auth.rs:40-58`, called from `crates\api\src\main.rs:340`), and the server starts.
**Verdict: `adopt`.** tracklight is self-hosted, holds provider credentials for benchmarking, and serves an operator surface — the exact three conditions. The banner is honest and well-written and it is still a warning; portkey's answer is that a debug surface with credentials behind it has two legal states, not three, and the third is spelled `--headless`.

**6.2 — Redaction fails closed by allowlist, or open by denylist.**
Portkey: everything the stream emits is allowlisted — six provider-option keys survive and every other key becomes `[REDACTED]` (`src/middlewares/log/index.ts:20-37`), plus a blanket redaction of *all* request headers by key (`:18`). A new credential field is redacted by default.
tracklight: ingest scrubbing is a regex denylist over free text (`crates\api\src\redact.rs:337-342` → `lighttrack_anon::scrub`), whose precision problems are documented at length in `docs\DECISIONS.md` D14. But tracklight *already uses an allowlist where it constructs the payload itself*: `METADATA_PASSTHROUGH`, five keys, everything else dropped (`crates\api\src\redact.rs:215-221`).
**Verdict: `adopt`.** Not a new idea here — an extension of one this repo already made. The rule is: a payload this service constructs itself is allowlisted; free text it received from a caller is scrubbed. `crates\api\src\logging.rs:26-53` configures structured logs with no field-level pass at all, which is the seam where the rule is currently absent.

**6.3 — The credential is named by a slug, never held by the caller.**
Portkey: `conf.json.integrations[]` maps `dev_team_anthropic` → `{provider, credentials, rate_limits[], models[]}` (`C:/t/portkey/conf.example.json:19-46`); a request names the slug.
tracklight: provider keys are raw env vars — `ANTHROPIC_API_KEY` (`crates\engine\src\anthropic_api.rs:29`), a three-name fallback chain for Gemini (`crates\engine\src\providers.rs:268-278`), `OPENAI_API_KEY` (`:356-365`) — loaded by `dotenvy` from the CWD (`crates\runner\src\main.rs:43`). No credentials table in `schema\sqlite\001_init.sql`.
**Verdict: `adapt`.** The multi-team half does not apply: one tracklight install has one operator holding one set of provider keys. The rotation half does — "the secret never enters a config a caller writes" is already tracklight's rule for its *own* keys (§6.5), and the asymmetry between the two is the finding, not the slug.

**6.4 — A credential carrying its own price book and its own permitted roster.**
Portkey: each integration's `models[]` carries `{slug, status, pricing_config}` (`conf.example.json:38-44`), and a `preRequestValidator` returns both a terminal response and a `modelPricingConfig` that then rides on the log object (`src/handlers/services/preRequestValidatorService.ts:20-31`; `src/handlers/handlerUtils.ts:402-427`; `src/handlers/services/logsService.ts:37-40`).
tracklight: the price book is keyed `(provider, model)` and nothing else — `PriceBook` at `crates\core\src\pricing.rs:64-70`, table `model_prices` with PK `(provider, model)` at `schema\sqlite\001_init.sql:204-212`. A repo-wide search for a model allowlist attached to a credential returns nothing.
**Verdict: `adapt` — one clause.** "Team A pays list, team B is on a negotiated rate" is not expressible when the credential is not an axis of price resolution. tracklight prices benchmark runs and publishes leaderboards from them; a negotiated rate makes those numbers wrong in a way nobody can see. This is the `credential-vault` proposal.

**6.5 — Per-credential limits exist here, on the wrong credentials.**
Portkey puts `rate_limits[{type: requests|tokens, unit: rph, value}]` on the *provider* integration (`conf.example.json:26-37`).
tracklight has exactly this shape — `LimitScope::ApiKey(String)`, keyed by the key's opaque row id, never the secret (`crates\core\src\limits.rs:44-52`), stamped server-side from the authenticated principal and stripped from the body (`docs\BENCHMARK_FRAMEWORK.md`; `docs\ARCHITECTURE.md` §7a0) — on its *own* ingest keys.
**Verdict: `keep ours`.** The per-key-budget design is better than portkey's (row id not key material, server-stamped, admin traffic deliberately unattributed). The observation is only that the same machinery has never been pointed at the outbound direction.

**6.6 — Price variants.**
tracklight supports prompt-length tiers and batch/flex lanes as variant rows — `<model>@in>N`, `<model>@batch`, `<model>@flex`, no schema change (`crates\core\src\pricing.rs:66-70`, `:195-226`; `docs\PRICING.md`), with cost computed from the row whose `effective_date` ≤ the event time. Portkey's OSS tree ships `pricing_config: null` and no resolver at all.
**Verdict: `keep ours` — inverse list.** tracklight's price book is strictly the deeper artifact.

**6.7 — Operator-surface hardening already present.**
Constant-time admin compare (`crates\api\src\guards.rs:58-64`), failed-auth throttling as outer middleware (`crates\api\src\main.rs:515-518`), HMAC-verified billing webhooks rather than bearer (`main.rs:492`; `crates\billing\src\stripe.rs:14-23`), MCP write tools gated behind `LIGHTTRACK_MCP_ALLOW_WRITES` default off (`CLAUDE.md` § Key invariants). Portkey's surface is a 12-hour in-memory session Map (`src/middlewares/adminAuth/index.ts:6`, `:71-76`).
**Verdict: `keep ours`.** Everything except the boot gate (6.1) is stronger here.

---

## 7. Config shape — tree vs. flat, two doors

**7.1 — The recursive strategy tree.**
Portkey: `targets: z.array(z.lazy(() => configSchema))` — targets are configs, recursively (`C:/t/portkey/src/middlewares/requestValidator/schema/config.ts:12-74`), with 26 keys inherited child-wins at every hop and `retry`/`cache` **replaced wholesale rather than deep-merged** (`src/handlers/handlerUtils.ts:476-560`, `:503-513`).
tracklight: a benchmark is a flat `BenchTarget` list (`crates\core\src\score.rs:186-195`) parsed from `benchmark.target` (`crates\runner\src\bench.rs:33-42`); there is no policy to inherit.
**Verdict: `different forces`.** A flat config that does not need a tree should stay flat, and the inheritance table is the trap as much as the value — portkey's own child-`{attempts:1}` erasing a parent's `onStatusCodes` is the failure mode. Nothing to import.

**7.2 — There is no config object to validate.**
Portkey's whole contract is one ~170-line zod schema with cross-field `.refine()` invariants carrying human-readable messages (`src/middlewares/requestValidator/schema/config.ts`) — the checker *is* the contract, readable in one screen.
tracklight's API server has no `Config` struct: `main()` reads ~30 `LIGHTTRACK_*` vars inline via `env_or` / `std::env::var` (`crates\api\src\main.rs:199-289`, documented in the module comment at `:76-123`), and `config\lighttrack.example.toml` is parsed nowhere in `crates\api`. There is no `validate()`; the closest equivalents are two non-fatal boot warnings (`crates\api\src\auth.rs:40-58`; `crates\api\src\redact.rs` `log_posture`).
**Verdict: `adopt` — from tracklight's own agent crate, not from portkey.** `crates\agent\src\config.rs:57-70` already does the right thing: `bail!` on an empty `sources` list, and eager resolution of every source's device-key env var at load "so a missing secret fails at startup, not on the first lease" (`:65-68`). The API server is the binary that holds the credentials and it is the one without the check.

**7.3 — Two doors into one pipeline.**
Portkey: `constructConfigFromRequestHeaders` normalizes a namespaced-header form and a JSON-config-header form into one `Options | Targets` before anything downstream runs (`src/handlers/handlerUtils.ts:836+`).
tracklight owns this and is ahead: `POST /v1/events` and the OTLP `POST /v1/traces` both reach `events::prepare_event`, so "validation, redaction, pricing and limit admission are identical" (`docs\ARCHITECTURE.md` §4) — and `docs\DECISIONS.md` D14 records the *third* door that had bypassed it (the relay-settle path writing its run event straight to the store) and closes it.
**Verdict: `keep ours` / cite theirs.** The transfer runs the other way; portkey is a second sighting of tracklight's rule in a different domain, and D14 is the better field record because it names the door that escaped.

---

## 8. Caching

**8.1 — Cache identity belongs where the request is canonical.**
Portkey: the cache key is SHA-256 over the *provider-transformed* body plus the endpoint (`C:/t/portkey/src/middlewares/cache/index.ts:14-26`; `src/handlers/services/cacheService.ts:88-95`), so two different gateway-level requests that transform identically share a hit.
tracklight: no response cache exists (no `moka`, no `cached`, no LRU anywhere). The only cache is `RedactionCache`, a per-project persistence-policy cache with a 60s staleness TTL (`crates\api\src\state.rs:90-95`).
**Verdict: `adapt`.** Not for ingest — an event is a fact and caching it is meaningless. For the *benchmark*: a compare run is `targets × cases × gen_samples × (1 + judge_samples)` calls, and `generate_deterministic` exists precisely to make a call reproducible — `temperature: 0` plus `PINNED_SEED = 42` (`crates\engine\src\providers.rs:148-218`). A re-run over an unchanged `(target, case, prompt, seed)` is the textbook cacheable call, and it is currently paid for again. `crates\runner\src\budget.rs` is where the saving would show up.

**8.2 — Cacheability is an enumerated per-endpoint property, not a per-call guess.**
Portkey: `putInCache` returns early on `stream`, and 16 endpoint kinds are excluded by an explicit non-cacheable list (`src/middlewares/cache/index.ts:69-72`; `src/handlers/services/cacheService.ts:22-40`).
tracklight: n/a today.
**Verdict: `adapt`.** If 8.1 lands, the exclusion is a list — a `Determinism::Sampled` outcome (`crates\engine\src\lib.rs:105-107`) must never be cached, because variation *is* the measurement there, and that has to be a declared property rather than a check someone remembers to write.

---

## 9. Testing — the mocked-provider-boundary integration suite

**9.1 — The inbound pipeline test exists and is at the right altitude.**
Portkey: `tests/integration/src/handlers/tryPost.test.ts` boots the real gateway and drives 26 named cases through fluent builders, asserting on the *pipeline's* behaviour — "should handle failing after request hooks with retry", "should include hook results in cached responses", "should not cache file upload endpoints".
tracklight: `crates\api\src\tests_ingest.rs` drives the real wired `build_router` over an in-memory `SqliteStore` via `tower::ServiceExt::oneshot`, 29 cases, "exercising auth → project-scoping → pricing → redaction → limit admission as one stack" (`tests_ingest.rs:3-9`, setup `:34-60`).
**Verdict: `keep ours`.** Same seam choice, same altitude, arrived at independently. Nine other `tests_*.rs` modules sit beside it.

**9.2 — The outbound provider boundary has no test at any altitude.**
`wiremock` / `mockito` / `httpmock` appear zero times in the tree and in no `Cargo.toml`. `crates\engine` calls real provider endpoints through `reqwest::blocking` (`crates\engine\src\providers.rs:29-40`).
Consequences, each naming code with no integration coverage: the retry ladder and its classification (`crates\engine\src\retry.rs:18-47`); the schema-rejection fallback that retries once schema-less (`crates\engine\src\providers.rs:112-122` and `:156-167`); the `EngineError` mapping of statuses and transport failures (`providers.rs:44-78`); and everything 2.3 / 3.1 would add.
**Verdict: `adopt`.** The unit tests in `retry.rs:68-115` exercise `with_retry` against a closure, which proves the loop and nothing about the classification of a real response. Portkey's contribution is the *seam choice*: mock at the outbound HTTP boundary and test everything inboard of it as one thing.

**9.3 — The builder DSL that makes 26 cases readable.**
Portkey: `RequestBuilder().model(…).stream(true).options` and `URLBuilder().chat()`.
tracklight: `compare.rs:765` uses a deterministic stand-in for `compute_cell` "whose value depends only on its coordinates" — a good fixture, but the matrix's inputs are still assembled inline per test.
**Verdict: `adapt`.** Worth exactly as much as 9.2 is worth and no more; it is what keeps 9.2 from being three tests.

---

## 10. Repository maintenance

**10.1 — Gate breadth.**
tracklight: 13 CI jobs in `.github\workflows\ci.yml` (`conformance`, `pg-conformance`, `firestore-conformance`, `test`, three client-SDK jobs, `clippy`, `fmt`, `deny-policy`, `secrets`, `secrets-latest`, `deny-advisories`), a pre-commit secret scan and a pre-push `scripts\gates.sh`, `deny.toml`, `.gitleaks.toml`, plus `docker.yml` / `release.yml` / `soak.yml`. Portkey's tree carries no equivalent and `plugins/Contributing.md` is the whole contributor contract.
**Verdict: `keep ours` — inverse list, decisively.**

**10.2 — Where the design reasoning lives.**
Portkey has no rules page at all: `docs/` is 486 lines of deployment recipes, `CLAUDE.md` is 94 lines of orientation, and the intake run's sweep of operating documents returned essentially nothing — every design finding came from `src/`. tracklight's `docs\DECISIONS.md` carries 17 dated decisions with their forces (D14's five paragraphs on why a redaction default flipped is the reference example), `docs\BENCHMARK_FRAMEWORK.md` §4b/§4c/§5a state failure accounting, leases and the spend ceiling as designed contracts.
**Verdict: `keep ours`.** The comparison is worth recording because it is the reason this study could be written at all in one direction and not the other.

---

## Tests to initiate

Each is paired — an assertion against the current behaviour and one against the intended — names the instrument, and names the number that would move.

1. **The provider-boundary suite.** New: `crates\engine\tests\provider_boundary.rs`, `wiremock` as a dev-dependency, mocking at the outbound HTTP seam that `crates\engine\src\providers.rs:29-40` builds. First pair: *a 429 carrying `Retry-After: 5` currently produces three attempts spanning ~600ms* / *after §2.3, it produces attempts spaced by the stated delay*. Instrument: the mock server's recorded request timestamps. Number that moves: **attempts-per-429 against a rate-limited provider**, and downstream, `errored_cases` in a matrix run against a throttling provider (`crates\runner\src\compare.rs:641`) — currently every cell burns its ladder and errors; the prediction is that it falls to near zero.

2. **The unhealthy-target matrix.** New case in `crates\runner\src` beside `compare.rs:765`'s deterministic stand-in: a 10-target × 60-case matrix in which target 7 fails every call. Pair: *today, target 7 is called 60 times* / *after §3.1, it is called ≤ N before being filtered, and the run still completes*. Plus the degenerate half from §3.2: *all 10 targets failing must still attempt, not refuse* — the assertion `crates\responder\src\breaker.rs:56-57` already makes for permits. Instrument: the mock's per-target call count. Number that moves: **wasted provider calls per matrix run** and **wall-clock to a `partial` verdict**.

3. **The boot gate.** New case in `crates\api\src\tests_dev_mode.rs` (4 cases today). Pair: *today, `LIGHTTRACK_AUTH_MODE` unset + `LIGHTTRACK_ADMIN_KEY` unset boots and serves `Principal::Dev` to a request with no token (`crates\api\src\guards.rs:51-54`)* / *after §6.1, the same environment fails `build_router` construction with a named error, and only an explicit opt-out flag reaches the old behaviour*. Instrument: the router constructor's `Result`. Number that moves: **the count of reachable-and-open configurations**, from ≥1 to 0.

4. **The allowlist boundary.** Extend `crates\api\src\redact.rs`'s test module (`:472-852`). Pair: *a newly added field on the logged config object is emitted verbatim* / *after §6.2, it is `[REDACTED]` until it is named*. Instrument: a struct-field-addition fixture. Number that moves: **fields exposed by default**, from all to none.

5. **Cache-hit rate on a benchmark re-run.** Pair: *re-running an identical compare run costs the same dollars twice (`crates\runner\src\budget.rs` `spend()`)* / *after §8.1, the second run's `budget_spent_usd` is near zero for `Determinism::Exact` cells and unchanged for `Determinism::Sampled` ones*. Instrument: `budget_spent_usd` on the run report (`compare.rs:649-656`). Number that moves: **repeat-run cost**.

---

## Features, ranked — with why the scope admits each

`scope.does` is one line: *"ingest LLM telemetry, score with judges, benchmark providers, serve an operator API"*. Each clause below cites which part of it admits the feature.

1. **Boot-time refusal on an unsecured operator surface** (§6.1) — *"serve an operator API"*. The scope names an operator API as a thing this project serves; it does not name an unauthenticated one, and the product holds provider credentials while serving it. Smallest change with the largest failure removed. → proposal `2026-09-02-browser-credential-boundary.md`.
2. **Read the provider's stated retry-after, against a wall-clock budget** (§2.3, §2.4) — *"benchmark providers"*. Benchmarking means calling providers under load, which means meeting 429s; tracklight already serves the header it will not read. → proposal `2026-09-02-retry-backoff.md`.
3. **A health filter over the benchmark target matrix, with the all-open degenerate case** (§3.1, §3.2) — *"benchmark providers"*. Same clause, and the two ship together or the second one ships an outage.
4. **Allowlist redaction wherever the service constructs the payload** (§6.2) — *"serve an operator API"* plus the existing `telemetry-pii-redaction` context. An extension of `METADATA_PASSTHROUGH`, not a new subsystem.
5. **A credential as an axis of price resolution** (§6.4) — *"benchmark providers"* + *"ingest LLM telemetry"*. A leaderboard priced at list rates for an operator on a negotiated rate is wrong and unfalsifiable from inside the product. → proposal `2026-09-02-credential-vault.md`.
6. **The outbound provider-boundary test suite** (§9.2) — enabling, not a feature; nothing above it is verifiable without it, which is why it is the first test and not the last feature.
7. **A deterministic-call cache for benchmark re-runs** (§8.1, §8.2) — *"benchmark providers"*. Real money, but it depends on 6 and on the exclusion list being right, so it sequences last.

**Not proposed, deliberately.** The registry subject the intake run is forging now — `software-engineering/multi-provider-gateway-plane` — is the fourth direction tracklight would take, and it is held back because the subject does not exist yet: proposing against an unwritten golden path would mean the owner reviewing a direction whose boundary against `retry-backoff` and `stream-proxy-hop` has not been drawn. Return condition: the subject lands.
**Also not proposed:** `model-routing/failover-horizon` (§5.3's home) — `.ai\manifest.yaml` excludes `software-engineering/llm-agent/orchestration` by list, so it is out of scope by the owner's own declaration, not by judgment.

---

## The inverse list — what tracklight does better

Stated plainly, because a comparison that only runs one way is an advertisement.

1. **Jitter on every scheduled delay** (`crates\engine\src\retry.rs:50-66`) vs. portkey's `randomize: false` (`src/handlers/retryHandler.ts:169`) on the component that is by construction its fleet's correlator. tracklight is right; the source is wrong.
2. **Retryability by typed variant** (`retry.rs:18-25`, `providers.rs:44-78`) vs. operator-supplied integer status lists that will happily retry a 400 policy refusal.
3. **Determinism recorded as provenance** (`crates\engine\src\lib.rs:94-139`) — portkey's strict mode collapses an unmappable finish reason to `stop` with no marker that it did.
4. **The price book**: effective-dated rows, prompt-length tiers, batch/flex variants, an admin `PUT` so prices change without a redeploy (`crates\core\src\pricing.rs:66-70`, `:195-226`; `crates\api\src\main.rs:410-411`). Portkey ships `pricing_config: null`.
5. **Per-key budgets keyed by row id, server-stamped, body-stripped** (`crates\core\src\limits.rs:44-52`; `docs\ARCHITECTURE.md` §7a0), with admin traffic deliberately unattributed rather than borrowing an identity.
6. **Three doors into one pipeline, and the record of the one that escaped** (`docs\ARCHITECTURE.md` §4; `docs\DECISIONS.md` D14).
7. **Atomic check-then-insert admission with a per-backend honesty report** — `Store::admission_is_atomic()` and a conformance test firing eight simultaneous admissions at one cap, with Firestore's `insert_event_checked_nonatomic` named rather than hidden (`docs\ARCHITECTURE.md` §7).
8. **Leases distinct from claims** (`docs\BENCHMARK_FRAMEWORK.md` §4c) — "a claim answers who won this job; a lease answers whether the winner is still alive" — with fenced writes and a 409 that stops a zombie worker. Portkey's admin sessions are an in-memory `Map` that dies with the process (`src/middlewares/adminAuth/index.ts:6`).
9. **Honest failure accounting**: `attempts` / `stale_reclaims` / `failures` as three counters, and `"benchmark failure: …"` vs `"worker lost: …"` as two operator-readable strings (`docs\BENCHMARK_FRAMEWORK.md` §4b).
10. **The gate that refuses to call a partial run green** (`docs\BENCHMARK_FRAMEWORK.md` §5a) — exit 4, unverified, distinct from pass and fail.
11. **Thirteen blocking CI jobs, two hooks, and a locally-runnable `scripts\gates.sh`** (§10.1).
12. **Decisions written with their forces** (`docs\DECISIONS.md`) — the artifact class portkey's tree does not contain at all, and the reason its design record had to be reconstructed from 5,500 lines of TypeScript.
