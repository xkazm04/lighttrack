# tracklight vs. `kube-rs/kube` — peer design comparison

- **Source**: `kube-rs/kube`, clone `C:/t/kube`, pinned `7a4641d4cc2f693b2dee97b9fc15fadb96d7f62e`
- **Design record**: `librarian/sources/2026-09-03-kube-rs.md` (intake run `intake-kube-0903`, §3 design entries A1–A4 / B1–B3 / C1–C2 / D1 / E1–E2, §9 Rust-craft inventory R1–R14, §10 peer check Study 2)
- **Why this peer**: kube-rs is a controller runtime — a long-lived process that watches a store it does not own, converges declared state against observed state, and survives losing its own connection to the truth. tracklight is a long-lived service that watches providers it does not own, converges a judged verdict against a sampled one, and survives losing its own connection to them. The two dimensions this comparison runs on are the two places the systems actually touch: **(a)** Rust error/retry/queue craft, where both are idiomatic Rust services doing bounded work against a flaky dependency; **(b)** the Kubernetes workload contract, where kube-rs is a library you *deploy as* the workload and tracklight is a workload with a chart that has never been run.
- **Verdicts** come from the closed set `adopt` / `adapt` / `keep ours` / `different forces`. A `keep ours` carries its reason exactly as an `adopt` does.
- Nothing here is a task. This is input for the owner's direction pass; the three proposals it implies sit beside it in this directory.

**Verdict tally: 30 points — 13 `adopt`, 5 `adapt`, 7 `keep ours`, 5 `different forces`.** Dimension (a) 14 points (§1.1–1.14), dimension (b) 16 points (§2.1–2.16). The `adopt` count is high for a peer study and the reason is structural rather than flattering to kube: eleven of the thirteen are in dimension (b), where the thing being adopted is usually **kp's** answer rather than kube's, and where tracklight's artifact is self-declared unverified.

**Corrections to the seeded points**, made against the trees and listed up front because two of them change a verdict:

1. **Seed a-2 overstated the second classifier.** `crates\responder\src\classify.rs` is not a naive substring matcher. Its *phrase* list is substrings (`:15-27`), but its *status-code* list is matched structurally as whole tokens requiring HTTP context, with the collision it is avoiding written out in the comment (`:29-35`, `:46-56`) — `contains("500")` firing on `AssertionError: expected 500 got 200`. The contradiction with `crates\engine\src\retry.rs:3-4` is real and is still the most actionable point in this study, but the honest verdict is **`adapt`, not `adopt`**, and the reason is §1.2.
2. **Seed a-2's anchors were stale.** Not `classify.rs:7,40-58` / `pipeline.rs:54`; the type is `:4-10`, the function is `:38-60`, the call site is `crates\responder\src\pipeline.rs:53`.
3. **Seed a-8 was wrong about `#[ignore]`.** tracklight *has* the convention — `crates\store\src\sqlite\bench.rs:96`, `crates\api\src\tests_ingest.rs:385`, `crates\store\tests\soak.rs`. It applies it to the **timing** axis and not to the **live-dependency** axis, which is a different and smaller finding than "no convention". §1.11.
4. **Seed a-3 has landed since the seed was written.** `with_retry_within` now carries the wall-clock budget and the `OverBudgetWait` terminal state (`crates\engine\src\retry.rs:86-128`) — the accepted `retry-backoff` direction from run `intake-portkey-0902` (`.ai\directions\ledger.jsonl`, 2026-09-03). The `keep ours` verdict stands and is now backed by shipped code rather than by a proposal. §1.4.
5. **Seed b-11's anchor**: `deploy\helm\lighttrack\templates\NOTES.txt:25`, not `:26-27`.
6. **Seed b-9 understated the chart's absence.** Not only ServiceAccount/RBAC/securityContext: the chart also has no `strategy:`, no `PodDisruptionBudget`, no `checksum/config` annotation, and `resources` resolves to nothing at all. §2.

---

## Dimension (a) — errors, retry, custody, tests

### 1.1 — Retryability is a property of the type, on both sides, and tracklight states it more sharply

kube: `watcher::Error` is a five-variant enum whose *module doc* states the whole enum's retry class in one sentence — *"These are all considered retryable from a watcher's point of view"* (`C:/t/kube/kube-runtime/src/watcher.rs:21-48`). Retryability is documented once, at the type, not re-decided at each call site. `finalizer.rs:14-54` goes further and warns against `anyhow` by name, because a flattened error has no taxonomy left to read.

tracklight: `EngineError::is_retryable()` is a `matches!` over three variants (`crates\engine\src\retry.rs:34-41`), and the module header states the rule as doctrine: *"Classification is by typed `EngineError` variant — never by string-matching provider messages"* (`retry.rs:3-4`). The doc comment on `is_retryable` additionally explains an *exclusion* — `OverBudgetWait` is deliberately absent because *"it is a terminal state the ladder itself produces, and retrying it would re-ask a provider that already named a wait we could not hold"* (`retry.rs:31-33`).

**Verdict: `keep ours`.** kube documents the class of a whole enum; tracklight documents the class of each variant *and* the reason a variant is excluded from it. The second is the stronger artifact, and it is the shape `backend-platform/resilience/retry-backoff:42-43` asks for — *"Classify first. The failure's class — not the caller's impatience — decides whether to retry, when, and who gets to say so."*

### 1.2 — …and one crate over, the doctrine is contradicted. Name it.

`crates\responder\src\classify.rs:38-60` is a second retry-class decision in this workspace, and it reads an error **message**:

