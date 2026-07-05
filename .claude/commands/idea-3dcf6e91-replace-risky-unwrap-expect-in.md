Execute this requirement immediately without asking questions.

## REQUIREMENT

# Replace risky unwrap()/expect() in billing & judge code

## Metadata
- **Category**: code_quality
- **Effort**: Unknown (4/3)
- **Impact**: Unknown (6/3)
- **Scan Type**: dev_experience_engineer
- **Generated**: 6/21/2026, 12:32:32 AM

## Description
~57 unwrap()/expect() calls exist in library code; the dangerous ones are in security/parse-sensitive paths: HMAC setup and webhook verification in crates/billing/src/stripe.rs (~189,208,219) and polar.rs, JSON parse/deserialize of judge output in crates/engine/src/judge.rs:219-220, and poisoned-Mutex panics from .lock().unwrap() in crates/api/src/alerts.rs:90 / prices.rs:61. Convert these to typed errors (BillingError/EngineError) and handle lock poisoning instead of panicking.

## Reasoning
A panic in a webhook verifier or judge JSON parser takes down a request (or worker) on malformed external input — turning a recoverable error into an outage and an unreadable stack trace for whoever debugs it. Typed errors here improve both reliability and the debugging experience for the team.


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