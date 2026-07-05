Execute this requirement immediately without asking questions.

## REQUIREMENT

# Add a provider seam and unit-test judge scoring + pass-gating

## Metadata
- **Category**: maintenance
- **Effort**: Unknown (5/3)
- **Impact**: Unknown (9/3)
- **Scan Type**: test_mastery
- **Generated**: 6/21/2026, 12:19:39 AM

## Description
run_rubric_judge in crates/engine/src/judge.rs computes the weighted-mean score, the floor-gated pass/fail decision, and the self-consistency agreement metric � the product money path � but only generate() reaches it and there is no fake-provider seam, so the only tests cover JSON extraction. Introduce a small generation trait (or fn pointer) so a deterministic fake provider can drive run_rubric_judge, then assert: a sub-floor critical dimension forces pass=false even when the weighted overall clears threshold, weighted means are correct, out-of-range model scores clamp to [0,1], and divergent samples lower agreement.

## Reasoning
For an LLM-as-judge tool, a silent regression in floor-gating or weighting means benchmarks pass when a business-critical dimension actually failed � exactly the regression the product exists to prevent. Adding the seam also makes the engine testable without burning live API calls.


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