```rust
let e = error.unwrap_or_default().to_lowercase();
if TRANSIENT_PHRASES.iter().any(|m| e.contains(m)) { return Class::Transient; }
```

Eleven phrases (`:15-27`: `"overloaded"`, `"rate limit"`, `"timed out"`, `"capacity"`, `"connection reset"`, `"econnreset"`, …), consumed at `crates\responder\src\pipeline.rs:53` to decide whether a spike is worth a paid Claude Code investigation. `crates\engine\src\retry.rs:3-4` says never do this. The same workspace does it.

The seed called this a naive string classifier. It is not, and the difference matters for the fix. The *numeric* half was already hardened, with the failure written into the comment: a bare `contains("500")` *"fires on `AssertionError: expected 500 got 200` (a real code bug misread as transient, so it is never diagnosed) and even on `processed 5000 rows`"* (`classify.rs:29-35`), so a code counts only as a whole token and only with an `http`/`status` context word or in leading position (`:46-56`), and both collisions are pinned by tests (`:100`, `:105`). Somebody already met this failure and fixed the half that bit.

The half that has not been fixed is the phrase list, and the reason it exists is structural rather than sloppy: `spike.error` is an `Option<String>` that crossed a process boundary. The responder is classifying an error **it did not produce** — a message that arrived as prose in a telemetry record, with no variant left to match on. `EngineError` is not in scope there and never was.

**Verdict: `adapt`.** The transferable rule from kube is not "delete the string matcher" — kube's own `DeserializeGuard` exists precisely because the thing it receives arrived degraded (`C:/t/kube/kube-core/src/error_boundary.rs:12-59`). It is *classify at the boundary where the type still exists, and carry the class across the wire as data*. `crates\core\src\event.rs` already does exactly this for one field — `Provider::from_wire()` canonicalizes a provider string at ingest rather than letting every consumer re-parse it. A retry class is the same shape of fact. Point 1.3 is what that costs.

### 1.3 — Where the class would be minted, and what it makes the responder

If the ingest boundary stamped a class the way it already stamps a provider, `classify.rs`'s eleven phrases become a **fallback for records that predate the field**, not the primary path — and the primary path becomes a match on data, which is what `retry.rs:3-4` asks for. The responder's own decision is unchanged; only its input improves.

The corpus states both halves of this and names the failure mode. `backend-platform/resilience/retry-backoff:47-50` — *"**Classification happens once, at the boundary, against structure.** The layer that still holds the structured response — status codes, error kinds, protocol fields — assigns the class"*. And `backend-platform/resilience/error-handling:79-81` — classification must key on *"**structured fields** — a status code, an error code, a typed variant, a machine-readable field in a response body — never on matching substrings of a human-readable message"* — with the reason at `:83-86`: a text classifier is *"a correct program today and a silent misclassifier after any dependency upgrade — and it fails in the worst direction"*. `error-handling:43-45` names what tracklight currently has: consumers that each classify separately *"manufactures three classifiers"*. tracklight has two.

The measurable that says this paid off is not "fewer string matches". It is `classify(…) == Class::Code` on a record whose producer knew it was a rate limit — a paid investigation spent on a provider incident. Nothing counts that today, because the transient branch only prints (`pipeline.rs:55-59`).

**Verdict: `adopt` (the discipline).** This is proposal `2026-09-03-error-handling.md`.

### 1.4 — The retry ladder's two exhaustion states, and the one kube does not have

kube: `RetryPolicy` retries only 429/503/504 with exponential backoff between `min_delay` and `max_delay`, and `server_aware` honours the server's `Retry-After` over the computed delay (`C:/t/kube/kube-client/src/client/retry.rs:50-80`). When the policy gives up, the caller gets the last transport error. There is one exhaustion state.

tracklight: `with_retry_within` has two, and names which one stopped it (`crates\engine\src\retry.rs:77-128`). *Exhausted* returns the underlying transient failure *"because that failure is the story"* (`:78-80`). *Over-budget wait* is its own `EngineError` variant carrying `asked_secs` and `remaining_secs` (`:111-116`), because a provider that stated a delay which does not fit is a different fact from a provider that kept failing — *"the number an operator needs to tell a wrong budget from a sick provider. Folding this into the rate-limit error would destroy exactly that fact"* (`:108-110`). And the wait is *"neither shortened nor the budget stretched"* (`:81-85`).

**Verdict: `keep ours`. Inverse list.** kube would sleep the stated delay and then discover it had nothing left. This landed on 2026-09-03 from the accepted `retry-backoff` direction (`.ai\directions\2026-09-02-retry-backoff.md`, ledger row same date) — it is the fleet's reference spelling of the rule now, and `crates\engine-http\src\lib.rs:917-930` in pumper is the other.

### 1.5 — Jitter on a *stated* delay: tracklight has it, kube does not

kube honours `Retry-After` verbatim when `server_aware` (`retry.rs:50-80`). Sixty workers told "5s" by one limiter wake in one pulse.

tracklight adds bounded jitter *past* the stated wait and explains both bounds: *"it only ever delays past what the provider asked, never before it, and it stays small so the wait remains recognisably the one that was stated"* (`crates\engine\src\retry.rs:23-27`, applied at `:101-103`, capped by `STATED_JITTER_CAP_MS`).

**Verdict: `keep ours`. Inverse list.** This is a boundary case `retry-backoff` does not state: jitter as herd control is in the corpus, but *jitter added one-sidedly to a delay somebody else chose* is not.

### 1.6 — `std::thread::sleep` in a workspace that runs tokio

