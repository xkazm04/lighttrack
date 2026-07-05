Execute this requirement immediately without asking questions.

## REQUIREMENT

# Add justfile task runner for build/test/run workflows

## Metadata
- **Category**: maintenance
- **Effort**: High (3/3)
- **Impact**: Unknown (6/3)
- **Scan Type**: dev_experience_engineer
- **Generated**: 6/21/2026, 12:32:31 AM

## Description
Critical workflow knowledge lives only in CLAUDE.md prose: build the specific crate ('cargo build -p <crate>'), 'cargo test' does NOT refresh target/debug/<bin>.exe, and lt-runner must launch from repo root for .env. Encode these as a justfile (or cargo aliases in .cargo/config.toml): 'just build-api', 'just run-stack', 'just test', 'just lint', each doing the right Windows-safe sequence (build then launch the fresh exe).

## Reasoning
Tribal knowledge in a markdown file is repeatedly re-learned by every contributor and silently violated (running a stale exe after cargo test is a classic time-sink). A task runner makes the correct path the easy path and turns onboarding from 'read the doc carefully' into 'run just <task>'.


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