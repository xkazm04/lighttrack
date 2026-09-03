# Removing something from the published surface

This deployment model is the whole reason this document exists. A hosted service can enumerate its
callers and migrate them; LightTrack is **self-hosted**, so it cannot. It publishes `/openapi.json`,
`/v1/capabilities`, and three client SDKs (`clients/rust`, `clients/python`, `clients/typescript`)
that version and ship independently of the server — an operator running last quarter's Python client
against this week's server is a supported pair, not a mistake. Every name in that document is a
promise to somebody nobody here can list.

Changing internal code and removing from the published surface are therefore different acts. The
first needs a review; the second needs a schedule an operator can plan an upgrade against.

This is **not** about `/v1/capabilities`' capability advertisement, which answers a different
question and is working: *what can this deployment do*, a property of the backend an operator chose.
This answers *what does this version still promise*, a property of the build. A field going away
next release and a field this backend never had must stay distinguishable, so the markers are their
own list on the manifest, not a third state of `surfaces`/`unsupported`.

## The three stages

Each stage is one release long, at minimum. A field or route moves down, never sideways.

**1 — Advertised.** Still served, still honoured, behaving exactly as before. It carries a
`Deprecation { stage: Advertised, removed_in, replacement }` marker in `crates/contract`, which
surfaces as `deprecated: true` plus `x-removed-in` on `/openapi.json` and as a row of
`deprecations` on `GET /v1/capabilities`. A caller learns the removal version by reading the surface
it is already calling. Nothing breaks in this stage; that is the point of having it.

**2 — Erroring.** Off by default: using it is a 400 naming the replacement, not a silent
no-op. One flag re-enables it — `LIGHTTRACK_ALLOW_REMOVED`, whose value is the **version being
escaped from**, e.g. `LIGHTTRACK_ALLOW_REMOVED=0.2.0`. It re-enables only the elements whose
`removed_in` is that version, so a deployment that upgrades past it finds the escape matching
nothing and has to act. An escape hatch that outlives the thing it escapes stops being a hatch and
becomes the surface; taking the version as its argument is how this one expires by construction
rather than by anyone remembering.

**3 — Removed.** The name is gone from the table, from `/openapi.json`, and from the manifest.

## The rules

- **A patch release never removes.** Stage 3 lands on a minor or a major, and `removed_in` is
  therefore never an `x.y.Z` bump. An operator must be able to take a patch without reading anything.
- **A marker names a replacement.** "This is going away" without "use this instead" is not a
  deprecation, it is a notice.
- **The gate's baseline is the published artifact**, `clients/contract/openapi.baseline.json` —
  the document a release actually served. Diffing the source table against itself can only ever be
  green.

## The gate

`crates/api/src/openapi.rs::removal_guard` runs on every `cargo test --workspace`. It fails when a
name the baseline carried — an operation, a parameter, a body field, or a property of a named
response schema — is absent from the current document and the baseline did **not** mark it
deprecated. Additions are free; prose changes are free; removing a marked element the moment its
release arrives is free. The one act it refuses is the silent disappearance.

Refresh the baseline deliberately, at a release, never to make a red run green:

```bash
LIGHTTRACK_UPDATE_SURFACE_BASELINE=1 cargo test -p lighttrack-api openapi
```

## Deprecating something

1. Add the `deprecated: Some(Deprecation { … })` marker to the `Param` or `Endpoint` row in
   `crates/contract/src/endpoints/`. Both renderings follow from that one edit.
2. Ship that release. The marker is now in a document callers have.
3. Next minor: move the marker to `stage: Erroring` and make the handler refuse it unless
   `LIGHTTRACK_ALLOW_REMOVED` names the version.
4. The release named by `removed_in`: delete the row, and refresh the baseline in the same commit.

## Currently marked

`POST /v1/projects/:id/benchmarks`, body field `target` — stage `advertised`, removed in `0.2.0`,
replaced by a one-element `targets` array. The two have meant the same thing since the comparison
matrix landed; until now the only place that was written down was a six-word aside in the field's
own description.