kube's every wait is a `Stream` or a `Sleep` — `StreamBackoff` pauses the stream (`C:/t/kube/kube-runtime/src/utils/stream_backoff.rs:9-42`); nothing blocks a runtime thread.

tracklight: `crates\engine\src\retry.rs:122` is `std::thread::sleep(delay)`. The ladder is a synchronous `impl FnMut() -> Result<T>` (`:86-89`), so this is coherent *within the engine crate*. The hazard is the call graph: `crates\api` and `crates\runner` are async, and a stated `Retry-After` can now be minutes (`:106-121` only refuses it when it does not fit a 60-second budget — a 40-second wait fits). Forty seconds of a blocked executor thread is not a benchmark problem; it is an ingest problem.

**Verdict: `adapt`.** Not "rewrite the engine as async" — confirm the call graph. The check is mechanical and belongs beside the ladder's own tests: assert that no async fn reaches `with_retry` without a `spawn_blocking`. If one does, the fix is one wrapper, not a redesign.

### 1.7 — No streams anywhere, and that is mostly fine

kube is stream combinators end to end: the watcher is a `Stream` over an explicit `enum State<K>` advanced by one `step` returning `(Option<item>, next_state)` (`C:/t/kube/kube-runtime/src/watcher.rs:114-146`), the reflector is a pass-through combinator, the controller is `stream::select` into a scheduler into a runner (`controller/mod.rs:422-503`), and `lib.rs:7-9` states the property this buys: *"all components are designed to be usable á la carte"*.

tracklight: zero hits for `futures::Stream` or `StreamExt` across all 14 crates.

**Verdict: `different forces`.** kube's substrate is a long-poll HTTP connection that can desync; the FSM exists because *"HTTP GONE, means we have desynced and need to start over and re-list"* (`watcher.rs:610-623`) is a state, not an error. tracklight's substrate is a request/response call and a SQLite table polled by a claim statement — there is no cursor to lose. The transferable half is not `Stream`; it is **the state machine as an explicit enum with a `step` function**, and the place tracklight has that shape without the type is the job lifecycle (§1.9).

### 1.8 — Backoff reset semantics: no shared question

kube's `ResetTimerBackoff` resets the ladder only after the stream has been *healthy for a duration*, so *"a stream that fails every 30 minutes"* does not restart at the first rung each time (`utils/stream_backoff.rs`, `watcher.rs:5`).

tracklight's ladder is per-call and lives for at most 60 seconds (`retry.rs:19`). There is nothing to reset.

**Verdict: `different forces`.** Recorded because the *relay* queue is the one place tracklight has a long-lived cadence — a fixed five-hour interval (`crates\core\src\relay.rs:13-15`) — and a fixed interval has no ladder to reset either. If the relay ever gains a ladder, this is the rule it wants.

### 1.9 — Fence-token custody: the same technique as kube's finalizer, at a different scope

kube: `finalizer()` adds and removes a marker with a JSON Patch whose **first operation is a `Test`** — the add tests the current finalizer list because the platform does not dedupe, and the remove tests that index *i* still holds *our* name, since *"`Test` ensures that we fail instead of deleting someone else's finalizer"* (`C:/t/kube/kube-runtime/src/finalizer.rs:150-176`, add at `:184-222`). Custody is proved by the write, not assumed by the reader.

tracklight: `renew_lease` is one conditioned `UPDATE` that moves `claimed_at` forward *"only where it is still `fence` and the job is still live. Zero rows means this caller no longer holds the job — the affirmative evidence its work loop needs to stop, rather than a guess"* (`crates\store\src\sqlite\jobs.rs:106-125`). `finish` carries the same fence condition, because *"a worker reclaimed as stale while it was busy would otherwise finish later and overwrite whatever verdict its replacement had already written, silently and plausibly"* (`jobs.rs:127-137`).

**Verdict: `keep ours`.** Two independent systems reached compare-and-act-or-stop from opposite substrates. The design record names this as the fleet's existing instance of the proposed `declarative-resource-lifecycle` technique `deletion-blocked-until-dependents-confirm`; nothing here needs to change for that to be true.

### 1.10 — `cancelling` is outside the claimable set — the same guard, prophylactically

`crates\store\src\sqlite\jobs.rs:43-49`: `running` → `cancelling`, *"which is **not** in the claimable set — so the stale-claim reclaim path can never restart a cancelled run, no matter which of the two statements lands first"*. And `claim` separates two facts a naive counter would merge: reclaiming a `running` job is *"a WORKER DEATH, not a benchmark failure, so it is counted in `stale_reclaims` (never in `failures`, which is the retry budget)"* (`jobs.rs:77-84`).

kube has no equivalent because it has no lease — `grep -rni "leader.elect|lease\b|coordination.k8s.io"` over the whole tree returns zero code hits, and the design record argues that absence is load-bearing: the runtime's correctness comes from idempotence plus markers plus per-field ownership, not from exclusivity.

**Verdict: `different forces`, and it is the discriminator worth stating.** kube's writers converge on a server that arbitrates every write; tracklight's writers converge on a SQLite file that arbitrates nothing beyond one statement's atomicity. `backend-platform/work-execution/concurrency-guards`' `leadership-is-the-lock` covers the election tracklight does not need; what tracklight has instead is a *lease per unit of work*, which is the cheaper answer when the unit is a row.

### 1.11 — The test ladder: tracklight has the `#[ignore]` convention on the wrong axis

