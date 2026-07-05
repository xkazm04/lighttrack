Execute this requirement immediately without asking questions.

## REQUIREMENT

# Admin key compared in non-constant time (timing leak)

## Metadata
- **Category**: code_quality
- **Effort**: Low (1/3)
- **Impact**: Unknown (5/3)
- **Scan Type**: bug_hunter
- **Generated**: 6/21/2026, 12:41:17 AM

## Description
crates/api/src/guards.rs authenticate() checks the admin key with `&token == admin`, a byte-wise short-circuiting comparison. Under AuthMode::Enforced this is the master credential, and the early-exit timing is in principle observable, leaking how long a prefix matched. Fix: compare with a constant-time equality (e.g. the subtle/constant_time_eq crate) for the admin key, and apply the same to any other raw secret comparison so secret comparisons are timing-safe by construction.

## Reasoning
The admin key bypasses all project scoping, so any erosion of its secrecy is high-severity, and constant-time comparison is the standard, well-understood defense. The change is a one-line swap with effectively no regression risk. Worth closing now that enforced-auth deployments (Cloud Run) are live.


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