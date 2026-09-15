# Agent guidance bootstrap project overlay

## Skill improvement log

- 2026-09-15 — Refreshing this repository showed that `AGENTS.md` can be a guarded projection of a
  differently named canonical guidance file. Resolve `.ai/manifest.yaml`'s `guidance.canonical`
  before editing a discovered guidance file, and compare generated context-map provenance with
  workspace-member history so a newly added entry point is not silently absent.
- Proposed method improvement (installation is an ordinary copy with no development receipt): add
  canonical-guidance discovery and generated-map freshness checks to the evidence-gathering steps.
- Proposed repository follow-up (not performed by this guidance-only run): update the CI workflow's
  ship-inventory comment from six workspace binaries to seven now that `lt-gateway` is a member.