kube: four classes with prohibitions — *"Unit tests MUST NOT try to contact a Kubernetes cluster"*, *"E2E tests MUST NOT be used where an integration test is sufficient"* — closing on *"use the least powerful method of testing available to you"*, with a per-crate assignment (`C:/t/kube/CONTRIBUTING.md:100-118`). Live-dependency tests are `#[ignore]`d so the default `cargo test` is hermetic, and the e2e binary is deliberately trivial because its job is to prove in-cluster auth works, not to test logic (`e2e/README.md:1-8, 20-30`).

tracklight, corrected against the tree: `#[ignore]` **is** used — `crates\store\src\sqlite\bench.rs:96` (*"it asserts nothing, it measures"*), `crates\api\src\tests_ingest.rs:385` (*"it asserts on timing"*), `crates\store\tests\soak.rs`. The axis is *"does this test assert a number that a busy CI box will move"*. The live-dependency axis is handled differently: the conformance suite is env-var gated — *"the SQLite (in-memory) test runs in CI always; the Postgres / Firestore tests run only when a test env var points at one"* (`crates\store\src\conformance.rs:4-7`).

**Verdict: `adapt`, narrowly.** Env-var gating and `#[ignore]` reach the same place — the default run is hermetic — and this tree's manifest confirms it: `test` is `cargo test --workspace` and is in `controls.ciHardPass` (`.ai\manifest.yaml`). What tracklight lacks is not the convention but its **statement**: there is no written rule assigning a tier to a crate, so the next test to be written picks its power by whichever neighbour it was copied from. That is a `CONTRIBUTING.md` paragraph, not a code change, and it is why this is `adapt` rather than `adopt`.

### 1.12 — The conformance suite is stronger than anything in kube's tree

`crates\store\src\conformance.rs:1-7` — one backend-agnostic suite exercising the full `Store` trait, run by each backend crate's integration test, *"so SQLite, Postgres, and Firestore can be held to identical behavior"*, and deliberately *"safe against a **non-empty** database"*. Nineteen sections including `admission_race` (`:22-40`).

kube has no such thing; its abstraction is a trait over one server.

**Verdict: `keep ours`. Inverse list, and a `practices/` candidate.** The transferable rule — *a trait with N implementations owns one executable definition of what the trait means* — is a `build-and-release/test-harness` technique the corpus does not carry.

### 1.13 — Lint posture: kube states its exemptions, tracklight states none

kube: `#![deny(clippy::pedantic)]` with seven `#![allow(...)]` lines, **each carrying the reason it exists** (`// Triggered by educe derives on enums`, `// Triggered by nightly clippy on idiomatic code`) so a later reader can test whether the exemption is still earned (`C:/t/kube/kube-runtime/src/lib.rs:11-22`).

tracklight: no crate-level lint attributes anywhere in `crates/*/src/lib.rs`. The gate is the CI flag — `cargo clippy --workspace --all-targets -- -D warnings`, in `controls.ciHardPass` (`.ai\manifest.yaml`). That denies warnings at the *default* level; `pedantic` is not on, and nothing in the tree records a decision either way.

**Verdict: `adapt`.** Not "turn on pedantic across 14 crates" — that is a large mechanical change with a large mechanical `allow` list, which is exactly the shape the corpus's `quality-gates` warns produces unexamined exemptions. The portable half is the *justification convention*: whatever `allow` this tree eventually writes, it writes the reason on the line above. That is one sentence in `CONTRIBUTING.md` and costs nothing until the first `allow`.

### 1.14 — Feature-gating each unstable surface separately

kube: 23 feature flags, and `unstable-runtime` fans out to three separately-named sub-gates (`C:/t/kube/kube-runtime/Cargo.toml:15-18`) — so one surface can stabilise without the other two.

tracklight: the `Store` trait's capability negotiation is the equivalent mechanism at runtime rather than compile time — `admission_is_atomic()` defaults to `false` (*a backend is advisory until it proves otherwise*), relay methods default to `Unsupported`. Both answer "this surface is not ready everywhere", at different times.

**Verdict: `keep ours`.** A runtime default-method negotiation is the correct shape when the variation is *which backend is configured*, not *which features were compiled*. Recorded so a future reader does not import the flag count as an improvement.

---

## Dimension (b) — the chart

