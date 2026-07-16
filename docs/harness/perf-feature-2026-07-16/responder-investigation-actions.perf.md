# Performance Optimizer — Responder Investigation & Actions

> Total: 3
> Critical: 1 | High: 1 | Medium: 1

## 1. The INVESTIGATE stage is completely ungated — no concurrency cap, no cooldown, no dedup

- **Severity**: Critical
- **Category**: subprocess / unbounded-growth
- **File**: `crates/responder/src/investigate.rs:23-25`, `crates/responder/src/claude.rs:20-59`, `crates/responder/src/breaker.rs:23-53`
- **Scenario**: A project starts flapping — a bad deploy makes every LLM call fail, and the alerter fires a spike webhook every minute. `pipeline::handle_trigger` runs on a detached task per webhook (`webhook` spawns it), and `investigate()` unconditionally spawns a fresh `claude -p` child. Nothing between the webhook and the spawn checks how many are already running, or whether this project was investigated 40 seconds ago for the same error. The `Breaker` exists but is only consulted inside `act::run_act` (`act.rs:57`), i.e. *after* the investigation has already been paid for — and `record()` is called only when a fix is actually applied (`act.rs:83`), so investigations never touch the counters at all.
- **Root cause**: The only bounds on a run are per-run: `--max-budget-usd` (default `1.0`, `config.rs:114`) and `timeout_secs` (default `240`, `config.rs:116`). Both are *per child*. There is no bound on the *number* of children. Cost and RAM therefore scale linearly with inbound webhook rate, which is exactly the quantity that goes up during an incident — the failure mode is self-amplifying: more errors → more alerts → more investigations, each of which is a full agentic Claude Code session doing Read/Grep/Glob/`git log` over the repo.
- **Impact**: At 1 alert/min for a flapping project: 60 concurrent-to-overlapping Claude Code sessions per hour at up to $1.00 each = **up to $60/hr per project, unbounded and unattended**. Each session is a Node process plus its tool grandchildren (ripgrep, git); with a 240s timeout, steady state at 1/min is ~4 sessions live at once per project — several GB RSS — and that multiplies by the number of mapped projects, since `Config::projects` is a map with no global cap. Each session also re-reads the same repo and re-derives the same diagnosis, so >90% of that spend is duplicate work.
- **Fix sketch**: Three layers, cheapest first.
  1. **Dedup**: key recent investigations by `(project_id, classify(status, error))` in a small `Mutex<HashMap<Key, Instant>>` and skip if one landed within an `investigate_cooldown_secs` window (default ~15 min). This alone kills the flapping case, because a flap is the *same* error repeatedly.
  2. **Concurrency cap**: put a `tokio::sync::Semaphore` (permits = `max_concurrent_investigations`, default 2) in `AppState` next to the breaker, and `try_acquire_owned()` in `investigate::investigate` — on failure, log-and-skip rather than queue, since a queued investigation of a stale spike has no value.
  3. **Budget ledger**: extend `Breaker` with a rolling-hour *spend* total fed by `ClaudeRun.cost_usd` (already parsed, `claude.rs:86`, currently only rendered into the report) and refuse to spawn past `max_spend_per_hour_usd`. `Breaker` already has the rolling-hour shape in `recent`/`HOUR` — generalize `Vec<Instant>` to `Vec<(Instant, f64)>` and it serves both.
- **Trade-offs**: Dedup can suppress a genuinely new error that shares a classification with a recent one within the cooldown — mitigate by keying on a hash of the error text, not just the class. The semaphore means a burst across *different* projects gets partially dropped; that is the correct trade (an incident on 5 projects at once is a human page, not 5 robots). Metric to watch: investigations spawned vs. suppressed vs. rolling-hour spend, exported alongside the existing per-run cost.

## 2. `run_test` buffers the entire test-suite output into memory, then discards it

