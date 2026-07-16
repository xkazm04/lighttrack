# Feature Scout — Judge Engine

> Total: 3
> Critical: 0 | High: 2 | Medium: 1 | Low: 0

## 1. Judge verdicts are not reproducible — no temperature/seed controls
- **Severity**: High
- **Category**: trust
- **File**: `crates/engine/src/providers.rs:248,187-192` (OpenAI/Gemini bodies), `crates/engine/src/claude.rs:22-41` (CLI args)
- **Scenario**: An operator re-runs a benchmark or re-scores the same calibration item and gets a different pass/fail than yesterday. They cannot tell whether their prompt/rubric change moved the score or whether the judge just sampled differently. A regression gate flips red on a re-run with no code change.
- **Root cause**: Every provider dispatch fires at the model's *default* temperature with no seed. The OpenAI body is only `{"model","messages"}` (+`response_format`) — no `temperature`, no `seed` (`providers.rs:248`). Gemini's `generationConfig` is set **only** when a schema is present and carries only `responseMimeType`/`responseSchema` — no `temperature` (`providers.rs:187-192`). The `claude -p` command builds `-p/--model/--effort/--json-schema` but never a temperature flag (`claude.rs:22-41`). For an eval product, "can you reproduce a verdict" is a headline credibility claim (the crate docs stress reproducibility), yet nothing pins the sampler. Self-consistency `agreement` (`judge.rs:305`) is also confounded: cross-sample spread reflects sampler noise, not genuine judge uncertainty.
- **Impact**: Reproducible scoring is table stakes vs Braintrust/LangSmith/Langfuse; gates built on a non-deterministic judge produce flaky CI and erode trust in every downstream number.
- **Fix sketch**: Add `temperature: Option<f64>` (default `0.0`) and `seed: Option<u64>` to `EngineConfig` (or the judge call sig); inject `temperature`/`seed` into the OpenAI body, `generationConfig.temperature` into Gemini (always, not only under schema), and `--temperature` (if the CLI exposes it) for Claude. Document that self-consistency (`samples > 1`) should deliberately raise temperature, so the two knobs stay orthogonal.
- **Trade-offs**: Temperature 0 slightly reduces diversity for intentional self-consistency runs — mitigated by keeping the knob per-call. Provider seed support is best-effort (OpenAI honors it; Gemini/Claude may not) — treat as advisory.

## 2. Calibration measures judge bias but nothing can act on it — feedback loop dead-ends
- **Severity**: High
- **Category**: half-implemented
- **File**: `crates/core/src/calibration.rs:36,88` (`bias` computed), `crates/runner/src/calibrate.rs:179-185` (only prints advice), `crates/engine/src/judge.rs:286-302` (scoring never consumes bias)
- **Scenario**: An operator runs `calibrate`, sees "judge is more generous than humans by 0.18 — consider tightening the rubric," and then… has no lever. Their only recourse is to hand-edit rubric prose and re-calibrate by trial and error. The measured `bias`/`trusted`/`cohen_kappa` are serialized into the report (`calibrate.rs:57-66`) and read by a human, but never flow back into the judging path.
- **Root cause**: `Agreement.bias` is a fully-computed, well-defined number (`calibration.rs:88`), but the judge's overall/pass computation (`judge.rs:286-302`) has no notion of a calibration offset — it takes raw model scores at face value. The capability is structurally one-directional: measure, print, stop. There is no persisted "calibration profile" (offset/scale + κ + trusted flag) that a subsequent `run_rubric_judge` could apply or that gating could require.
- **Impact**: Bias *correction* (and refusing to gate on an un-`trusted` rubric) is exactly the reliability surface competitors sell. Turning the existing measurement into an applied correction converts a diagnostic into a product feature with near-zero new math.
- **Fix sketch**: Persist a `CalibrationProfile { offset, scale, kappa, trusted, threshold }` keyed by rubric+judge (reuse the report JSON as the store). Add an opt-in `apply_calibration` path in `run_rubric_judge`/`run_judge` that shifts the overall by `-bias` (clamped to 0..1) before the pass gate, and a gating flag that errors/warns when `!trusted`. Surface "calibrated" vs "raw" in `RubricOutcome` so the correction is auditable, never silent.
- **Trade-offs**: A global scalar offset is a first-order fix; heavy per-dimension miscalibration needs per-dimension human labels (see the CalibrationItem note below). Applying a correction must be explicit and logged so verdicts stay debuggable.

## 3. Self-consistency loses all but the first sample's reasoning
- **Severity**: Medium
- **Category**: trust
- **File**: `crates/engine/src/judge.rs:240-245`
- **Scenario**: An operator runs a rubric judge with `samples = 5` to get a robust score, sees `overall = 0.62` with `agreement = 0.4` (the samples disagreed sharply), and opens the verdict to understand why. Every dimension's `reasoning` is from **sample #1 only** — which may have scored 0.9 while the reported mean is 0.62. The explanation contradicts the number.
- **Root cause**: In `aggregate`, reasoning is captured under `if !have_reasonings` and `have_reasonings` is latched `true` after the first parsed sample (`judge.rs:240-245`). Scores are averaged across all k samples, but `DimScore.reasoning` carries a single unrepresentative sample's text. The richest debugging signal in a self-consistency run — *why the judges disagreed* — is discarded exactly when it matters most (low agreement).
- **Impact**: "Can you debug a verdict" is core to judge credibility. A mean score with mismatched reasoning actively misleads; users can't see the dissent that the `agreement` metric is flagging.
- **Fix sketch**: Either (a) pick the reasoning from the sample nearest the per-dimension mean (median sample), or (b) collect all k reasonings per dimension into a `Vec<String>` on `DimScore` so the UI/report can show the spread. Option (b) preserves the most information and pairs naturally with the `agreement` metric.
- **Trade-offs**: Retaining all reasonings grows `RubricOutcome` payload (bounded by k, typically ≤5) — acceptable; the median-pick option is cheaper but still shows only one voice.
