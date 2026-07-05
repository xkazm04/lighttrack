Execute this requirement immediately without asking questions.

## REQUIREMENT

# Add .env.example and one-command local stack startup

## Metadata
- **Category**: functionality
- **Effort**: Medium (2/3)
- **Impact**: Unknown (5/3)
- **Scan Type**: dev_experience_engineer
- **Generated**: 6/21/2026, 12:32:34 AM

## Description
Required env vars (GEMINI_API_KEY, OPENAI_API_KEY, LIGHTTRACK_*) and startup steps are scattered across CLAUDE.md, README.md, and deploy/README.md, and the only .env is git-ignored so newcomers can't see the shape. Add a committed .env.example documenting every variable with comments, plus a single 'dev up' script (PowerShell + sh) that brings up the compose stack and launches lt-runner from the repo root in one command.

## Reasoning
Onboarding currently means reverse-engineering three docs and a git-ignored file before anything runs — the classic first-hour-of-frustration for new contributors. A self-documenting template plus one startup command turns first-run setup into a copy-and-go experience and reduces 'works on my machine' env drift.


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