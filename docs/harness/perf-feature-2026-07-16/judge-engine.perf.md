# Performance Optimizer — Judge Engine

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Non-`bare` default reloads ~40k tokens of CLAUDE.md/skills/MCP on every `claude -p` judge call
- **Severity**: High
- **Category**: wasted-llm-call
- **File**: `crates/engine/src/claude.rs:11-47`, `crates/engine/src/lib.rs:78-95`
- **Scenario**: Calibration of a modest labeled set — N=50 items × k=5 self-consistency samples = 250 judge invocations. Each is a separate `claude -p` subprocess.
- **Root cause**: `EngineConfig::default()` sets `bare: false` (lib.rs:92). With `bare` off, `invoke` does **not** pass `--bare`, so every subprocess auto-loads hooks/skills/MCP/CLAUDE.md. The doc comment on `bare` (lib.rs:82-84) states this re-caches ~40k tokens per call. The judge prompt is fully self-contained (rubric + input + output); none of that project context is needed to score a verdict. It is pure per-call overhead, paid once per subprocess with no reuse (each call is a cold process; server-side prompt cache only helps within its short TTL and only on the system block).
- **Impact**: Up to ~40k input tokens of irrelevant context attached to each of 250 calls (~10M tokens) that dwarf the actual judge payload (typically a few KB). At even cache-read/cheap-input rates this is real, recurring $ on every calibration and every gating run, and it inflates per-call latency (larger context to load). The judge doesn't get one bit of value from it.
- **Fix sketch**: Default `bare: true` for judging/calibration paths (the judge never needs project context), or force `--bare` in the judge entrypoints. Requires `ANTHROPIC_API_KEY` in env (already the documented trade-off).
- **Trade-offs**: `bare` bypasses subscription OAuth and needs an API key; make it the judge default rather than a global default if OAuth must stay available elsewhere.

## 2. Judged payload (input/expected/output) is embedded verbatim and re-sent for every sample, with no size cap
- **Severity**: High
- **Category**: payload-size
- **File**: `crates/engine/src/prompts.rs:121-152`, `crates/engine/src/judge.rs:178-207`
- **Scenario**: Judging a target whose candidate `output` (or `input`/`expected` reference) is large — e.g. a model that emitted a 200 KB response, or a long-document eval — with `samples=5`.
- **Root cause**: `build_rubric_prompt` interpolates `input`, `expected`, and `output` raw into the prompt with no truncation or byte cap. `judge_with` then re-sends that identical prompt for each of the `k` samples (`pool::parallel_map(k, ...)` at judge.rs:197). There is a hard cap on the provider *response* body (`MAX_BODY_BYTES`, providers.rs:25) but nothing bounds what we *send*. Input token cost therefore scales linearly with payload size **and** with `k`.
- **Impact**: For a 200 KB output (~50k tokens) at k=5, that's ~250k input tokens per single case judged — unbounded in the payload dimension (a pathological or accidental multi-MB output multiplies straight through), and multiplied by every self-consistency sample. This is the largest controllable input-token line item and has no ceiling.
- **Fix sketch**: Apply a documented character/token cap to `input`/`expected`/`output` before interpolation (head+tail elision with a `…[truncated N bytes]…` marker), sized to the judge model's context budget; surface truncation in the outcome so verdicts on truncated inputs are auditable.
- **Trade-offs**: Truncation can change a verdict on very long outputs; keep the cap generous and explicit rather than silent, and make it configurable.

## 3. k self-consistency samples re-transmit an identical prompt prefix with no prompt-cache reuse
- **Severity**: Medium
- **Category**: missing-cache
- **File**: `crates/engine/src/judge.rs:188-207`, `crates/engine/src/providers.rs:131-153`
- **Scenario**: Any `run_rubric_judge` with `samples > 1` (self-consistency), the common calibration/gating path.
- **Root cause**: The `k` samples send byte-identical prompts (only the model's sampling differs). No provider call marks the shared prefix for caching: the anthropic path serializes and passes the prompt fresh per subprocess (`claude.rs invoke`, no `cache_control`), and the Gemini/OpenAI bodies (providers.rs:183, 248) carry no explicit cache hint. So the full input is billed at full rate for all `k` samples. (OpenAI auto-caches ≥1024-token prefixes, partially mitigating that provider; Anthropic-via-CLI and Gemini do not benefit here.)
- **Impact**: For k=5, input tokens are billed ~5× when samples 2..k could be cache reads (~10% cost on the cached prefix). Rough saving on the input-token portion is ~60-70% across the sample set where caching is available — real recurring $ on every multi-sample judge, compounding with finding #2.
- **Fix sketch**: For providers that support it, mark the shared prompt prefix with cache_control (Anthropic Messages API path instead of a cold CLI subprocess per sample; Gemini explicit context cache). Alternatively expose the sample fan-out to a single batched request where the API supports it.
- **Trade-offs**: Moving the Anthropic path off `claude -p` to the raw Messages API is an architectural change (loses CLI session/OAuth conveniences); cache TTLs are short so the win requires the k samples to run close together (they do — same `parallel_map`).

---
Files read: judge.rs, prompts.rs, claude.rs, providers.rs, lib.rs (engine); calibration.rs (core); calibrate.rs (runner); plus pool.rs, parse.rs, retry.rs for call-flow.
