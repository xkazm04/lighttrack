# Feature Scout — Calibration Watch & Run Stats

> Total: 3
> Critical: 1 | High: 2 | Medium: 0

## 1. The CI gate is blind to judge trust — an untrusted judge still ships a green build

- **Severity**: Critical
- **Category**: capability-gap
- **File**: `crates/runner/src/gate.rs:6-18`
- **Scenario**: A team wires `lt-runner bench --gate` into CI as the quality gate. In parallel they run `calibrate --watch`, which has been screaming `ALERT untrusted: κ 0.31 < bar 0.60` for a week — the judge model was silently updated and now disagrees with their human labels almost at chance. Every CI run still exits `0` and every PR merges. The gate is confidently reporting `passed` from a judge that is, by the product's own measurement, no better than a coin flip. The scores are green *because* the judge got worse — a broken judge that hands out 0.9s can never "regress" against a baseline.
- **Root cause**: `gate_exit_code` maps only the run's *score* status:
  ```rust
  match status {
      "regressed" => EXIT_REGRESSED,
      "no_baseline" => EXIT_NO_BASELINE,
      _ => 0,
  }
  ```
  Judge trust is never an input. The trust signal is fully computed and persisted — `calibrate_watch.rs:135` even defines `UNTRUSTED_EXIT: i32 = 5` — but it lives in a *different command's* exit path. Grep confirms the reserved rubric `lt:calibration:` (`calibrate_watch.rs:93-95`) is referenced **only inside `calibrate_watch.rs`**: no reader in `bench.rs`, `gate.rs`, or the API's `decide_gate` (`crates/api/src/benchmarks.rs:230-257`, which likewise branches only on `passed`/`regressed`/`no_baseline`). The κ history is written and never consulted by anything that gates. `EXIT_NO_BASELINE` proves the design intent: the gate already distinguishes "unverified" from "regressed" — but only for the *baseline*, never for the *judge*. Calibration is what makes the score credible, and the one place the score becomes a build decision cannot see it.
