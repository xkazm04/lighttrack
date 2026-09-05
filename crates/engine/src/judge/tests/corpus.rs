//! The judge's own eval: `evals/judge/corpus.json` replayed through the real scoring path.
//!
//! Every other test in this crate asserts a *behaviour* ("a sub-floor dimension fails the case").
//! None of them held a whole verdict still, so a reworded prompt, a re-ordered narration, or a
//! change to how samples are folded could move what a score MEANS with the suite entirely green.
//! For a tool whose product is the score, that is the regression that matters most and was the one
//! nothing watched.
//!
//! The corpus is closed over the model: judge replies are canned, so a red run here is always a
//! change made in this repository and never an upstream model drifting. Live-provider agreement is a
//! different measurement and lives in the benchmark lane (`docs/CALIBRATION.md`) — it cannot be a
//! gate, because it can go red without anyone touching the code.
//!
//! This runs inside `cargo test --workspace`, which blocks CI (see `.ai/manifest.yaml`
//! `controls.ciHardPass`), so a verdict cannot move without a diff to `corpus.json` in the same
//! change to argue for it.

use std::collections::BTreeSet;

use super::*;

/// Read at compile time: a missing or renamed corpus is a build error, never a silently skipped eval.
const CORPUS_JSON: &str = include_str!("../../../evals/judge/corpus.json");

const EPS: f64 = 1e-9;

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < EPS
}

fn cases() -> Vec<Value> {
    let doc: Value =
        serde_json::from_str(CORPUS_JSON).expect("evals/judge/corpus.json is valid JSON");
    doc["cases"]
        .as_array()
        .expect("the corpus has a `cases` array")
        .clone()
}

/// A required string field, named in the panic so a malformed corpus points at itself.
fn s<'a>(v: &'a Value, key: &str) -> &'a str {
    v[key]
        .as_str()
        .unwrap_or_else(|| panic!("corpus field `{key}` must be a string, in: {v}"))
}

/// An optional list-of-strings field. A missing key is an empty list, not an error.
fn strings(v: &Value) -> Vec<&str> {
    v.as_array()
        .map(|a| {
            a.iter()
                .map(|x| x.as_str().expect("corpus list entries are strings"))
                .collect()
        })
        .unwrap_or_default()
}

/// Judge one corpus case exactly as `run_rubric_judge` would, with the provider call replaced by the
/// case's canned replies. Everything else — deterministic scorers, prompt build, fence, aggregation
/// — is the production path.
fn run(case: &Value) -> (RubricOutcome, Prompt) {
    let id = s(case, "id");
    let r: Rubric = serde_json::from_value(case["rubric"].clone())
        .unwrap_or_else(|e| panic!("case `{id}`: rubric does not deserialize: {e}"));
    let input = s(case, "input");
    let output = s(case, "output");
    let expected = case["expected"].as_str();
    let det = crate::scorers::evaluate_all(&r, expected, output)
        .unwrap_or_else(|e| panic!("case `{id}`: deterministic dimensions failed: {e:?}"));
    let prompt = build_rubric_prompt(&r, input, expected, output);
    let outputs = strings(&case["judge_outputs"]);
    let samples = case["samples"]
        .as_u64()
        .unwrap_or_else(|| panic!("case `{id}`: `samples` must be a number")) as u32;
    let outcome = judge_with(
        &FakeGen::new(&outputs),
        &r,
        &prompt,
        "judge-model",
        samples,
        1,
        &det,
    )
    .unwrap_or_else(|e| panic!("case `{id}`: judging failed: {e:?}"));
    (outcome, prompt)
}

