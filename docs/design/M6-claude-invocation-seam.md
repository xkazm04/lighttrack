# M6 — One headless Claude invocation seam

Size L · gate policy · wave A · contexts: judge-engine, alert-responder, device-agent, runner-worker

## Problem
Three crates spawn `claude -p` through their own code: the engine (`crates/engine/src/claude.rs`
~35-67, ~195-216: stdin null, `--json-schema`, effort suffix, 600 s reaper, `--bare`), the
responder (`crates/responder/src/claude.rs` ~20-110: `--permission-mode`, `--allowedTools`,
`--max-budget-usd`, cwd, tokio timeout, **prompt passed on argv** — Windows arg-length limit and
quote fragility), and the device agent via the engine (`crates/agent/src/exec.rs` ~59-65, which
can pass only prompt/model/system/schema). `resolve_claude_bin` is duplicated
(`crates/responder/src/config.rs` ~161-187 "mirrors … kept local"). No door probes availability
or auth before spending; billing-key strip/inject is deliberate in one door and absent in the
others; `ActionSpec` (`crates/agent/src/actions.rs` ~18-31) cannot express mode, cwd, tools or
budget although `docs/RELAY.md` says "allowed tools live on the device"; the agent drops the
envelope's `total_cost_usd` (`exec.rs` ~90-99).

## Design
1. New module `crates/engine/src/invocation/{mod,probe,run,envelope,resolve,posture}.rs` (each
   <300 LOC; a separate crate is acceptable if the engine's `Cargo.toml` deps make it cleaner —
   prefer the module):
   ```rust
   pub enum Mode { Generate, ReadonlyScan, Edit }
   pub struct Invocation<'a> { prompt: &'a str, model: &'a str, mode: Mode, cwd: Option<PathBuf>,
       allowed_tools: Vec<String>, system: Option<&'a str>, schema: Option<&'a str>,
       effort: Option<&'a str>, budget_usd: Option<f64>, timeout: Duration, bare: bool }
   pub struct RawOutcome { text, json: Option<Value>, cost_usd: Option<f64>, input_tokens, output_tokens, latency_ms, raw: Value }
   pub fn probe(bin: &str) -> Probe { installed, version, authed: Option<bool> }
   pub fn run(cfg: &ClaudeBin, inv: &Invocation) -> Result<RawOutcome, EngineError>
   ```
   **Posture is enforced in one place** (`posture.rs`): `Generate` ⇒ empty `--allowedTools`,
   neutral temp cwd (no ambient CLAUDE.md), no `--permission-mode`; `ReadonlyScan` ⇒ read-only
   allowlist (Read/Glob/Grep/LS + explicit extras), `--permission-mode plan` or default; `Edit` ⇒
   explicit `cwd` required, explicit permission mode required. Contradictions return
   `EngineError::Posture(..)` before spawning. Prompt travels over **stdin** (`Stdio::piped`),
   never argv; `stdin(null)` only for the empty prompt.
2. `subscription-auth-selection`: one function decides whether `ANTHROPIC_API_KEY` is stripped
   from the child env (seat run, `bare == false`) or required (`bare == true`); log the decision
   once per process.
3. Engine judge path calls `run` with `Mode::Generate`; behaviour byte-identical (D9/D12/D15
   invariants untouched; existing `claude.rs` parse tests move to `envelope.rs`).
4. Responder: `investigate` → `Mode::ReadonlyScan` with its existing `READONLY_TOOLS`
   (`investigate.rs` ~13-21) as `allowed_tools`; `act` → `Mode::Edit` with its `acceptEdits`
   permission mode and cwd; delete `crates/responder/src/claude.rs` and
   `config.rs::resolve_claude_bin`; keep `timeout_secs`, `max_budget_usd` as `Invocation` fields.
   Responder and `lt-runner serve` call `probe()` at startup and refuse to claim paid work when
   `installed == false` (log, exit non-zero for the responder; `serve` logs and keeps polling).
5. Device agent: `ActionSpec` gains `mode: Mode (default generate)`, `workspace: Option<String>`,
   `allowed_tools: Vec<String>`, `max_budget_usd: Option<f64>`, `timeout_secs: Option<u64>`.
   `AgentConfig` gains `workspaces_root: Option<PathBuf>`; `workspace` must resolve under it (same
   traversal rule as `validate_action_type`, `actions.rs` ~46-58). `exec.rs` builds an
   `Invocation`; `RunReport` gains `cost_usd` and `mode`, forwarded by `cloud.rs::settle` (the
   API's `ResultReq` gains optional `cost_usd`, `mode` — additive; **do not** change how the API
   prices the event — M5/D5 territory, out of scope here).
6. `actions/README.md` + `actions/_example/`: add a `readonly-scan` example; `docs/RELAY.md`
   posture matrix. New `docs/DECISIONS.md` entry: relay actions carry a mode; edit-capable actions
   require an explicit workspace and permission mode.

## Out of scope
Pricing relay runs (D5/M5), device enrolment (M18), job queue (M7).

## Gates
`cargo build/test/clippy -p lighttrack-engine -p lighttrack-responder -p lighttrack-agent
-p lighttrack-runner -p lighttrack-api`; a `posture_matrix` unit test asserting each mode's argv
shape and that `ReadonlyScan` never contains a write tool; an envelope test with prompt via stdin.
No live `claude` run required.

## Evaluation
Before: 3 `Command::new(claude_bin)` sites, 2 `resolve_claude_bin`, prompt on argv in one, 0
probes, `ActionSpec` 0/5 posture fields. After: 1 spawn site, 1 resolver, stdin everywhere,
`probe()` at two startups, `ActionSpec` 5/5.
