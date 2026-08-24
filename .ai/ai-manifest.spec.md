# The `ai-manifest` contract — schemaVersion 0.1.0

This document defines what `.ai/manifest.yaml` in this repository means. It **ships inside the
repository it describes**, because the moment a manifest matters most is the offline one: somebody
auditing a clone they did not write, on a laptop, with no access to whatever site a specification
might have been published on. Until 2026-08-24 the manifest pointed at "the ai-manifest spec carried
by the registry consumer lane" — a pointer that resolved from no clone, and in fact named no
document that existed anywhere. A contract whose definition lives somewhere else stops meaning
anything the moment that somewhere else is unreachable.

**Authority.** There is no external source of truth for this contract today, so this file *is* the
authority and there is nothing to drift against. If an `ai-manifest` specification is ever published
outside this repository, this file becomes a **vendored copy**: the direction is one-way (the copy
never edits back), and it acquires a byte-for-byte drift check that runs on every change and fails on
any difference without attempting a merge. Reconciling the copy is then the fix; deleting the check
never is.

**Reimplementation clause.** Any reader or generator that performs the checks described below is
conformant. `crates/core/tests/manifest_guard.rs` is *a* checker, not the definition. Every rule here
is stated as: the input it reads, the condition it asserts, and the outcome it emits — precisely
enough to write a second checker from this document alone. A question a reimplementer has to ask
somewhere else is a hole in this document, and should be filed as one.

---

## 1. Purpose

`.ai/manifest.yaml` is the agent-facing contract for a repository: what it is, how to build and
verify it, where the heavy subsystems live, what must not be touched, and where each check is
enforced. It exists so that a tool arriving cold — an agent, a CI bootstrapper, a fleet dashboard —
can act correctly without inferring conventions from file layout.

It is **not** a build system, a lockfile, or a place to embed content. It is a small, stable index of
pointers and commands.

## 2. Document shape

A single YAML mapping at `.ai/manifest.yaml`, UTF-8, with these top-level keys.

| Key | Required | Type | Meaning |
| --- | --- | --- | --- |
| `schema` | yes | string | Constant `ai-manifest`. Identity, and it never changes. |
| `schemaVersion` | yes | string | Semver of *this contract*, e.g. `0.1.0`. |
| `generatedAt` | no | string | ISO date the file was last written. |
| `generatedFrom` | no | list of strings | The inputs a human or generator consulted. Provenance, not a dependency list. |
| `repo` | yes | mapping | See §3. |
| `capabilities` | yes | mapping | See §4. |
| `paths` | no | mapping | See §5. |
| `boundaries` | no | mapping | See §6. |
| `controls` | no | mapping | See §7. |
| anything else | — | — | See §8. |

## 3. `repo`

| Key | Required | Meaning |
| --- | --- | --- |
| `name` | yes | The repository's own name. |
| `purpose` | yes | One sentence. What it is, not how it works. |
| `languages` | no | List of language identifiers, lowercase. |
| `archetype` | no | One of `service`, `library`, `cli`, `app`, `monorepo`, `data`. |

## 4. `capabilities` — names, not tools

Each key is a **capability name**: what a reader wants done, in tool-neutral vocabulary. Each value
is a mapping:

| Key | Required | Meaning |
| --- | --- | --- |
| `command` | yes | The exact shell invocation that fulfils the capability, run from the repository root. |
| `verified` | yes | Boolean. `true` only once someone (or a doctor tool) has actually run the command in a clean checkout and seen it succeed. It is a claim about evidence, not an intention. |

The naming rule is the point of the block: `test`, `lint`, `format-check`, `audit-advisories` are
capabilities; `cargo`, `eslint`, `ruff` are tools. Only the `command` string may name a tool, so a
reader that wants "run the tests" never has to know which ecosystem this is, and swapping the tool is
a one-line change that breaks nothing downstream.

Capability names are freely extensible. A reader that does not recognize a name ignores it (§8).

**Checkable rule (C1):** every value under `capabilities` is a mapping with a non-empty string
`command` and a boolean `verified`. A violation fails.

## 5. `paths` — pointers, never embeds

A mapping of well-known slot names to repository-relative paths. Content is never inlined: a
subsystem behind a pointer can change format entirely without breaking this contract.

Recognized slots (all optional): `contextMap`, `docs`, `workingAgreement`, `contributing`,
`dependencyPolicy`. Others may be added.

**Checkable rule (C2):** every value under `paths` resolves to an existing file or directory,
relative to the repository root. A pointer that does not resolve is a broken contract, not a note —
that is the exact failure this section exists to prevent.

## 6. `boundaries`

| Key | Meaning |
| --- | --- |
| `neverTouch` | Paths a tool must not modify. Build output and vendored trees. |
| `generatedNotHandEdited` | Paths that *do* change, but only through their generator (a lockfile through its package manager). Distinct from `neverTouch`, because conflating them forbids the tool that legitimately rewrites them — which is how a "never touch `Cargo.lock`" rule ends up forbidding the `cargo update` that answers a security advisory. |
| `secretsFrom` | Prose: where secrets come from, and where they must never go. |
| `secretScanning` | Mapping: `config`, `allowlist`, and `bindingRung` — the name of the CI check where a leaked credential cannot pass. |

## 7. `controls` — where each capability is enforced

| Key | Meaning |
| --- | --- |
| `ciHardPass` | List of capability names that BLOCK a merge. |
| `ciAdvisory` | List of capability names that run and report but never block. |

The split is graded by **input determinism**, not by how serious the findings sound: a check whose
verdict is a function of this repository's contents can block, because a failure is attributable to
the change being gated and fixable inside it. A check that reads a feed which moves without the
repository (an advisory database, an upstream rule set) can never be made green by work here, so it
must not wall a pull request — it goes in `ciAdvisory` and its output is read on a schedule.

`controls` is a **projection**. The workflow file is the authority; this block must be updated in the
same change as the workflow.

**Checkable rule (C3):** every name appearing in any `controls` list is a key of `capabilities`. A
control naming a capability that does not exist is a projection that has drifted.

## 8. Unknown fields MUST be ignored

A reader encountering a key it does not recognize — at any level — ignores it and continues. It never
errors, never warns as if it were malformed, and never drops it when rewriting the file.

A writer that rewrites the manifest **carries unknown fields through unchanged**. This is what makes
the two halves of §9 possible: without carry-forward, the first old tool to touch a file written by a
newer one silently deletes the newer fields, and additive evolution stops being additive.

## 9. Versioning

`schemaVersion` is semver over this contract:

- **Patch** — editorial only; no reader changes behavior.
- **Minor** — additive: new optional keys, new capability names, new `paths` slots. A reader written
  for an earlier minor version keeps working by §8.
- **Major** — a removal, a rename, or a meaning change. This is the version a reader is entitled to
  refuse.

`schema` never changes. A reader identifies the document by `schema`, then decides what it can do
with it by `schemaVersion`.

## 10. What a conformant checker asserts

The complete list, so a second implementation can be written from this document:

1. The file parses as a YAML mapping.
2. `schema` is exactly `ai-manifest`.
3. `schemaVersion` is a semver string.
4. `repo.name` and `repo.purpose` are non-empty strings.
5. **C1** — every capability has a non-empty `command` and a boolean `verified`.
6. **C2** — every `paths` value resolves in the working tree.
7. **C3** — every `controls` list entry names a declared capability.
8. Unknown keys are ignored, at every level, and preserved on rewrite.

Nothing above requires reading any source file of this repository. That is deliberate: a checker is a
checker, and a manifest that could only be validated by the program it describes would be a program's
behaviour wearing a standard's clothes.
