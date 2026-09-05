# Security Policy

LightTrack is self-hosted software that stores other people's LLM prompts, completions, and cost
data. A vulnerability here is usually a data-exposure vulnerability. Please report it privately.

## Reporting a vulnerability

**Use GitHub private vulnerability reporting:**
[Report a vulnerability](https://github.com/xkazm04/lighttrack/security/advisories/new)
(Security tab → Report a vulnerability). It is enabled on this repository. The report is visible only
to the maintainer until an advisory is published.

**Do not open a public issue for a security bug**, and please do not post a proof-of-concept
publicly before there is a fix.

A useful report includes:

- what an attacker can do, and what they need first (network reach? a valid ingest key? admin key?)
- the affected component — `lighttrack-api`, `lt-runner`, `lt-mcp`, `lt`, `lt-agent`, or a client SDK
- the store backend, if it is backend-specific (SQLite / Postgres / Firestore)
- the version: image tag or commit SHA
- reproduction steps, and the smallest config that shows it

If you cannot use GitHub's reporting form, email **michal.kazdan@nuda.dev** with the same details.
The advisory form is still preferred — it keeps the report, the fix and the eventual CVE in one
private thread — but email is a fine second door.

Failing both, open a **public issue containing no details** — title it `security contact request`
and say only that you have a security report. The maintainer will open a private advisory thread and
invite you to it. Keep the specifics out of that issue.

## What to expect

This project is maintained by one person as a side project. **There is no SLA and no bug bounty.**
Realistically: an acknowledgement within about a week, and a fix timeline that depends on severity
and on how much of a weekend the fix costs. If a report goes a week without a reply, ping the thread
— it was missed, not ignored.

Fixed vulnerabilities get a GitHub Security Advisory with credit to the reporter, unless you would
rather stay anonymous. Coordinated disclosure is welcome; pick a date with the maintainer rather than
assuming one.

## Supported versions

Only the latest `main` and the latest published container image
([`ghcr.io/xkazm04/lighttrack`](https://github.com/xkazm04/lighttrack/pkgs/container/lighttrack))
receive fixes. There are no maintained release branches — the project is pre-1.0.

## Verifying what you downloaded

Release artifacts and the container image are signed **keylessly** with
[cosign](https://docs.sigstore.dev/): there is no long-lived private key anywhere in this project, so
there is none to leak. The signing identity is the GitHub Actions OIDC token of the workflow that
published the artifact, which is why the commands below pin the *identity*, not just "a valid
signature" — an unpinned `cosign verify` is satisfied by anything anyone ever signed.

A release binary (`.sigstore.json` bundles are attached beside each archive, along with an SPDX SBOM
for the tagged source tree):

```bash
cosign verify-blob \
  --bundle lighttrack-x86_64-unknown-linux-gnu.tar.gz.sigstore.json \
  --certificate-identity-regexp '^https://github\.com/xkazm04/lighttrack/\.github/workflows/release\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com \
  lighttrack-x86_64-unknown-linux-gnu.tar.gz
```

The container image — signed by digest, because a tag is a moving pointer and a signature over a name
that can be repointed says nothing about the bytes you pulled:

```bash
cosign verify ghcr.io/xkazm04/lighttrack:latest \
  --certificate-identity-regexp '^https://github\.com/xkazm04/lighttrack/\.github/workflows/docker\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com

# and its SBOM, attested against the same digest
cosign verify-attestation ghcr.io/xkazm04/lighttrack:latest --type spdxjson \
  --certificate-identity-regexp '^https://github\.com/xkazm04/lighttrack/\.github/workflows/docker\.yml@' \
  --certificate-oidc-issuer https://token.actions.githubusercontent.com
```

**Known gap, 2026-09-05.** The GitHub Actions this project uses are still referenced by floating tag
(`actions/checkout@v4`) rather than by commit SHA, so a compromised or retagged upstream action could
run inside the workflow that does the signing. Signing raises the cost of tampering *after* the build;
it does not close that one. Pinning is tracked as outstanding supply-chain work.

## In scope

Anything that lets someone read, alter, or exfiltrate observability data they should not see, or take
over an instance:

- **Auth and keys** — API-key handling, admin-key checks (`LIGHTTRACK_ADMIN_KEY`), ingest-key scoping,
  project isolation. A path that lets project A read project B's events is in scope.
- **Ingest and query handling** — injection into any store backend, request bodies that crash or hang
  the server, unbounded reads.
- **Redaction and PII** — this one is explicitly in scope and easy to get wrong. `crates/api/src/redact.rs`
  applies two layers on ingest: the per-project persistence policy (`none` / `hash` / `drop`) and the
  server-global PII scrub gated by `LIGHTTRACK_REDACT_INGEST` (`all` | a CSV of project ids | `off`),
  which scrubs **every field a caller can write** via the `lighttrack_anon` regex pass: `input`,
  `output`, `error`, `tags`, `name`, `source`, and `metadata`. The only exceptions are the accounting
  keys inside `metadata` (`api_key_id`, `customer_id`, `product_id`, `cost_source`, `pricing_mode`),
  which pass through **un-rewritten and on purpose** — they are join keys, and scrubbing one would
  merge the buckets cost and margin are grouped by rather than protect anyone. If you use a real
  email address as a `customer_id`, it is stored as you sent it; send a pseudonym.

  The walk is **bounded** (depth, breadth, string length, total nodes) and, at every cap, **drops**
  what it could not inspect and marks it `<UNSCANNED: <cap>>` — distinct from the `<EMAIL>`-style
  markers, so "nothing sensitive here" and "I could not look" never read alike. A scrub that panics
  discards the payloads rather than falling back to the unscrubbed original.

  **The scrub is ON by default** — an unset variable means `all` (see D14 in `docs/DECISIONS.md`);
  `off` is an explicit opt-out. A case
  where a project's `drop`/`hash` policy is not honored, or where enabled redaction still persists PII
  it claims to remove — including on paths other than plain ingest (batch, OTLP, relay, dataset build,
  judge prompts, exports) — is a security bug, not a feature request. Report it here.
- **Secret leakage** — provider API keys or admin keys reaching logs, error responses, traces, or
  agent context. `lt-mcp` deliberately does not expose key minting; a way to reach it anyway is in
  scope.
- **The MCP server** — write tools reachable without `LIGHTTRACK_MCP_ALLOW_WRITES`, or anything that
  gives it DB access it should not have.
- **Deployment defaults** — a shipped Compose/Helm/Terraform default that exposes an instance
  unauthenticated to the internet.

Known-and-documented limits are *not* vulnerabilities, but a case where the docs overstate the
protection is: the PII scrub is heuristic and regex-based, and free-text PII (names, places) is
explicitly out of its reach. "Regex missed an unusual phone format" is a bug report. "Redaction was
on and stored the raw prompt anyway" is a security report.

## Out of scope

- Vulnerabilities in an instance you deployed with auth disabled (`LIGHTTRACK_AUTH_MODE=dev`) or bound
  to a public interface without a proxy. Dev mode is documented as unauthenticated.
- Anything requiring an attacker who already holds the admin key or database credentials.
- Findings from an automated scanner with no demonstrated impact, missing security headers on
  endpoints that serve no credentialed content, and dependency-advisory dumps with no exploit path
  into this code.
- Social engineering, physical access, or attacks on the maintainer's accounts.
- Denial of service via sheer volume against your own instance — apply the built-in ingest limits.