#[test]
fn every_corpus_case_still_produces_its_recorded_verdict() {
    let cases = cases();
    assert!(
        cases.len() >= 5,
        "the corpus holds only {} cases — too few to notice a scoring change",
        cases.len()
    );
    let mut seen = BTreeSet::new();

    for case in &cases {
        let id = s(case, "id");
        assert!(seen.insert(id), "duplicate corpus case id `{id}`");
        assert!(
            !s(case, "why").is_empty(),
            "case `{id}` records no `why` — a case nobody can justify cannot be refreshed honestly"
        );

        let (out, prompt) = run(case);
        let want = &case["verdict"];

        for (field, got) in [("overall", out.overall), ("agreement", out.agreement)] {
            let expect = want[field]
                .as_f64()
                .unwrap_or_else(|| panic!("case `{id}`: verdict.{field} must be a number"));
            assert!(
                close(got, expect),
                "case `{id}`: {field} is {got}, the corpus records {expect}.\n\
                 If the new number is right, say why in the same change — this is what a score means."
            );
        }

        for (field, got) in [
            ("samples", out.samples),
            ("samples_parsed", out.samples_parsed),
            ("parse_failures", out.parse_failures),
        ] {
            let expect = want[field]
                .as_u64()
                .unwrap_or_else(|| panic!("case `{id}`: verdict.{field} must be a number"))
                as u32;
            assert_eq!(got, expect, "case `{id}`: {field}");
        }

        assert_eq!(
            out.pass,
            want["pass"].as_bool().expect("verdict.pass is a bool"),
            "case `{id}`: the gate flipped"
        );
        assert_eq!(out.model, s(want, "model"), "case `{id}`: scoring model");

        let injected = want["injection_suspected"]
            .as_bool()
            .expect("verdict.injection_suspected is a bool");
        assert_eq!(
            prompt.injection_suspected, injected,
            "case `{id}`: the fence's build-time signal"
        );
        assert_eq!(
            out.injection_suspected, injected,
            "case `{id}`: the signal must survive onto the outcome — a score nobody knows was \
             attacked is the failure mode"
        );

        let dims = want["dimensions"]
            .as_object()
            .unwrap_or_else(|| panic!("case `{id}`: verdict.dimensions must be an object"));
        assert_eq!(
            out.dimensions.len(),
            dims.len(),
            "case `{id}`: dimension count"
        );
        for (key, d) in dims {
            let got = out
                .dimensions
                .iter()
                .find(|x| &x.key == key)
                .unwrap_or_else(|| panic!("case `{id}`: the outcome has no dimension `{key}`"));
            let score = d["score"].as_f64().expect("dimension score is a number");
            assert!(
                close(got.score, score),
                "case `{id}`: dimension `{key}` scored {}, the corpus records {score}",
                got.score
            );
            assert_eq!(
                got.floor_hit,
                d["floor_hit"].as_bool().expect("floor_hit is a bool"),
                "case `{id}`: dimension `{key}` floor_hit"
            );
        }

        // The prompt is the other half of what a verdict means: the same math over a differently
        // worded instruction is a different measurement.
        for needle in strings(&case["prompt_must_contain"]) {
            assert!(
                prompt.text.contains(needle),
                "case `{id}`: the judge prompt no longer contains {needle:?}.\n\
                 Rewording the prompt changes what the judge is being asked — update the corpus in \
                 the same change, deliberately."
            );
        }
        for needle in strings(&case["prompt_must_not_contain"]) {
            assert!(
                !prompt.text.contains(needle),
                "case `{id}`: the judge prompt now contains {needle:?}, which it must not"
            );
        }
    }
}

/// A corpus that cannot go red is a diary. Perturb the scoring input and prove the recorded verdict
/// stops holding — otherwise a future refactor could make `run` return anything and stay green.
#[test]
fn the_corpus_is_a_gate_not_a_diary() {
    let mut case = cases()
        .into_iter()
        .find(|c| c["id"] == "weighted-mean-over-samples")
        .expect("the base weighting case is in the corpus");
    let recorded = case["verdict"]["overall"]
        .as_f64()
        .expect("the base case records an overall");

    // The heaviest dimension drops to parity: (0.7 + 0.4) / 2 = 0.55, not 0.625.
    case["rubric"]["dimensions"][0]["weight"] = serde_json::json!(1.0);
    let (out, _) = run(&case);
    assert!(
        !close(out.overall, recorded),
        "re-weighting a rubric left the overall at {recorded} — this eval is not measuring the \
         scoring math it claims to"
    );
}

/// The prompt half only binds if cases actually pin phrases. A corpus that recorded verdicts and
/// nothing about the prompt would let a full rewrite of the judge's instructions through.
#[test]
fn the_prompt_expectations_are_not_vacuous() {
    let n: usize = cases()
        .iter()
        .map(|c| {
            strings(&c["prompt_must_contain"]).len() + strings(&c["prompt_must_not_contain"]).len()
        })
        .sum();
    assert!(
        n >= 8,
        "only {n} prompt expectations across the whole corpus — a reworded judge prompt would slip \
         through with every verdict still green"
    );
}
