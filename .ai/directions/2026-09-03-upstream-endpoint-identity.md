---
subject: software-engineering/multi-provider-gateway-plane
project: tracklight
raised_by: intake llmfit-0903
source: librarian/sources/2026-09-03-llmfit.md
stage: benchmark target resolution — where a run is told which provider it is measuring, before the result becomes a leaderboard row keyed on that string
size: 3 files / ~150 lines / M
status: accepted
---

## Why the scope implies it

`scope.does` says *"benchmark providers"* (`.ai/manifest.yaml`). That is the whole
argument: a tool whose job is to publish measurements attributed to providers has
provider identity as a **correctness** concern, not a labelling one, and this tree
currently takes that identity from whatever string the operator typed.

The forces are not hypothetical here, and they arrive from the direction this
project already worries about. `crates/core/src/collective/aliases.rs:1-13` states
the rule the collective network runs on:

> The conservative rule is unchanged and load-bearing: an identity absent from the
> declared table passes through unchanged (minus the derivable normalization), and
> rows never merge on *family* — **a leaderboard that merged `openrouter` into
> `anthropic` would publish a number nobody measured.**

That reasoning is correct and it is applied to *declared* identities. It has no
counterpart for the case where the identity was never reliable in the first place:
an operator benchmarking a **self-hosted, OpenAI-compatible endpoint**. Half a
dozen local runtimes answer that same protocol, the protocol deliberately does not
identify the implementation — it exists so implementations are interchangeable —
and the provider string attached to the resulting row is a free-text label the
contributor chose.

Two contributors who each benchmark "their local setup" can therefore land in the
same merged row while having measured different runtimes at different versions
with different quantizations. The merge will pool them, winsorize them, compute a
between-source spread over them, and publish a confidence interval — every one of
those steps working correctly on inputs that were never the same measurement. This
is precisely the failure `aliases.rs` refuses to commit deliberately, reached by a
door nobody guarded.

## What the first context would contain

An **endpoint identity probe**, run once per benchmark target before any run is
attributed, producing a record rather than a boolean: the resolved implementation,
the evidence class that produced it, and the probe's own date.

The evidence hierarchy, strongest first — the registry technique
`upstream-identity-before-inventory` owns the reasoning:

1. **A route only that implementation serves.** A runtime that exposes its own
   native management surface alongside the shared protocol has published a
   discriminator by construction, and that route usually carries fields the shared
   schema has no word for.
2. **A namespace the implementation controls inside the shared protocol's own
   response** — the per-record ownership field, read from the response and never
   from configuration.
3. **The root path's banner**, which is crude and is the correct fallback for the
   empty case below.
4. **`Unrecognized`** — a first-class state, never the most likely guess.

The empty-inventory hole must be handled explicitly: a runtime with nothing loaded
returns a well-formed empty model list, so every per-record discriminator vanishes
exactly on a freshly set-up machine. That is what rung 3 is for.

The identity then rides with the run into the digest, so a self-hosted row is
keyed on *what actually answered* rather than on what the operator called it, and
an `Unrecognized` endpoint's rows are either excluded from the collective or
carried under an identity that cannot be confused with a named provider.

## What it must not absorb

- **The declared-alias table.** `aliases.rs` and `model_id::canonicalize` own
  identity for providers that declare themselves; this is the layer *beneath*
  them, for endpoints that do not. It feeds the alias table an input; it does not
  replace it.
- **Health or availability.** Whether an endpoint answers is `health-checks`'
  question and this project already has that direction in flight. This probe asks
  *what* is answering, which is a different question with different evidence.
- **Model identity.** `aliases.rs` is explicit that model identity is never
  family-matched. Endpoint identity does not license loosening that.
- **Routing.** Nothing here decides which provider serves a request; this project
  does not proxy inference and should not start.

## The measurable

**Rows in the collective whose provider identity was asserted rather than
established.** Today that is every self-hosted row, and the count is not
computable because the field does not exist — which is itself the finding. After:
every benchmark row carries an evidence class, and the number to watch is the
share of contributed rows whose class is `operator-asserted` or `Unrecognized`.

The stronger second measure, available once the field exists: **merged rows whose
sources disagree about the runtime behind one provider string.** That is currently
undetectable and is the failure this direction exists to prevent.

## What would make this wrong

Three falsifiers, in the order they should be checked, because the first one may
end the proposal on its own:

1. **Nobody benchmarks self-hosted endpoints.** If every benchmark target in
   practice is a named commercial provider reached at its own documented address,
   the identity is already established by the address and this whole direction is
   solving a problem the deployment does not have. This is a question about usage
   that the tree cannot answer — `docs/` lists the capability, nothing records
   whether it is used. **Ask the owner before building.**
2. **The collective already excludes self-hosted rows.** If a contributed digest
   can only carry rows for providers in the declared price book, the merge is
   already protected and the correct scope shrinks to a local warning rather than
   a plane. Checkable in `gather_run_stats` and the alias table's admission path.
3. **The probe is fooled by the thing it exists to catch.** If a proxy that fronts
   several runtimes stamps its own identity, the probe reports the proxy and the
   underlying runtime stays invisible — in which case the honest answer is that
   endpoint identity is not resolvable in that topology and the row should be
   marked `Unrecognized` rather than given the proxy's name. A design that reports
   the proxy as the runtime is worse than no probe, because it manufactures
   confidence.

## Size

Three files, roughly 150 lines: the probe and its evidence enum in the engine
crate, the field on the benchmark target/run record, and the digest path carrying
it through. The probe's per-runtime discriminators are data and belong in one
table with a dated comment per entry, because they are observations about other
people's programs and decay on their release schedules — not invariants.
