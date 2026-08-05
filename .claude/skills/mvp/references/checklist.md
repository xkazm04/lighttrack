# /mvp checklist — 21 items, 7 phases

Format per item: **key** — name. *Done when* (the certification bar). *Probes*
(cheap, deterministic checks an assessor runs — code-observable unless marked
ASK, which means only the operator can answer). *Paths* (the 2 realistic
options a decision round offers; one gets (Recommended) based on evidence).

Levels: 🟢 met (done-criterion observed) · 🟡 partial · 🔴 missing · ❔ not
code-observable, ask · ⚪ skipped-by-choice (manifest only).

---

## P0 — Define (scope-defining; everything later serves these)

**value-case** — Three killer features vs named competitors.
*Done when:* a written value case exists (feature → competitor gap → proof it
works live today) and each of the 3 features survives an end-to-end walkthrough.
*Probes:* look for a value/positioning doc (README pitch, docs/, landing copy,
`.personas/` feature inventory); ASK the operator to name/confirm the 3 and the
competitors; walkthrough happens at P6 smoke if not verifiable earlier.
*Paths:* A: draft the value case interactively from the feature inventory and
verify each feature live · B: operator supplies it, skill only verifies.

**monetization** — Monetization decision recorded.
*Done when:* the decision is recorded (pricing page shipped, or "free during
beta" with a revisit trigger). Deciding *not now* is fine; not deciding is not.
*Probes:* pricing page/route, billing deps (stripe, paddle, lemonsqueezy) in
package.json, plan/tier tables in schema; else ❔ ASK.
*Paths:* A: free-beta recorded in manifest + landing says so · B: pricing page
+ checkout wiring now.

## P1 — Rails (make the push trackable before heavy work)

**artifacts** — Personas artifacts fresh (context map, feature inventory, KPIs).
*Done when:* context map coverage healthy, features inventoried, KPI set
triaged. **Delegate: project-populate.**
*Probes:* `.personas/` marker; `context-map.json` exists + age + file-ref
resolution rate; KPI files/DB rows if reachable.
*Paths:* A: run project-populate (scoped to stale lanes) · B: refresh context
map only.

**milestone** — Ship milestone live in the Personas Factory Ship tab.
*Done when:* a "Launch MVP" milestone exists with measurable criteria wired so
convergence is visible in-app (and Fleet can dispatch on criteria).
*Probes:* not code-observable in the target repo — ASK, or check the Personas
app/DB when reachable.
*Paths:* A: create the milestone with criteria mirroring this checklist's
phases · B: track in mvp-passport.json only (no app widget).

**notes** — Durable decision/observation journal for the launch push.
*Done when:* a designated notes surface exists and this skill writes to it
every session (memory outbox for managed repos; else a repo doc).
*Probes:* `.personas/` presence (outbox path); else a docs/launch-journal or
similar.
*Paths:* A: memory outbox (managed) · B: `docs/launch-log.md` in-repo.

**fleet** — Project orchestratable by Athena + Fleet.
*Done when:* a Fleet-dispatched session against this repo completes
end-to-end and Athena can answer "where is <project> on its launch path?"
*Probes:* managed-project registration (ASK / Personas app); `.personas/`
marker; skills compatibility (marker + ingest caller + product-level context
names — all three or coverage silently reads 0%).
*Paths:* A: register + verify one dispatched no-op session · B: skip (manual
sessions only).

## P2 — Automate (pipeline + visibility early so later phases dogfood prod)

**cicd** — CI gating + auto-deploy to test env + manual prod promotion.
*Done when:* passport `ci` sub-signals read `checks:true, gating:true,
testDeploy:true, prodPromotion:"manual"`. **Delegate: passport-onboard §CI.**
*Probes:* `.github/workflows/` (or CI config), branch protection via `gh api`,
`app-passport.json` ci block.
*Paths:* A: full passport CI dimension run · B: checks-only gating (no
deploy automation yet).

**deploy** — Production domain + deploy + rollback.
*Done when:* the prod URL serves the current build over SSL with prod env vars
set, and a rollback has been executed once on purpose (or the provider's
instant-rollback verified). **Delegate: passport-onboard §hosting.**
*Probes:* hosting config (vercel.json / netlify / Dockerfile+fly / etc.),
authed provider CLI, `app-passport.json` hosting entry, prod URL reachable
(fetch it), domain ≠ *.vercel.app default (ASK if custom domain is wanted).
*Paths:* A: wire prod env on existing host + custom domain · B: accept
provider subdomain for MVP (recorded).

**observability** — Prod errors reach a dashboard; LLM calls tracked if any.
*Done when:* a deliberately thrown prod error appears in the dashboard and the
connector binding shows in app-passport.json (not ".env homework"). If the app
calls LLMs: calls tracked with cost attribution. **Delegate: passport-onboard.**
*Probes:* sentry/posthog/langfuse-class deps + init code, DSN env var NAMES in
env examples, `app-passport.json` observability entry; LLM SDK deps to decide
if llm-tracking applies (N/A is a valid level → treat as 🟢 with note).
*Paths:* A: wire Sentry-class connector to prod + test error · B: hosting
provider's built-in logs only (recorded as partial).

## P3 — Harden (engineering floor, against a deployable app)

**code-quality** — Gate suite green + shipped-surface structure bar.
*Done when:* typecheck clean, lint 0 errors, build succeeds, tests (if any)
pass; no component >200 LOC and no obvious perf sink (unbounded lists, render
loops) on SHIPPED paths — sweep the shipped surfaces, not the whole repo.
*Probes:* run the repo's own scripts (typecheck/lint/build/test from
package.json); LOC scan of components on shipped routes.
*Paths:* A: fix to green + targeted refactor of oversized shipped components ·
B: fix gates only, log structure debt.

