---
subject: software-engineering/health-checks
project: tracklight
raised_by: intake intake-kube-0903 (peer comparison)
source: librarian/sources/2026-09-03-kube-rs.md
stage: the /health handler in crates/api, and the two probes in deploy/helm/lighttrack/templates/deployment.yaml that both read it
size: 3 files / ~120 lines / S
status: accepted
---

## Why the scope implies it

`scope.does` says *"serve an operator API"* (`.ai\manifest.yaml`). The operator this service is for reads `docs\ALERTS.md`, watches other people's LLM calls fail, and installs tracklight to find out. A service in that business that cannot express its own health accurately is not merely inconsistent — it is the one defect its users are guaranteed to notice.

**One endpoint answers two different questions, and the chart asks it both.**

`deploy\helm\lighttrack\templates\deployment.yaml:52-59`:

```yaml
livenessProbe:
  httpGet: { path: /health, port: http }
  initialDelaySeconds: 5
  periodSeconds: 30
readinessProbe:
  httpGet: { path: /health, port: http }
  initialDelaySeconds: 3
  periodSeconds: 10
```

Same path, same port, different clocks. And the endpoint they both read is `crates\api\src\main.rs:541-543`:

```rust
async fn health() -> &'static str {
    "ok"
}
```

A constant. It observes no dependency, opens no connection, and cannot return anything but 200 while the process is running. So the two probes are not merely redundant — **the readiness probe is a liveness probe wearing a readiness name**, and the pod joins the Service the moment axum binds, whether or not the store is reachable, whether or not a migration has finished.

That is the exact failure `health-checks:81-85` is about: *"Each proxy check passes exactly when the proxy diverges from the target — which is the only situation the check existed for."* The proxy here is "the HTTP server answers"; the target is "this pod can serve a request end to end". They diverge precisely during a cold start, a severed store connection, or a running migration — which is when readiness is the only thing standing between a broken pod and live traffic.

The mirror-image defect follows from the same line. Once the endpoint *does* observe the store, wiring it to the liveness probe as well means any condition that makes it non-200 does two things at once: it removes the pod from the Service **and** it restarts the pod. Those are opposite remedies. Removing from the Service is correct when the pod is temporarily unable to serve — a Postgres migration, a cold cache, a reconnecting dependency. Restarting is correct only when the process is wedged and cannot recover on its own. A slow dependency behind a liveness probe is a restart loop, and the restart makes the dependency slower. Fixing the first defect without the second converts a false green into a crash loop.

**The condition is not hypothetical here.** `deploy\cloudrun\README.md` states that the app *"auto-migrates on startup"* when handed a Postgres DSN. With `initialDelaySeconds: 5` on liveness (`deployment.yaml:54`), a migration against a cold or busy database has five seconds plus one 30-second period before the kubelet kills it — then the restarted pod starts the migration again. The failure is a crash loop whose cause is a *successful* database that is merely slow.

**The corpus does not cover this, and that is a finding rather than an excuse.** `operations/service-operations/health-checks` is the governing subject and it is strong on the questions it asks — the present tense (`:17-25`), the three-state refusal to collapse *"could not determine"* into either side (`:50-57`), staleness as part of the result (`:135-144`), the proxy rule (`:81-85`). But `readiness` and `liveness` appear in it exactly once, at `:27-28`, as two entries in a flat list of check *domains* — never as an asymmetric pair with different remedies. There is no readiness-versus-liveness contract in the subject, no probe-asymmetry rule, and nothing about a dependency check inside a liveness probe. The nearest material is one technique line, `techniques/probe-design.md:128-133`: *"Warm-up is declared, not discovered."*

Two peers reached the rule independently, which is what makes it a corpus amendment and not just a chart fix:

- **gravitone**, in the fleet, states it in the file. `C:\Users\kazda\kiro\gravitone\deploy\helm\gravitone\templates\deployment.yaml:52-66` — *"/health returns 503 until the model is loaded — exactly what readiness wants. Liveness stays TCP so a long model (re)load never gets the pod killed"* — with readiness `failureThreshold: 30` and liveness a bare `tcpSocket`. It is the fleet's best cluster artifact on reasoning and it is one directory away.
- **kube-rs** reaches the readiness half from the other side: `Store::wait_until_ready()` gates work on **cache completeness**, is armed exactly once at the first `InitDone`, and a later resync does *not* re-close it, because closing it would deadlock work already in flight (`C:/t/kube/kube-runtime/src/reflector/store.rs:33-34, 137-140, 196-215`), wired as `Runner::delay_tasks_until(store.wait_until_ready())` (`controller/mod.rs:485-490`). The force is stated plainly: a reconciler reading a half-filled cache concludes its children are missing and recreates them — a correctness bug, not a latency bug.

## What the first context contains

**One handler split into two, in `crates/api`.** `/health` today is one answer. It becomes two endpoints over one internal health record — which is the shape `health-checks:151-157` already asks for, where a composite *"names its failing members"* and *"could not determine"* members are surfaced rather than laundered into either side.