The chart is `deploy\helm\lighttrack\`. Its own first lines are the honest summary: *"UNVERIFIED template (authored without a local helm to lint) — run `helm lint` / `helm template` before installing"* (`values.yaml:2-3`). Nothing in CI reads `deploy/`. Every point below is a consequence of that one fact, so they are ordered by what an operator meets first.

### 2.1 — Two writers, one file: the chart templates the replica count

`deployment.yaml:8` is `replicas: {{ .Values.replicaCount }}`. The default is 1 (`values.yaml:5`), and nothing holds it there. Under it: SQLite at `/data/lighttrack.db` (`values.yaml:17`) on a PVC declared `accessModes: ["ReadWriteOnce"]` (`deployment.yaml:96`).

kube is not the peer for this one — it is stateless and assumes concurrent writers. **kp is**: `deploy\helm\kp\templates\deployment.yaml:12` pins `replicas: 1` as a literal with the reason above it, and `scripts\deploy\check-chart.mjs:156-165` fails CI if the literal is ever templated back, because *"A future edit that helpfully wires `replicas: {{ .Values.replicaCount }}` back up would pass every generic policy in existence"* (`check-chart.mjs:19-20`). That sentence describes this chart exactly.

**Verdict: `adopt` (kp's answer). Highest severity in this study.** The nuance tracklight has and kp does not: tracklight *can* run multi-writer — with `secrets.databaseUrl` set, state is Postgres and the invariant lifts (`values.yaml:21`). So the rule is conditional, not absolute: `replicaCount > 1` requires `secrets.databaseUrl`. A conditional invariant is still a checkable one, and it is currently checked nowhere — not in the template, not in `NOTES.txt`, not in CI.

### 2.2 — No `strategy:` at all, so a deploy is briefly two pods

The Deployment sets no `strategy` (`deployment.yaml:7-11`), which is `RollingUpdate` with `maxSurge: 25%` — on one replica, one extra pod. With `persistence.enabled: true` that is two pods claiming one RWO PVC; the second stays `Pending` and the rollout stalls, which is the *good* outcome. With `persistence.enabled: false` (the default, `values.yaml:25`) both pods come up on separate `emptyDir`s and the new one serves an empty database while the old one still holds writes.

kp pins `strategy: { type: Recreate }` (`deployment.yaml:13-14`) and gates it (`check-chart.mjs:166-174`): *"RollingUpdate overlaps the old and new pod on one RWO volume."*

**Verdict: `adopt`.**

### 2.3 — Readiness and liveness are the same probe

`deployment.yaml:52-59`: both probes are `httpGet /health` on the same port; the only difference is the timing. And the endpoint is `crates\api\src\main.rs:541-543` — `async fn health() -> &'static str { "ok" }`, a constant that observes nothing. So the readiness probe is a **liveness probe wearing a readiness name**: the pod joins the Service the moment axum binds, store reachable or not, migration finished or not. That is `health-checks:81-85`'s proxy rule exactly — *"Each proxy check passes exactly when the proxy diverges from the target — which is the only situation the check existed for"* — and the divergence is the cold start. The mirror defect arrives with the fix: once the endpoint *does* observe the store, a dependency failure would do two things at once — remove the pod from the Service *and* restart it.

kube's own readiness concept is the opposite: `Store::wait_until_ready()` gates work on **cache completeness**, is armed exactly once at the first `InitDone`, and a later resync does *not* re-close it (`C:/t/kube/kube-runtime/src/reflector/store.rs:33-34, 137-140, 196-215`), wired in as `Runner::delay_tasks_until(store.wait_until_ready())` (`controller/mod.rs:485-490`) — because a reconciler reading a half-filled cache concludes its children are missing and recreates them.

**The fleet's best instance is gravitone**, and it is worth citing by path because it states the asymmetry in the file: `C:\Users\kazda\kiro\gravitone\deploy\helm\gravitone\templates\deployment.yaml:52-66` — *"/health returns 503 until the model is loaded — exactly what readiness wants. Liveness stays TCP so a long model (re)load never gets the pod killed"*, readiness `failureThreshold: 30`, liveness a `tcpSocket`.

**Verdict: `adopt`.** The tracklight-specific reason is `deploy\cloudrun\README.md`: the app *"auto-migrates on startup"*. A Postgres migration behind `initialDelaySeconds: 5` + `periodSeconds: 30` liveness (`deployment.yaml:52-55`) is a pod that can be killed mid-migration on a slow database, restarted, and killed again.

**And the corpus does not cover this, which is worth stating precisely.** `operations/service-operations/health-checks` is the governing subject, and its thesis is the present tense — *"a green from an hour ago is a fact about an hour ago"* (`health-checks.md:17-25`) — with staleness developed at `:135-144`. But the subject contains **no readiness-versus-liveness distinction at all**: `readiness` and `liveness` appear once, at `:27-28`, as two items in a flat list of check *domains*, never as an asymmetric pair. The nearest material is `techniques/probe-design.md:128-133`, *"Warm-up is declared, not discovered"*. So the rule this chart breaks — **a liveness probe answers "is this process wedged"; a readiness probe answers "should traffic arrive"; wiring a dependency check into the first converts a slow dependency into a restart loop** — is not written down anywhere the project could have read it. gravitone reached it independently and wrote it in a comment (`gravitone\deploy\helm\gravitone\templates\deployment.yaml:52-54`); kube reached the readiness half independently and wrote it as a one-shot gate (`reflector/store.rs:196-215`). Two sightings and a fleet instance is an amendment to `health-checks`, not just a chart fix.

### 2.4 — `resources: {}` means the pod is BestEffort

`values.yaml:42` is `resources: {}` with the real values commented out at `:43-44`. The template applies them under `{{- with .Values.resources }}` (`deployment.yaml:63-66`), so an empty map emits **no `resources:` block at all**. The pod is BestEffort QoS: first evicted under node pressure, and free to take the node with it. The service runs judge workloads and a SQLite ingest path.

kp's answer: real defaults (`deploy\helm\kp\values.yaml:63-68` — `requests.cpu 250m`, `requests.memory 512Mi`, `limits.memory 1Gi`) plus a policy that checks **both** that the values declare a memory limit and that the Deployment still applies them (`check-chart.mjs:251-259`). The second half is the interesting one and it is exactly the trap here: a `{{- with }}` block over an empty default is a template that *looks* wired.

**Verdict: `adopt`.**

### 2.5 — No ServiceAccount, no RBAC, no security context of any kind