- **Severity**: High
- **Category**: unbounded-growth / allocation
- **File**: `crates/responder/src/act.rs:107-122`
- **Scenario**: An auto-fix lands on an `lt-fix/*` branch for a project whose `test_cmd` is a real suite — `cargo test`, `npm test`, `pytest -v`. The responder runs it to decide pass/fail. A verbose suite on a large repo emits tens to hundreds of MB on stdout (cargo test prints every test name; a failing suite prints every backtrace; `pytest -v` on a few thousand tests is comfortably 50MB+).
- **Root cause**: `cmd.output()` captures stdout *and* stderr into `Vec<u8>` in full, growing the buffers as the child streams. The function then reads exactly one bit of it: `o.status.success()`. Every byte captured is allocated, copied, held for the duration of the run, and dropped unread. Neither pipe is configured — only `stdin` is nulled (`act.rs:117`) — so both default to `Stdio::piped()` under `output()`.
- **Impact**: Peak RSS of the responder tracks the noisiest test suite it manages, for the full length of the run (up to `timeout_secs`, default 240s). A 200MB-output suite means a 400MB+ transient spike (both pipe buffers plus reallocation headroom during `Vec` growth) in a service that otherwise idles at a few MB. Combined with finding #1, concurrent acts multiply it. It is also pure waste: the allocation, the copy, and the pipe pumping all serve a value that is thrown away.
- **Fix sketch**: Replace `output()` with `status()` and null both pipes, letting the child's output go nowhere:
  ```rust
  cmd.current_dir(repo)
     .stdin(Stdio::null())
     .stdout(Stdio::null())
     .stderr(Stdio::null())
     .kill_on_drop(true);
  matches!(
      tokio::time::timeout(Duration::from_secs(timeout_secs), cmd.status()).await,
      Ok(Ok(s)) if s.success()
  )
  ```
  Constant memory, no pipe pumping. If the test output turns out to be wanted for the report later (it currently is not — `ActOutcome.tests` is a bare `Option<bool>`, `act.rs:21`), capture a bounded tail instead: pipe stderr only, read into a fixed-size ring buffer of the last ~8KB, and drop the rest.
- **Trade-offs**: Nulling the pipes forfeits the output, so a mysterious test failure gives no local clue — the report says `tests: FAILED` and nothing more. The bounded-tail variant costs ~30 lines and keeps the diagnostic value at a fixed memory ceiling; it is the better end state if anyone ever asks "why did it fail?". Note the same `output()`-buffers-everything shape exists at `claude.rs:50`, but there it is defensible: the stdout *is* the payload and `--max-budget-usd` bounds it indirectly.

## 3. Timeout kills the child but orphans its grandchildren

- **Severity**: Medium
- **Category**: subprocess / resource-cleanup
- **File**: `crates/responder/src/claude.rs:47-59`, `crates/responder/src/act.rs:117-121`
- **Scenario**: An investigation over-explores a large repo and blows the 240s wall clock — the intended, designed-for case, since the comment at `config.rs:26-27` notes this CLI has no `--max-turns` and the timeout is the *only* hard bound on a runaway. `tokio::time::timeout` fires, the `output()` future drops, and `kill_on_drop(true)` reaps the `claude` process.
- **Root cause**: `kill_on_drop` sends a kill to the *direct child only*. Claude Code is an agent that spawns its own tool subprocesses — `git log`, `git diff`, ripgrep for Grep/Glob — and those are not in a process group or a Windows job object owned by the responder. Killing the parent orphans them: they are re-parented (to init/PID 1 on Unix, or simply left running on Windows) and keep executing whatever they were doing. The same applies to `run_test` in `act.rs`, where the direct child is `cmd`/`sh` and the *actual* test runner is the grandchild — killing the shell on timeout typically leaves the test process running to completion.
- **Impact**: Bounded per event but cumulative and silent. A `git log` over a huge history or a ripgrep over a monorepo survives its timeout and keeps burning CPU and page cache with no supervisor. Under repeated timeouts — which is precisely the flapping scenario in finding #1 — orphans accumulate across the responder's lifetime, and the timeout stops being a real bound on resource use even though it correctly bounds the *responder's* view of the run. On Windows the `cmd /C` case is the worst: `cmd` dies instantly and the test suite runs on to completion, holding the repo's files while `act.rs:95` is concurrently trying to `git checkout` the original branch back — a cleanup path that can now fail on locked files.
- **Fix sketch**: Own the whole tree rather than the child.
  - **Windows**: create a Job Object with `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, assign the child at spawn, and drop the job handle on timeout — the OS then terminates the entire tree. `windows-sys`/`win32job` covers this; the responder already has Windows-specific code (`config.rs:146-162`), so the `cfg(windows)` seam exists.
  - **Unix**: `pre_exec(|| { libc::setsid(); Ok(()) })` (or `process_group(0)`, stable since Rust 1.64) to put the child in its own group, then `killpg(-pid, SIGKILL)` on timeout instead of relying on `kill_on_drop`.
  - Wrap this once in `claude.rs` as a `spawn_supervised()` helper and route both `claude::run` and `act::run_test` through it — they have identical needs, and `run_test`'s shell wrapper makes the grandchild problem structural rather than incidental.
- **Trade-offs**: Genuinely platform-specific `unsafe`-adjacent code (`pre_exec` is `unsafe`) and a new dependency on Windows, for a leak that is invisible day-to-day and only compounds under sustained timeouts. Reasonable to sequence after #1 and #2 — fixing #1 reduces how often timeouts fire, which reduces the orphan rate. But note the ordering interaction with `act.rs:95`: as long as the test grandchild can outlive its timeout, the restore-the-user's-branch guarantee in this module's doc comment is not actually guaranteed on Windows.
