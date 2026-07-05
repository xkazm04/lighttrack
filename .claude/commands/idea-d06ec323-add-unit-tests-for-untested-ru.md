Execute this requirement immediately without asking questions.

## REQUIREMENT

# Add unit tests for untested runner and cli crates

## Metadata
- **Category**: maintenance
- **Effort**: Unknown (5/3)
- **Impact**: Unknown (6/3)
- **Scan Type**: dev_experience_engineer
- **Generated**: 6/21/2026, 12:32:33 AM

## Description
crates/runner (13 modules: bench, score, compare, calibrate, rubric, dataset, schedule, billing, http, …) and crates/cli have zero tests despite holding complex orchestration and parsing logic. Add focused unit tests for the pure logic — comparison/calibration math, rubric parsing, schedule computation, CLI arg dispatch — extracting small pure functions where needed so they're testable without live providers.

## Reasoning
These crates contain the benchmark/scoring orchestration that is core to the product, yet a refactor there has no safety net and regressions surface only at runtime against paid LLM calls. Test coverage here documents intended behavior and lets contributors change the runner with confidence.


## Recommended Skills

Use Claude Code skills as appropriate for implementation guidance. Check `.claude/skills/` directory for available skills.

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