`grep` over `deploy\helm\lighttrack\templates\` returns no `ServiceAccount`, no `Role`, no `RoleBinding`, no `securityContext`, no `podSecurityContext`, no `automountServiceAccountToken`. The pod runs as whatever the image's `USER` is, with the namespace's `default` ServiceAccount and its API token mounted.

kube's *test fixture* is a better least-privilege example than this production chart: `C:/t/kube/e2e/deployment.yaml` ships a Namespace + ServiceAccount + Role + RoleBinding with an explicit five-verb list, for a job whose entire purpose is to create one Job and delete it.

**Verdict: `adopt`.** The cheapest first step is not RBAC — tracklight's API calls no Kubernetes API, so the correct Role is *none*. It is `automountServiceAccountToken: false` plus a `podSecurityContext` (`runAsNonRoot`, a non-zero uid, `fsGroup` matching the `/data` mount) and a container `securityContext` dropping `ALL`. kp's four hardening policies (`check-chart.mjs:186-224`) are the exact list, already written.

### 2.6 — The admin key ships as a chart value with no escape and no gate

`templates\secret.yaml:9` puts `.Values.secrets.adminKey` into a chart-managed Secret. There is no `existingSecret` path, so the credential lives in `values.yaml`, in the Helm release Secret, and in whatever file the operator passed to `-f`. `values.yaml:20` ships it empty, and `NOTES.txt:11-17` prints a warning *after* the install: *"WARNING: authMode=enforced but secrets.adminKey is empty — management endpoints will 401."*

kp does both halves. `existingSecret` short-circuits the whole template (`deploy\helm\kp\templates\secret.yaml:1`, resolved by `_helpers.tpl:36-40`), and when it is not used, `required()` **fails the install** rather than deploying a broken app: *"an empty operator password runs KP OPEN with no login"* (`secret.yaml:10-13`). And `check-chart.mjs:236-250` refuses a literal credential in `values.yaml` at all, with the reason naming the human behaviour — *"values.yaml is the file people paste into tickets"*.

**Verdict: `adopt`.** A post-install `NOTES.txt` warning and an install-time `required()` are not the same gate: one is read by whoever ran `helm install`, the other cannot be skipped.

### 2.7 — The default install loses its data and says so in the wrong place

`values.yaml:25` is `persistence.enabled: false`, so `/data` is an `emptyDir` (`deployment.yaml:72-73`) and the SQLite database dies with the pod. `NOTES.txt:19-23` says so — after the install.

kp defaults `persistence.enabled: true` (`deploy\helm\kp\values.yaml:41`) and annotates the PVC `helm.sh/resource-policy: keep` so an uninstall does not take the data with it — *"candidate PII lives here"* (`deploy\helm\kp\templates\pvc.yaml:8-10`).

**Verdict: `adopt`.** Recorded separately from 2.1 because they are different failures: 2.1 corrupts data that exists, this one silently has none.

### 2.8 — A config change does not roll the pod

The chart's env is inline in the Deployment (`deployment.yaml:33-51`), so a `helm upgrade` that changes `config.authMode` does re-render the pod spec and does roll. But `secrets.adminKey` lives in a Secret consumed by `secretKeyRef` (`:40-44`) — changing it updates the Secret and leaves the running pod holding the old value until something else restarts it.

kp closes this with `checksum/config: {{ include (print $.Template.BasePath "/configmap.yaml") . | sha256sum }}` (`deploy\helm\kp\templates\deployment.yaml:22-24`) — though note kp checksums only the ConfigMap, not the Secret, so kp has the same gap for its own credentials.

**Verdict: `adapt`.** Both charts need the Secret in the checksum, not just the ConfigMap. This is a shared fleet finding, not a tracklight deficit, and it is one line in each.

### 2.9 — No PodDisruptionBudget

Absent from both charts. For a single-writer stateful pod this is not a nicety: `kubectl drain` evicts the only replica and the service is down for the length of a reschedule plus a PVC reattach, with no signal to the operator that anything single-instance was involved. A PDB with `minAvailable: 1` on a one-replica Deployment blocks voluntary disruption entirely — which is the *correct* and deliberately loud behaviour for a database that cannot be moved without downtime.

**Verdict: `adopt`.** Same finding lands in kp's study; recorded in both because the artifact is per-project.

### 2.10 — The image tag is pinned in values and coherent with nothing

`values.yaml:9` pins `tag: "v0.0.4"`, and `Chart.yaml:6` says `appVersion: "0.0.4"`. Two spellings of one number, in two files, with nothing comparing them — and neither is compared to the crate version.

kp checks exactly this on **every push**, not only on a tag: `package.json` ↔ `Chart.yaml` `appVersion` ↔ a CHANGELOG section, because *"an operator who pins the chart's appVersion must be getting the version whose notes they read"* (`.github/workflows/ci.yml:143-148`, implemented at `scripts\release\prepare.mjs:122-126` — *"The chart's appVersion IS the image tag an operator gets by default — they must match"*).

**Verdict: `adopt`.** `engineering-process/continuous-integration/deployment-contract` is the home subject, and its rule — *a deployment is a claim that a specific verified build reached a named environment by a declared path* — is what a mutable tag cannot support. Note the honest ceiling on both projects: a tag is not a digest, so even a coherent tag is a claim about a name, not about a build.

### 2.11 — Four deployment surfaces, one of them a chart, none of them gated

`deploy\README.md:8-19` lists them: `docker/Dockerfile`, two compose files, `cloudrun/deploy.{sh,ps1}` + `cloudbuild.yaml`, `install.{sh,ps1}`, `terraform/modules/{gcp,azure}`, `helm/lighttrack`. All marked **available**. Nothing in `.github/workflows/ci.yml` reads any of them — the manifest's `controls.ciHardPass` list is nine entries and none of them touches `deploy/` (`.ai\manifest.yaml`).

kp's gate is dependency-free node with **no helm binary and no cluster** (`check-chart.mjs:34-36`), which is the property that makes it runnable here: the same 350 lines would read this chart with different policies.

**Verdict: `adopt` (the shape).** This is proposal `2026-09-03-deployment-contract.md`. The scope note that keeps it small: gate the Helm chart first, because it is the surface with the most invariants and the only one that is self-declared unverified. Terraform and Cloud Run are a second question.

### 2.12 — The single-instance invariant, stated three times in the fleet, enforced once

- kp: a CI policy with a named rule and a reason (`check-chart.mjs:156-165`).
- politicas: a comment. `C:\Users\kazda\kiro\politicas\fly.toml:4-7` — *"PGlite is single-connection per data dir… Never scale count above 1."*
- tracklight: prose in `NOTES.txt:19-23` and `deploy\cloudrun\README.md`, and the template that contradicts it (`deployment.yaml:8`).

**Verdict: `adopt`.** Three projects met the same constraint from three substrates (SQLite, PGlite, SQLite). One of the three made it un-regressable. This convergence is the argument for the `deployment-contract` technique the design record wants: *gate the deployment artifact with a check that needs no platform*.

### 2.13 — Cloud Run states a fourth spelling of the same invariant

`deploy\cloudrun\README.md` — *"Cloud Run has an ephemeral filesystem, so **SQLite data is lost on every cold start / new revision**"*, with Postgres as the escape. Cloud Run's `max-instances` is the same knob as `replicaCount`, and the README's answer is the same conditional as 2.1: multi-instance requires `--database-url`.

**Verdict: `keep ours`** — the README is correct and states its own force. Recorded because it means the conditional invariant in 2.1 is already understood by this project in one place and unenforced in every place. That is a cheaper direction than one where nobody had noticed.

### 2.14 — The chart deploys everything except the thing that does the work

`NOTES.txt:25` — *"The lt-runner judge/queue worker runs OUTSIDE the cluster (it needs the `claude` CLI + provider keys)"*.

kube is the opposite by construction: the runtime is a library and the controller *is* the workload; `e2e/` exists specifically to prove that in-cluster identity works, and its whole job is trivial *because* proving the environment is the point (`C:/t/kube/e2e/README.md:20-30`).

**Verdict: `different forces`.** A worker that shells out to an interactively-authenticated CLI genuinely resists containerisation, and pretending otherwise would produce a chart that cannot run. The finding is placement, not architecture: this fact appears in `NOTES.txt`, which an operator reads *after* installing, and not in `Chart.yaml:3`'s description — *"LightTrack — self-hosted LLM observability + scoring (API service)"* — which is where they read what they are installing.

### 2.15 — What an in-cluster control loop would own that the outside one cannot

Stated as analysis rather than as a proposal, because 2.14 is a real constraint and this is what it costs.

The outside runner claims jobs with `claim()` and renews a fence (`crates\store\src\sqlite\jobs.rs:72-95`, `:106-125`). Everything about its health is invisible to the platform: it is not a Pod, so it has no liveness probe, no restart policy, no `terminationGracePeriodSeconds`, and no place in the Deployment's `Recreate` ordering. When it dies, the *store* notices — `stale_reclaims` increments and `JOB_ERROR_WORKER_LOST` is stamped into the row (`jobs.rs:77-93`). That counter is doing a kubelet's job.

Four things a Pod-shaped runner would get for free and cannot get today:

1. **Restart on death.** Today a dead runner is discovered by the next claim's staleness window; the queue drains at zero until a human notices.
2. **A readiness gate before it claims.** kube's exact pattern: accept work into the queue while the cache fills, run nothing against a partial one (`C:/t/kube/kube-runtime/src/reflector/store.rs:196-215`). A runner that has not yet resolved provider credentials should be claiming nothing.
3. **Ordered drain.** `terminationGracePeriodSeconds` chosen to exceed the app's own drain — gravitone does this deliberately at `templates/deployment.yaml:76-78` (*45s, "Longer than TTS_DRAIN_TIMEOUT_S (20s)"*). Today a runner killed mid-job relies entirely on the fence to avoid a double-write, which works, but converts every deploy into a stale reclaim.
4. **Observed identity.** The fence is a timestamp, not a name (`jobs.rs:106-125`). A Pod has a stable identity the platform assigns; two runners started by accident are indistinguishable to the store today except by which one wins a claim.

**Verdict: `different forces`, with one adopt inside it.** Nothing here argues for containerising the runner. What it argues is that `stale_reclaims` is currently the *only* liveness signal for the component that does all the work, and that number is not on the operator API or in `docs\ALERTS.md`'s vocabulary. Exposing it is a small change with the same effect as a liveness probe: it makes a dead worker loud.

### 2.16 — What the corpus says this chart is claiming

`engineering-process/continuous-integration/deployment-contract:26-29` — *"A deployment is a claim that a **specific, verified build** reached a **named environment** through a **declared path**. Every part of that claim the repository does not control in writing… is a part that will eventually diverge silently."* This chart's claim is currently unverifiable on all three counts: the build is a mutable tag (2.10), the environment's invariants are templated open (2.1, 2.2), and the path is one of four with nothing reading any of them (2.11).

Three control-plane subjects — `declarative-resource-lifecycle`, `convergence-loop-and-requeue`, `watch-cache-and-resync` — are **being forged right now**: the `control-plane-operations` subcategory is declared in `knowledge/software-engineering/taxonomy.json:251-259`, the first subject's forge spec is written (`docs/subject-proposal-declarative-resource-lifecycle.md`), and no directory has been derived yet. They are named here, not cited, because there is nothing to open. When they land, 2.15's four items are what tracklight would read them for.

**Verdict: `adopt` (the framing).** Recorded as the study's closing point because it is the one that makes 2.1–2.11 a single direction rather than eleven chores.

---

## Tests to initiate

Paired, with the instrument named and the number that would move.

1. **The chart gate, on a fixture that is currently red.** Port kp's harness shape (`scripts\deploy\__tests__\check-chart.test.mjs`) and run its policy list against `deploy\helm\lighttrack\`. *Instrument*: findings count. *Predicted*: ≥ 7 on first run (2.1, 2.2, 2.4, 2.5 ×2, 2.6, 2.7). *Target after the direction*: 0, with each policy naming its reason. The paired assertion is the fixture that must stay red — a scratch copy with `replicas: {{ .Values.replicaCount }}` restored must fail rule 1, or the regex approach has the hole kp's own study predicts.

2. **Two writers, one file.** Install the chart at `replicaCount: 2` with `persistence.enabled: true` and no `databaseUrl`, against the soak workload in `crates\store\tests\soak.rs`. *Instrument*: the soak suite's assertions plus `PRAGMA integrity_check`. *Predicted*: corruption or a wedged second pod, > 0 incidents. This is the test that converts 2.1 from a review opinion into a number, and it is the only one here that needs a cluster.

3. **The blocked executor.** A test that calls the async ingest path while a mocked provider returns `429` with `Retry-After: 40`, asserting that no tokio worker thread is blocked for the duration. *Instrument*: `tokio::runtime::Handle::current().metrics()` blocking-thread count, or simply the latency of a concurrent request on the same runtime. *Predicted*: today, a 40-second stall on a shared worker (`crates\engine\src\retry.rs:122`). *Target*: unchanged latency for the concurrent request. Cheapest of the three and the only one that can regress silently.

4. **Class at the boundary.** Feed the responder a spike whose producer knew the failure was a rate limit and whose message says only `"upstream returned an error"`. *Instrument*: a new counter on `pipeline.rs:53`'s transient branch — investigations skipped as transient vs. investigations spent on a transient. *Predicted*: currently classified `Class::Code`, one paid investigation spent. *Target*: `Class::Transient` from the carried field, zero.

---

## Features, ranked, with why the scope admits each

`scope.does` is *"ingest LLM telemetry, score with judges, benchmark providers, serve an operator API"*; `scope.does_not` bars product UI, agent runtime and other domains (`.ai\manifest.yaml`). Nothing in either list excludes cluster operations, and the operator's own reading of the fleet named tracklight **admitted, and the clearest case**.

1. **The chart gate** (§2.11, and 2.1/2.2/2.4/2.5 are its first findings). *Why the scope admits it*: "serve an operator API" means an operator installs this, and the chart is the installation. The gate is also the only item here that makes the other ten un-regressable rather than fixed-once. Proposal `2026-09-03-deployment-contract.md`.
2. **Probes that answer different questions, and resources that exist** (§2.3, §2.4). *Why*: the same clause. A readiness probe that also restarts the pod is an operator-facing defect in a service whose value proposition is telling operators when things are wrong. Proposal `2026-09-03-health-checks.md`.
3. **One classification, minted where the type still exists** (§1.2, §1.3). *Why*: "ingest LLM telemetry" is the boundary in question, and the doctrine is already written in this tree (`retry.rs:3-4`) — this is bringing one crate into line with the workspace's own stated rule, not importing a new one. Proposal `2026-09-03-error-handling.md`.

Not proposed, recorded so the next sweep need not re-derive them: **§1.6** (the blocking sleep) is a test and possibly a one-line wrapper, too small for a direction and listed above as test 3; **§1.13** (lint justification) is a `CONTRIBUTING.md` sentence; **§2.15** (in-cluster runner) is `different forces` and its one actionable half — exposing `stale_reclaims` — belongs to `docs\ALERTS.md`, not to a direction.

---

## The inverse list — what tracklight does better

Six, each with the anchor a reader can open.

1. **Two named exhaustion states in the retry ladder** (`crates\engine\src\retry.rs:77-128`). kube has one; a stated delay that does not fit is silently slept through. tracklight returns `OverBudgetWait` carrying `asked_secs` and `remaining_secs`.
2. **One-sided jitter on a stated wait** (`retry.rs:23-27`). kube honours `Retry-After` verbatim and lets sixty workers wake together.
3. **A conformance suite that defines what a trait means** (`crates\store\src\conformance.rs:1-7`). Three backends held to one executable definition, safe against a non-empty database. kube has one implementation and no such artifact.
4. **The fence token as affirmative evidence** (`crates\store\src\sqlite\jobs.rs:106-125`). kube's finalizer `Test` is the same idea; tracklight's is cheaper, applies to a row rather than a record, and states the negative case — *"Zero rows means this caller no longer holds the job"*.
5. **Worker death counted separately from work failure** (`jobs.rs:77-84`). `stale_reclaims` is not `failures`, *"which is the retry budget"*, so a crash-and-reclaim cycle cannot consume a benchmark's chances. kube has no equivalent because it has no durable attempt ladder.
6. **The manifest guard** (`crates\core\tests\manifest_guard.rs`, per `.ai\manifest.yaml`'s own header). A test that holds the agent-facing contract to the code on every `cargo test`. kube's `CONTRIBUTING.md` is prose that nothing checks.

And one for the fleet rather than for this pair: **gravitone's chart is the fleet's best cluster artifact on reasoning** (`C:\Users\kazda\kiro\gravitone\deploy\helm\gravitone\templates\deployment.yaml:52-78`) and **kp's is the best on enforcement** (`scripts\deploy\check-chart.mjs`). Neither is in this repository, and both are one directory away.
