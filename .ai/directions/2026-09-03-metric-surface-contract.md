---
subject: software-engineering/metric-surface-contract
project: tracklight
raised_by: intake intake-vllm-0903
source: librarian/sources/2026-09-03-vllm.md
stage: the published surface — between the route table and the three client SDKs
size: 3 files / ~180 lines / M
status: accepted
---

## Why the scope implies it

The manifest's purpose is a **self-hosted** observability and scoring service, and
self-hosting is the whole argument. A hosted service can enumerate its consumers and
migrate them; this one cannot. It ships three client SDKs (rust, python, typescript),
a `contract/` suite, an `openapi.json`, and a `/v1/capabilities` endpoint whose job is
to tell a caller what a given deployment cannot do — every one of which is a promise to
someone whose deployment nobody here can inspect. That is precisely the force behind
the registry's `metric-removal-is-a-staged-pipeline`: a published surface acquires
consumers that the publisher cannot list, so removing from it is a different act from
changing internal code and needs its own procedure.

Note honestly what this proposal is **not** claiming. The sibling technique
`export-terms-not-ratios` was tested against this tree today and came back
`not-better` — the ingest status surface already publishes shed/timeout/admitted as
counters, names the admitted counter as the denominator for a rate, and states that it
deliberately declines a scrape endpoint. This tree does not need to be told how to
shape a metric. What it has no seam for at all is **removing** one, or removing any
other published field, on a schedule a downstream operator can plan against. The
`/v1/capabilities` comment already gestures at the problem — it prefers advertising a
capability over "a version number somebody has to remember to bump" — which is a
deliberate stance on surface evolution with the removal half not yet written.

Caveat the owner should weigh: this project's `.ai/manifest.yaml` carries no `scope:`
block, so the judgment above is read from `repo.purpose` plus the tree rather than from
a declared scope. If the owner adds a scope block that excludes surface-governance
concerns, this proposal is void and should be declined on that basis rather than on its
merits.

## What the first context contains

A **surface deprecation policy** with one enforcing instrument, sitting beside the
existing capability advertisement rather than replacing it:

1. A short policy document stating the three stages and the release axis: present and
   advertised with a stated removal version → present but off by default, erroring
   when used, re-enablable by one named flag that itself takes the version it escapes
   from → removed. Plus the rule that a patch release never removes.
2. A machine-readable `deprecated` marker on the affected field or route, surfaced in
   `openapi.json` and in `/v1/capabilities`, so a caller learns the removal version
   from the surface itself and not from a changelog it never reads.
3. One check in the existing gate that fails when a field present in the previous
   release's `openapi.json` has vanished without having carried a marker — the
   diff is against the published artifact, which is the only honest baseline.

**What it must NOT absorb.** Not the capability-advertisement design, which is working
and is a different question (what this deployment *can* do, versus what this version
still *promises*). Not versioning of the `/v1` prefix — the stance against a bumped
version number is deliberate and this policy is compatible with it. Not the SDKs'
own release cadence. Not any change to what is measured or how it is shaped; that
half was tested today and needs nothing.

## The measurable

**Fields removed from the published surface without having passed through a marked
stage, per release.** Today the instrument that would count it does not exist, so the
honest baseline is *unknown*, and the first thing the direction buys is the ability to
say the number at all. Target after one release cycle: zero, with the count produced by
the gate rather than asserted. Secondary: the number of surface fields currently
carrying a removal version, which should be greater than zero the first time anything
is deprecated and is otherwise a dead instrument.

## What would make this wrong

- **If the SDKs and the server are always released together and no operator ever runs a
  mismatched pair**, the consumers are enumerable after all, the central force is
  absent, and the policy is ceremony. The check: is there any supported combination of
  SDK version and server version that is not lockstep? If not, decline this.
- **If `/v1/capabilities` already covers it in practice** — that is, if the intended
  answer to "we removed a field" is "the capability stops being advertised and a caller
  must handle that anyway" — then the gap is a documentation gap, not a capability gap,
  and the right outcome is one paragraph in the existing docs rather than a new context.
  This is the likeliest reason to decline, and the owner is much better placed than the
  proposer to judge it.
- **If nothing has ever been removed from the surface**, the policy is premature. It is
  cheapest to write before the first removal and worth nothing until one is contemplated,
  so "defer until the first field is up for removal" is a legitimate answer with a
  natural return condition.