- **`/health/live`** — answers *is this process wedged?* It touches no dependency. A process that can accept a connection and route a request is alive; that is the entire claim. This is the endpoint the liveness probe reads, and per `health-checks:81-85`'s proxy rule the honest form here is genuinely minimal, because the thing being observed genuinely is just the process.
- **`/health/ready`** — answers *should traffic arrive?* It observes the real store: the SQLite file is open, or the Postgres pool has a connection and the migration has completed. `health-checks:81-85` is explicit that a proxy check *"passes exactly when the proxy diverges from the target"*, so this must be a real round-trip, not a flag set at boot.
- **`/health`** stays, unchanged in shape, as the operator-facing composite, since it is what `deploy\README.md`'s quick-start and the Cloud Run script both curl. It is the rollup, and per `health-checks:151-157` it names which member is red.

**Two probes that read different endpoints**, in `deploy\helm\lighttrack\templates\deployment.yaml:52-59`, with the asymmetry gravitone demonstrates: readiness on `/health/ready` with a `failureThreshold` sized to the slowest legitimate migration, liveness on `/health/live` with a long `initialDelaySeconds`. Both values-driven, so the operator with a slow database can raise the threshold without editing a template.

**A `startupProbe`**, which is the mechanism that makes the two thresholds independent — it holds off liveness entirely until the pod has passed once, so a long first migration and a wedged steady-state process stop competing for one number.

**What it must NOT absorb.** Not the chart's other defects — replicas, strategy, resources, RBAC and secrets all belong to `2026-09-03-deployment-contract.md`, and the gate proposed there is what would hold *these* probes in place afterwards. Not `docs\ALERTS.md`'s alert vocabulary: what the operator is paged about is a different question from what the kubelet reads. Not the runner's health — the judge worker runs outside the cluster (`NOTES.txt:25`), has no probe of any kind, and its liveness signal is `stale_reclaims` in the store (`crates\store\src\sqlite\jobs.rs:77-84`); exposing that number is a real and separate item, recorded in the comparison at §2.15 and deliberately not proposed here. Not probe caching: `health-checks/techniques/probe-caching.md` governs it and nothing in this tree needs it yet.

## The measurable

**Restarts caused by a healthy-but-slow dependency: currently unbounded, target 0.**

Measured directly. Install the chart against a Postgres whose first connection is artificially delayed past the liveness window (a `pgbouncer` pause, or a DSN pointing at a database still starting), and count container restarts over five minutes. Today the predicted number is "restarts until the migration finishes or forever, whichever comes first"; after, it is 0 with the pod sitting `NotReady` — which is the true state and the one an operator can act on.

**Second number, and the cheaper one: the readiness probe's false-green rate.** Today `/health` returns 200 whenever the HTTP server is up. A pod that is serving but whose store is unreachable is `Ready` and receives traffic. The instrument is the existing ingest path: send writes to a pod whose store connection has been severed and count 5xx responses served to clients that the Service should never have routed there. Predicted today: every one of them. Target: zero, because the pod left the Service first.

## What would make this wrong

**If `/health` must stay a constant.** `main.rs:529` and `crates\api\src\tests_auth_throttle.rs:245-256` both treat `/health` as the one endpoint that never authenticates and must keep answering *"while a source is blocked"*. That is a deliberate property with a test holding it, and a readiness check that opens a database connection on every probe is a new unauthenticated cost surface at 6 requests/minute per pod plus whatever else curls it. If that trade is unacceptable, the correct shape is a **cached** readiness record — `health-checks/techniques/probe-caching.md` governs exactly this, with a TTL sized to the fact's rate of change — and this direction grows by one small module rather than shrinking. Decide it before writing the handler, not after.

**If a store failure genuinely warrants a restart.** The argument above assumes the process can recover once the dependency does. For SQLite on a mounted volume that is true — the file comes back. For a Postgres pool it depends on whether the pool reconnects or has to be rebuilt at construction. If the code path is "the pool is built once at boot and a permanent failure leaves the process unable to ever recover", then a restart *is* the correct remedy and the liveness probe reading the store is not a bug — it is the only available repair, and the honest fix is to make the pool reconnect, which is a different and larger direction.

**If the operator wants a restart loop.** A crash-looping pod is loud; a `NotReady` pod is quiet, and a service that has been `NotReady` for an hour with nobody watching is worse than one that has restarted forty times and filled an alert channel. `health-checks:17-25` insists a red *"arrive holding the fix"*, and Kubernetes' `NotReady` state does not — it is a state, not a notification. If nothing in this deployment watches readiness, then splitting the probes trades a loud wrong behaviour for a quiet correct one, and the paired change — an alert on readiness, in `docs\ALERTS.md`'s vocabulary — is not optional and should land in the same direction or the direction should not land.
