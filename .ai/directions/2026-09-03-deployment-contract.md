---
subject: software-engineering/deployment-contract
project: tracklight
raised_by: intake intake-kube-0903 (peer comparison)
source: librarian/sources/2026-09-03-kube-rs.md
stage: CI, beside the nine gates in .github/workflows/ci.yml — a policy check over deploy/helm/lighttrack/ that runs before the chart is ever installed
size: 2 files / ~300 lines / M
status: accepted
---

## Why the scope implies it

`scope.does` ends with *"serve an operator API"* (`.ai\manifest.yaml`). An operator API is installed by an operator, and `deploy\helm\lighttrack\` is how. That chart is the only artifact in this repository that a stranger runs against their own cluster, and it is the only artifact in this repository that **nothing verifies**.

The chart says so itself, in its first three lines: *"UNVERIFIED template (authored without a local helm to lint) — run `helm lint` / `helm template` before installing"* (`deploy\helm\lighttrack\values.yaml:2-3`). `deploy\README.md:8-19` lists seven deployment surfaces and marks all of them **available**. `.ai\manifest.yaml`'s `controls.ciHardPass` lists nine gates and none of them reads `deploy/`.

`engineering-process/continuous-integration/deployment-contract:26-29` states what that costs:

> A deployment is a claim that a **specific, verified build** reached a **named environment** through a **declared path**. Every part of that claim the repository does not control in writing — where the build runs, what triggers it, what configuration it sees — is a part that will eventually diverge silently.

Three parts of this chart's claim are not controlled in writing, and each is a live defect rather than a hypothetical:

**One.** `deploy\helm\lighttrack\templates\deployment.yaml:8` is `replicas: {{ .Values.replicaCount }}`, over SQLite at `/data/lighttrack.db` (`values.yaml:17`) on a PVC declared `accessModes: ["ReadWriteOnce"]` (`deployment.yaml:96`). Two pods are two writers on one file. The invariant *does* lift — `secrets.databaseUrl` switches state to Postgres (`values.yaml:21`) — so the correct rule is conditional (`replicaCount > 1` requires `secrets.databaseUrl`), and a conditional rule stated nowhere is a rule nobody applies. The Deployment additionally sets no `strategy:` (`:7-11`), so even at one replica a rolling update briefly runs two pods.

**Two.** `values.yaml:42` is `resources: {}`, and the template emits the block under `{{- with .Values.resources }}` (`deployment.yaml:63-66`) — an empty map produces **no `resources:` at all**. The pod is BestEffort: first evicted under node pressure, unbounded on the way there, running judge workloads.

**Three.** No `ServiceAccount`, no `securityContext`, no `podSecurityContext`, no `automountServiceAccountToken` anywhere in `templates/`. The pod runs as the image's user with the namespace `default` ServiceAccount and its API token mounted, for a service that calls no Kubernetes API.

**The fleet has already solved this, dependency-free, and that is why this is a small direction.** `C:\Users\kazda\kiro\kp\scripts\deploy\check-chart.mjs` is 350 lines of `node:*` — no helm binary, no `npm ci`, no cluster (`:34-36`) — holding twelve policies plus an env contract, run in CI on every push (`kp\.github\workflows\ci.yml:139`). Its header states the force better than most golden paths state theirs (`:4-10`): the chart was correct *"BY REVIEW"*, and *"`helm template` renders a privileged pod as happily as an unprivileged one, `helm lint` has no opinion about replica counts, and none of the seven CI workflows had ever read `deploy/`."* Its rule 1 exists for the exact edit this chart already contains: *"A future edit that helpfully wires `replicas: {{ .Values.replicaCount }}` back up would pass every generic policy in existence"* (`:19-20`).

Three fleet projects met the same single-writer constraint from three substrates. kp made it un-regressable (`check-chart.mjs:156-174`). politicas wrote a comment — *"PGlite is single-connection per data dir… Never scale count above 1"* (`C:\Users\kazda\kiro\politicas\fly.toml:4-7`). tracklight wrote prose in `NOTES.txt:19-23` and templated the opposite.

## What the first context contains

Two files, both new, both dependency-free.

**`scripts/check-chart.mjs`** (or a Rust equivalent if the workspace prefers one binary — the policy content is the deliverable, not the language). Structure copied from kp's, because its two design decisions are the ones that make it runnable here:

- **It reads chart text, not rendered YAML.** kp's rationale applies unchanged: *"half of these files are `{{- if }}` / `{{- range }}` and do not parse until Helm has rendered them"* (`kp\scripts\deploy\check-chart.mjs:61-67`), so two helpers — the literal value of a key, and the keys a block declares — carry every policy.
- **Every rule is anchored to *both* the values and the template.** `check-chart.mjs:26-29`: *"a `securityContext` block nothing mounts is decoration, and checking only the values would call it hardened."* This chart's `resources: {}` under a `{{- with }}` is precisely that trap in reverse.

The starting policy set, each with its finding already known:

| policy | why | currently |
| --- | --- | --- |
| `replicas-conditional` | two writers on one SQLite file | **red** — `deployment.yaml:8` templates it; the rule is `replicaCount > 1` ⇒ `secrets.databaseUrl` non-empty |
| `no-update-strategy` | a rolling update overlaps two pods on one RWO volume | **red** — no `strategy:` block |
| `resources-not-applied` | BestEffort QoS on a judge workload | **red** — `values.yaml:42` empty, emitted under `{{- with }}` |
| `no-pod-security-context` | uid 0 by default; nothing declared | **red** |
| `service-account-token-mounted` | an API token in a pod that calls no API | **red** — `automountServiceAccountToken` absent |
| `secret-literal-in-values` | `values.yaml` is the file people paste into tickets | green today (`values.yaml:20` ships empty) — the rule keeps it that way |
| `service-exposed-by-default` | a default install should not ask a cloud for a load balancer | green (`values.yaml:30`) |
| `volume-access-mode` | ReadWriteMany schedules the second writer | green (`deployment.yaml:96`) |
| `image-version-coherence` | `values.yaml:9` `v0.0.4` vs `Chart.yaml:6` `0.0.4` vs the crate version, compared by nothing | **red** |

**`scripts/check-chart.test.mjs`** — the fixtures. kp runs `npm run test:deploy` immediately after `npm run deploy:check` (`kp\.github\workflows\ci.yml:139-141`), and the reason is worth copying: a policy that cannot fail is a policy that has stopped reading. Each rule gets a scratch chart that must trip it.

**The wiring**: one step in `.github/workflows/ci.yml`, and one `capabilities` entry in `.ai\manifest.yaml` (`chart-policy`), added to `controls.ciHardPass` in the same change — the manifest's own header requires that (`manifest.yaml`, `controls:` block: *"this is a projection of it and must be updated in the same change as the workflow"*), and `crates\core\tests\manifest_guard.rs` holds it.

**What it must NOT absorb.** Not `deploy\terraform\` or `deploy\cloudrun\` — those are different artifacts with different invariants, and `deploy\cloudrun\README.md` already states its own version of the storage rule correctly. Not `helm lint` or `helm template`: adding a helm binary to CI is a different (and larger) decision, and kp's whole point is that the useful half needs neither. Not the probe and health-check content — that is a separate direction (`2026-09-03-health-checks.md`) whose changes this gate would then hold. Not the amendment discipline as a *new* convention: kp's is one comment (`check-chart.mjs:38-40` — *"Loosening a value in values.yaml until the check goes quiet is the failure mode this file exists to prevent"*) and it should be copied verbatim in spirit.

## The measurable

**Chart findings, currently unmeasured because nothing reads `deploy/`. Predicted first run: ≥ 6 of the nine policies red. Target: 0, with every red either fixed in the chart or converted into a policy edit carrying its reason.**

The number is only honest with the second instrument beside it: **the fixture suite's must-fail count.** Every policy has a scratch chart that trips it, so `test:deploy`'s pass count equals the policy count. A gate whose findings go to zero because the policies stopped matching the template is the failure this pairing exists to catch — and it is a live risk here, since the checks are regexes over Go-template text and a refactor that moves a value into `_helpers.tpl` defeats them.

**Second number, and the one an operator would feel: installs that reach a running, durable, single-writer service on the first attempt.** Currently unknown and structurally low — `persistence.enabled: false` by default (`values.yaml:25`) means the default install loses its database on restart, and `NOTES.txt:19-23` says so only after the install has happened.

## What would make this wrong

**If nobody installs the chart.** The gate's value is proportional to the chart being a real distribution surface. `deploy\README.md:8-19` lists it as **available** alongside Docker, compose, Cloud Run and Terraform, and the honest question is which of the five anyone has actually used. If the answer is "Docker and Cloud Run only, the chart was written speculatively", then the correct direction is smaller and different: mark the chart experimental in `Chart.yaml:3` and in `deploy\README.md`, and gate nothing. That evidence is cheap — it is one question to the owner — and it should be asked before the file is written.

**If the conditional invariant cannot be expressed in chart text.** `replicaCount > 1 ⇒ secrets.databaseUrl` is a cross-value rule, and the two values live in different blocks of `values.yaml`. A text-level checker can read both, but it cannot see an operator's `-f my-values.yaml`. If the honest enforcement point is the template (a `{{ fail }}` when the combination is illegal, the way kp uses `required()` at `kp\templates\secret.yaml:12-13`) rather than CI, then most of policy 1's value moves into the chart and CI's job shrinks to "the guard is still there" — which is still worth having, but is a smaller claim than this proposal makes.

**If the regex approach has the hole kp's own study predicts.** Every policy here reads template *text*. Moving `securityContext` into `_helpers.tpl` and including it would satisfy the pod at runtime while the checker sees nothing — or, worse, would drop it while the checker still sees the string. The falsifier is concrete and should be run on a scratch branch before the gate is trusted: refactor one checked value into `_helpers.tpl` and confirm the gate's verdict changes in the direction the refactor actually moved the pod. If it does not, the gate is measuring file contents rather than deployed shape, and the correct answer is `helm template` in CI after all.