- **Impact**: Closes the product's central loop. Today calibration is an advisory side-channel; this makes it load-bearing. It converts "we measure judge trust" into "we refuse to gate on an untrusted judge" — the difference between a metric and a guarantee, and the single strongest trust claim an LLM-as-judge product can make.
- **Fix sketch**:
  1. Add `EXIT_UNTRUSTED_JUDGE: i32 = 5` to `gate.rs` (reuse `UNTRUSTED_EXIT`'s value; promote the const here and have `calibrate_watch` import it, so one number means one thing).
  2. Widen the mapper to take trust: `gate_exit_code(status: &str, judge: JudgeTrust) -> i32`, where `JudgeTrust` is `Trusted | Untrusted { kappa, bar } | Unknown { age }`. Untrusted **dominates** a `passed` status (mirroring `DriftLevel`'s existing untrusted-dominates-drift precedence in `assess_drift`).
  3. In `bench.rs`, before the verdict, fetch the latest score under `reserved_rubric(&jp, &jm)` (needs finding #2's rubric filter) and derive `JudgeTrust`. A calibration row older than a `--judge-trust-max-age` (default e.g. 7d) → `Unknown`.
  4. CLI: `--require-trusted-judge` (opt-in first, to protect existing pipelines), later default-on. `Unknown` warns; `Untrusted` fails.
  5. Print the reason: `gate: judge anthropic/haiku untrusted (κ 0.31 < bar 0.60, measured 2h ago) — failing build`. A gate that fails without naming the judge and the κ will just get disabled.
- **Trade-offs**: Adds a read dependency from `bench --gate` to calibration history, so gating needs a calibrated judge — a real onboarding cost. Mitigated by staging it opt-in and treating `Unknown` as warn-not-fail, so nobody's pipeline breaks on upgrade. Distinct exit codes let CI branch rather than hard-fail.

## 2. Drift-vs-previous silently stops working on any project with real scoring traffic

- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/runner/src/calibrate_watch.rs:167-179`
- **Scenario**: An operator runs the sentinel against the same project their benchmarks score into. For the first few days drift detection works. Then the project accumulates benchmark scores, and the sentinel quietly degrades to a bare bar-check forever: it prints κ every cycle with no `Δκ`, and a 0.90 → 0.65 collapse — still above the bar, exactly the early warning the whole module exists to catch — is never reported. Nothing errors. The daemon looks healthy. The `WARN drift` branch has simply become unreachable code.
- **Root cause**: `previous_kappa` pulls a fixed, **unfiltered** page of scores and filters client-side:
  ```rust
  Some(pr) => format!("/v1/scores?project={pr}&limit=500"),
  ...
  scores.into_iter().find(|s| s.rubric == reserved).map(|s| s.value)
  ```
  `/v1/scores` accepts only `project` and `limit` (`crates/api/src/scores.rs:33-46`) — **there is no `rubric` filter**. The sentinel posts *one* calibration row per cycle; `bench.rs:166` posts *one score per case, per run*. A single 500-case benchmark run therefore flushes every calibration row past the 500-row window. `find` returns `None`, and the function's best-effort contract (`a read failure ⇒ None`) makes that indistinguishable from a legitimate first run — so `assess_drift` gets `prev_kappa: None`, `delta` is `None`, and `drifted` is hard-wired `false` (`:81`). The failure is silent by construction. Note `limit` is capped at 1000 server-side, so raising the number is not a fix.

  The same missing filter makes the persisted history **write-only**. `post_calibration` serializes a rich metrics blob — `pearson`, `mae`, `rmse`, `bias`, `agreement_rate`, `human_pass_rate`, `judge_pass_rate`, `n`, `judge_cost_usd` (`:195-200`) — into `reasoning`, and **nothing anywhere parses it back**. `previous_kappa` reads only `s.value`. Every metric except κ is structurally dead: computed, paid for with real judge tokens, stored, never read by any code path.
- **Impact**: Restores the module's headline feature (drift-vs-previous) from unreachable to working, and unlocks data already being paid for. An operator asking the two questions that matter on a drift alert — *"is this a blip or a trend?"* and *"is the judge drifting generous or harsh?"* — currently cannot answer either, despite `bias` and 8 other metrics sitting in the database.
- **Fix sketch**:
  1. **API**: add `rubric: Option<String>` to `ScoresParams` and push it into `list_scores` as a `WHERE rubric = ?`. Small, additive, and the prerequisite for finding #1 too.
  2. **Runner**: `previous_kappa` → `/v1/scores?project={pr}&rubric={reserved}&limit=1`. Correct by construction at any traffic volume, and cheaper.
  3. Distinguish *absent* from *failed*: return `Result<Option<f64>>` so a genuine HTTP error logs `"drift check skipped — history unreadable"` instead of masquerading as a first run. This is the bug that let the regression hide.
  4. **Surface the history**: add `calibrate --history [--limit N]` that pulls the reserved rubric, parses the `reasoning` blob into a typed `CalibrationMetrics` struct, and prints the κ/MAE/bias time-series (a sparkline plus `--json` for dashboards). Give the blob a `serde` struct in `core` beside `Agreement` so writer and reader can't drift apart — the current stringly-typed `json!` blob has no reader to keep it honest.
  5. Once history is queryable, upgrade `assess_drift` to compare against a trailing **median of the last k cycles** rather than the single previous run — one noisy cycle currently both raises a false alarm *and* silently re-baselines the next comparison against itself.
- **Trade-offs**: (1) touches the API crate — but it is a purely additive optional query param. (5) changes alert semantics and deserves its own change with tests; `assess_drift` is pure and already well-covered, so it is cheap to extend safely.

## 3. The watch daemon is a sentinel that cannot raise an alarm

- **Severity**: High
- **Category**: capability-gap
- **File**: `crates/runner/src/calibrate_watch.rs:99-136`
- **Scenario**: An operator deploys `calibrate --watch --interval 86400` as the long-running judge-drift sentinel — the module's headline mode. Three weeks later the judge has degraded past the bar. No webhook fired, no email, no page. The `ALERT untrusted` line is sitting in a container log nobody tails. The daemon's exit code is `0` (it hasn't exited). The sentinel detected the exact condition it was built for and had no way to tell anyone.
- **Root cause**: The module doc asserts the alerting is free:
  > *"Because every `POST /v1/scores` feeds the API's rolling `score_drop` detector, a degrading κ **rides the existing alert channel automatically** — we build no parallel alerting."*

  Verified against `crates/api/src/alerts.rs`, that path cannot realistically fire for calibration:
  - **Sample starvation**: `note_score` returns `None` until the (project, rubric) window holds `score_min_samples` (default 8, floored at 4) — and additionally requires `base_n >= 3` after reserving `recent_k = (len/4).max(3)` (`alerts.rs:400-407`). The sentinel contributes **one** score per cycle. At `--interval 86400` that is **8+ days** before the first alert is even eligible.
  - **In-memory window**: `score_windows: Mutex<HashMap<String, VecDeque<f64>>>` (`alerts.rs:105`) is process state, built only from live `record_score` calls and **never rehydrated from the store**. Any API restart or deploy resets the count to zero. A daily sentinel racing an 8-sample threshold against the API's uptime will, in practice, never arm.
  - **Wrong shape**: `score_drop` is a *relative* drop of a recent tail vs a trailing mean within one window — it is built for high-frequency production scores. It has no concept of the trust **bar**, so a judge parked below the bar at a steady κ=0.2 never trips it, and `baseline <= 0.0` bails outright (`alerts.rs:410-412`) — reachable since κ is legitimately ≤0 and `record_score` clamps negatives to 0.0 (`:368`).

  The runner's own immediate check — the documented mitigation ("cron gets a non-zero exit on the very next bad run") — is `--once`-only. In daemon mode `watch()` assigns `last = level` each cycle and then discards it: the return is gated `if p.once && last == DriftLevel::Untrusted` (`:135`). `last` is **dead in the daemon path**. Drift and Untrusted reach `eprintln!` and stop there. So the two modes have inverted capabilities: `--once` (cron, already has a supervisor watching exit codes) gets the signal, while the daemon (nothing watching it) gets none.
- **Impact**: Makes the flagship mode actually operable. Right now the honest recommendation is "don't use `--watch`, use `--once` under cron" — the daemon is a strictly worse product with no way to page a human. This is the difference between a dashboard you must remember to check and a sentinel that wakes you up.
- **Fix sketch**:
  1. Give calibration a **first-class alert**, not a piggyback. Add `POST /v1/alerts/calibration` (or extend the bench-webhook path — `notify_bench_run` at `alerts.rs:309-318` is the right shape already: cooldown-deduped, off the request path, no rolling window, fires on a **single** event). Payload: judge, κ, bar, prev κ, Δ, level.
  2. Runner posts it on `DriftLevel::{Drift, Untrusted}` in `run_cycle`, best-effort — an alert failure must not kill the cycle, matching the existing `Err(e) => eprintln!` resilience at `:127-128`.
  3. Dedup key `calibration:{jp}/{jm}:{level}` so a sustained outage doesn't spam, and a `Drift → Untrusted` escalation still gets through (the same independent-key trick `warn_key` already uses for warnings vs breaches at `alerts.rs:434-436`).
  4. Fire a **recovery** notice on `Untrusted → Ok`. An alert channel that never says "resolved" trains operators to ignore it.
  5. Correct the module doc — the "rides the existing alert channel automatically" claim is what stopped this from being built.
- **Trade-offs**: Contradicts the module's explicit "we build no parallel alerting" design constraint. That constraint was correct in spirit but rests on a false premise: the `score_drop` detector is frequency-based and calibration is a low-frequency, absolute-threshold signal — the shapes genuinely don't match. Reusing `Alerter`'s cooldown + channel fan-out (rather than a new delivery path) keeps the spirit of the constraint at ~40 lines, with no new config surface.
