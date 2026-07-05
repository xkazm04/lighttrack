Execute this requirement immediately without asking questions.

## REQUIREMENT

# Add CI quality gate: fmt, clippy, and test on every PR

## Metadata
- **Category**: maintenance
- **Effort**: High (3/3)
- **Impact**: Unknown (8/3)
- **Scan Type**: dev_experience_engineer
- **Generated**: 6/21/2026, 12:32:30 AM

## Description
The two workflows in .github/workflows (docker.yml, release.yml) only build artifacts on version tags — no cargo fmt --check, clippy, or test runs on push/PR. Add a ci.yml that runs 'cargo fmt --all --check', 'cargo clippy --workspace --all-targets -D warnings', and 'cargo test --workspace' on PRs. The SQLite store conformance suite runs in-memory and offline, so the test job needs no external services.

## Reasoning
CI is currently release-only, so a broken build or failing test can land on main undetected — exactly the regressions the conformance suite was built to catch. A push/PR gate is the single highest-leverage DX investment: it protects every future change for the whole team at near-zero marginal cost.


## Recommended Skills

- **compact-ui-design**: Use `.claude/skills/compact-ui-design.md` for high-quality UI design references and patterns

## Notes

This requirement was generated from an AI-evaluated project idea. No specific goal is associated with this idea.

## DURING IMPLEMENTATION

- Use `get_memory` MCP tool when you encounter unfamiliar code or need context about patterns/files
- Use `report_progress` MCP tool at each major phase (analyzing, planning, implementing, testing, validating)
- Use `get_related_tasks` MCP tool before modifying shared files to check for parallel task conflicts

## AFTER IMPLEMENTATION

1. Log your implementation using the `log_implementation` MCP tool with:
   - requirementName: the requirement filename (without .md)
   - title: 2-6 word summary
   - overview: 1-2 paragraphs describing what was done
   - category: one of feature/bugfix/refactor/performance/security/infrastructure/ui/docs/test
   - patternsApplied: comma-separated patterns used (e.g. "repository pattern, debounce, memoization")

2. Verify: `npx tsc --noEmit` (fix any type errors)

Begin implementation now.