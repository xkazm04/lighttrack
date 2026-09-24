# Batch response budget is enforced against a ceiling the wire does not send

status: proposed
kind: task (defect)
registry-subject: software-engineering/llm-agent/prompt-and-context/prompt-assembly
registry-technique: response-reservation-sizing
source: intake:claude-code-from-source (2026-09-21)
size: 2-4 files, ~40-90 lines depending on which option is taken

## The defect

Two crates size the same response and neither reads the other.

`crates/runner/src/batch.rs`
- `OUT_TOKENS_PER_DIM = 220` — derived from a measured failure, per its own comment.
- `DEFAULT_MAX_OUT_TOKENS = 16_000` — the response budget `cases_per_response()` divides.
- `run_batched()` comment: overrunning the output is the one failure that costs every
  case in the batch at once, "so the ceiling is enforced here rather than left as advice."

`crates/engine/src/anthropic_api.rs`
- `MAX_TOKENS = 4096`, `MAX_TOKENS_HIGH_EFFORT = 64_000`, chosen by `max_tokens_for(effort)`.
- That value is what goes on the wire as `max_tokens`.

The enforcement is real and it enforces the wrong number. At default effort on the
bare-API path the packer protects a budget **3.91x larger** than the ceiling the
request will carry.

| LLM dimensions | packer packs | wire ceiling fits | over-packed by |
| --- | --- | --- | --- |
| 1 | 72 | 18 | 54 |
| 2 | 36 | 9 | 27 |
| 3 | 24 | 6 | 18 |
| 6 | 12 | 3 | 9 |

Arithmetic over the two constants at 220 tokens/dimension/case. The failure produced is
exactly the one the packer's comment names: input fits, response does not, JSON truncates
mid-object, call unparseable, every case in the batch lost at once. `truncation_cap()`
raises `EngineError::Truncated` rather than salvaging a fragment, so it is loud — and it
is still the whole batch.

## Scope

Binds only where all three hold, which is why it has stayed open:

1. the bare Anthropic API provider path (`ANTHROPIC_API_KEY` present, routed at
   `crates/engine/src/providers.rs:347`) rather than a CLI transport;
2. effort below `xhigh` (at `xhigh`/`max` the wire ceiling is 64,000 and 16,000 sits
   safely under it — an escalation added for an unrelated reason closes the hole);
3. a batch wide enough to cross the per-dimension line above.

## Why it cannot be fixed in the engine alone

`ChatRequest` (`crates/engine/src/chat.rs:57`) carries `system`, `turns`, `schema`,
`tools`, `tool_choice`. **There is no output-ceiling field.** The packer's computed
budget has no channel to the request. The engine's constant is not a lazy default; it is
the only thing the type permits.

## Options

**A — clamp in the runner (small, local, no type change).**
Teach `max_out_tokens_from_env()` (or its caller) the ceiling the selected provider path
will actually send, and take the min. Keeps the change inside `crates/runner`. Downside:
the runner has to know a provider's wire constant, which is the coupling the engine
boundary exists to prevent.

**B — carry the projection on the request (correct, larger).**
Add an optional output-ceiling field to `ChatRequest`; have each provider path treat it
as a request-scoped ceiling under its own semantics (Anthropic `max_tokens`, OpenAI/
Gemini equivalents, CLI transports ignore or map it). `max_tokens_for(effort)` becomes
the floor when the caller supplies nothing. This is the shape the registry technique
argues for and it removes the class rather than this instance.

Recommend B, with A as a same-day stopgap if a batch run is imminent.

## Measurable

- **Target:** no batch is planned whose projected response exceeds the ceiling the
  request will carry. Read from a unit test over `cases_per_response()` plus the
  selected path's ceiling; currently violated for every rubric at default effort above
  the per-dimension line.
- **Floor:** batch sizes at `xhigh`/`max` effort are unchanged (they are already
  correct, and a clamp must not shrink them), and no new truncation is introduced on any
  provider path.

## Gate

`cargo test -p lighttrack-runner` for the planned-size assertion; `cargo test -p
lighttrack-engine` for the request-type change under option B. Note that `guidance_guard`
fails at HEAD for unrelated reasons — do not read it as this change's verdict.

## First step

Not taken. A test asserting the invariant is red until the fix lands, and committing a
red test to `main` is not this run's call to make. Step 1 of the work is that test,
written alongside option A or B rather than ahead of it.

## Open question for the owner

Is the bare-API path used for batch judging in practice, or is the CLI transport the
real path? If batch judging never runs on the bare-API path, this is latent rather than
live and option A is enough.