**auth** — Account lifecycle actually works.
*Done when:* signup, login, password reset (email arrives), email
verification, session expiry, and rate limiting on auth routes each walked
end-to-end on prod (or staging if prod not yet live — re-walk at P6).
*Probes:* auth framework (next-auth/clerk/supabase/lucia…), reset+verification
flows present in code, email provider wired (resend/postmark/ses), rate
limiting middleware. N/A for no-account products → 🟢 with note.
*Paths:* A: close the missing flows (usually reset/verification emails +
rate limit) · B: cut accounts from MVP (product call — ASK).

**security** — Security pass clean or consciously accepted.
*Done when:* the review runs clean or every finding is accepted by the
operator: secrets not in client bundle, API routes check authZ (not just
authN), baseline security headers, dependency audit triaged, no debug
endpoints in prod. **Delegate: security-review when available.**
*Probes:* grep client bundles/env usage for leaked secrets (`NEXT_PUBLIC_`
misuse), middleware/headers config, `npm audit` summary, debug/test routes.
*Paths:* A: full review + fix criticals · B: criticals-only sweep.

**design-system** — Token-based, consistent, responsive, base a11y.
*Done when:* shipped pages pass a consistency sweep (no five hand-rolled
buttons), a tokens file exists that new code actually uses, light/dark works
if promised, responsive on shipped pages, focus/contrast basics.
*Probes:* tailwind config / tokens file / component library dir; sample
shipped pages for raw hex + ad-hoc components; dark-mode plumbing.
*Paths:* A: consolidate primitives + tokenize shipped pages · B: freeze —
consistency-fix only the landing + onboarding path.

**i18n** — Launch-locale decision recorded; coverage if multi-locale.
*Done when:* the decision is in the manifest. English-only MVP is a legitimate
answer, recorded not defaulted. If multi-locale: no hardcoded strings on
shipped surfaces + coverage gate wired.
*Probes:* i18n framework presence (next-intl/i18next/…), locale file count,
hardcoded-string sample on shipped pages; the DECISION itself is ❔ ASK.
*Paths:* A: record English-only · B: wire/complete coverage for named locales.

## P4 — Polish (needs killer features stable)

**onboarding** — A stranger reaches killer feature #1 without help.
*Done when:* a fresh-account, no-priming run reaches killer feature #1 in
minutes; friction points logged and fixed or accepted. Empty states guide;
first-run flow exists.
*Probes:* first-run/empty-state components, tour/wizard code, protected-route
default landing; then an actual simulated walkthrough (UAT-character style —
live app if driveable, else code-derived journey).
*Paths:* A: build/repair the first-run path to feature #1 · B: guided-tour
overlay on existing UI.

**feedback** — In-app feedback intake that lands somewhere you read.
*Done when:* a test submission arrives somewhere the operator will actually
see (widget/mailto/form → inbox, issue tracker, or Personas).
*Probes:* feedback widget/route/mailto in shipped layout; where it posts.
*Paths:* A: minimal widget → email/issue intake · B: footer mailto + /feedback
page (5-minute version).

## P5 — Market surfaces (consume the P0 value case)

**landing** — Public page stating the 3 killer features, one clear CTA.
*Done when:* landing is live and a cold reader can say what the product does
in one sentence; CTA leads into the app.
*Probes:* landing route/repo, copy vs the P0 value case (the 3 features
present?), CTA target resolves.
*Paths:* A: build/rewrite landing from the value case · B: polish existing
copy to match the value case.

**seo** — Metadata, OG image, sitemap, robots on public routes.
*Done when:* pasting the prod URL into Slack/X shows a correct card; sitemap
resolves; titles/descriptions on public routes.
*Probes:* NextJS metadata API usage / head tags, `opengraph-image` or og
asset, `sitemap.(xml|ts)`, `robots.(txt|ts)`; live fetch of both when prod
URL exists.
*Paths:* A: full pass (metadata + OG image + sitemap/robots) · B:
titles/descriptions only.

**legal** — Privacy policy, ToS, consent matching reality.
*Done when:* pages exist, linked from the footer, and cookie-consent gating
matches what analytics actually drops. Template-grade is fine for MVP.
*Probes:* /privacy /terms routes, footer links, consent component vs
analytics cookies actually set.
*Paths:* A: template-grade pages + footer + consent wiring · B: pages only
(no cookies dropped → no banner needed; verify that's true).

**analytics** — Behavior funnel: visit → signup → activation → return.
*Done when:* "how many people activated this week?" is answerable from a
dashboard; activation event = killer feature used. (Distinct from
observability: that's errors, this is behavior.)
*Probes:* posthog/plausible/umami/ga deps + init, event calls at
signup/activation points, env var names in examples.
*Paths:* A: wire analytics + the 3 funnel events · B: pageview-only
(recorded as partial).

## P6 — Launch gate (the ritual, not a work item)

**launch-gate** — Smoke pass on prod + go/no-go record.
*Done when:* a scripted E2E pass of the critical path (land → signup → killer
feature #1 → core loop) is green against the PRODUCTION build/URL, and the
one-screen go/no-go review of all 21 items is written to the manifest with
the operator's verdict.
*Probes:* existing E2E harness (playwright/cypress) to reuse; else drive the
prod URL directly; every prior item's final level.
*Paths:* A: full smoke + go/no-go now · B: record no-go blockers and the
re-entry point for the next /mvp run.
