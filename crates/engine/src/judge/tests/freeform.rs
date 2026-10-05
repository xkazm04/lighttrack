//! The freeform contract's own tests: pass is computed here, the counters are measurements, and the
//! criteria carry an identity. Driven through the [`Generator`] seam with the deterministic fake, so
//! none of this spends a provider call.

use super::*;
use crate::judge::freeform::{
    freeform_with, FreeformContract, FreeformOutcome, DEFAULT_FREEFORM_THRESHOLD,
};
use crate::{build_eval_prompt, build_freeform_prompt};

/// A verdict whose model JSON claims `"pass": true` at 0.9/1.0, judged under a 0.95 bar.
const CONFIDENT_PASS: &str = r#"{"score":0.9,"max":1.0,"pass":true,"reasoning":"looks fine"}"#;

fn judge(contract: &FreeformContract, outputs: &[&str], samples: u32) -> Result<FreeformOutcome> {
    freeform_with(
        &FakeGen::new(outputs),
        contract,
        &Prompt::plain("prompt"),
        "fake-model",
        samples,
        1,
    )
}

/// **The dictated verdict.** The judge model's own `pass` is not the gate: the contract compares the
/// normalized score against its own threshold. The old path persisted `verdict.pass` straight
/// through (`bench.rs` / `judge_spec.rs`), so a model that said "pass" wrote the gate's answer.
#[test]
fn the_contract_computes_pass_not_the_model() {
    let strict = FreeformContract::with_threshold("be exact", 0.95);
    let out = judge(&strict, &[CONFIDENT_PASS], 1).unwrap();
    assert!(
        (out.overall - 0.9).abs() < 1e-9,
        "the score is the model's: {}",
        out.overall
    );
    assert!(out.verdict.pass, "the model's own claim is preserved");
    assert!(
        !out.pass,
        "0.9 under a 0.95 bar must not pass — the contract decides, not the judge model"
    );
    assert!(out.model_dissented(), "the disagreement is visible");

    // The same verdict under a bar it clears passes, and the dissent disappears.
    let lenient = FreeformContract::with_threshold("be exact", 0.85);
    let out = judge(&lenient, &[CONFIDENT_PASS], 1).unwrap();
    assert!(out.pass);
    assert!(!out.model_dissented());

    // A model that denies a passing score does not get to fail the case either.
    let denied = r#"{"score":1.0,"max":1.0,"pass":false,"reasoning":"I disapprove"}"#;
    let out = judge(&lenient, &[denied], 1).unwrap();
    assert!(out.pass, "a dictated fail is not a gate either");

    // `max` is the model's scale, so the comparison is against the normalized score.
    let five_scale = r#"{"score":4.0,"max":5.0,"pass":true,"reasoning":"4 of 5"}"#;
    let out = judge(
        &FreeformContract::with_threshold("x", 0.9),
        &[five_scale],
        1,
    )
    .unwrap();
    assert!(!out.pass, "0.8 normalized is under a 0.9 bar");
}

/// **A bad sample is a counted drop, not a dead run.** Two samples, one unparseable even after the
/// repair re-ask: the case still yields a verdict, and says how many samples survived.
#[test]
fn one_unparseable_sample_is_counted_and_the_case_survives() {
    let c = FreeformContract::with_threshold("be helpful", 0.5);
    // FakeGen is index-based and the repair re-ask reuses the index, so sample 1 stays bad.
    let out = judge(
        &c,
        &[
            r#"{"score":0.8,"max":1.0,"pass":true,"reasoning":"good"}"#,
            "the model rambled and produced no json at all",
        ],
        2,
    )
    .unwrap();
    assert_eq!(out.samples, 2, "samples requested");
    assert_eq!(out.samples_parsed, 1, "samples that produced a verdict");
    assert_eq!(out.parse_failures, 1, "the drop is counted, not hidden");
    assert!(
        (out.overall - 0.8).abs() < 1e-9,
        "the mean covers the survivors only"
    );
    assert!(out.pass);

    // Every sample unparseable is still an error — the same refusal `aggregate` makes rather than
    // recording a confident-looking 0.0 fail.
    let err = judge(&c, &["no json here", "still no json"], 2).unwrap_err();
    assert!(
        matches!(err, EngineError::Parse(_)),
        "no parseable sample must not become a score: {err:?}"
    );

    // A repaired sample is not a failure: the first attempt was bad, the re-ask was good.
    let repaired = freeform_with(
        &RepairGen {
            good: r#"{"score":0.6,"max":1.0,"pass":true,"reasoning":"ok"}"#.into(),
        },
        &c,
        &Prompt::plain("prompt"),
        "fake-model",
        1,
        1,
    )
    .unwrap();
    assert_eq!(repaired.parse_failures, 0);
    assert_eq!(repaired.samples_parsed, 1);
}

/// **The flattered agreement.** `agreement` is the spread of the samples that scored, not the
/// literal `1.0` the old benchmark arm wrote for an unsampled draw.
#[test]
fn agreement_is_the_spread_of_the_samples_that_scored() {
    let c = FreeformContract::with_threshold("be helpful", 0.5);
    let out = judge(
        &c,
        &[
            r#"{"score":0.9,"max":1.0,"pass":true,"reasoning":"great"}"#,
            r#"{"score":0.4,"max":1.0,"pass":false,"reasoning":"poor"}"#,
        ],
        2,
    )
    .unwrap();
    assert!(
        (out.agreement - 0.5).abs() < 1e-9,
        "1 - (0.9 - 0.4) = 0.5, got {}",
        out.agreement
    );
    assert!((out.overall - 0.65).abs() < 1e-9);
    assert!(
        out.agreement <= 0.7,
        "this verdict must be reachable by the `low_agreement` triage reason"
    );

    // One sample has nothing to disagree with.
    let lone = judge(
        &c,
        &[r#"{"score":0.9,"max":1.0,"pass":true,"reasoning":"x"}"#],
        1,
    )
    .unwrap();
    assert_eq!(lone.agreement, 1.0);

    // Bounded parallelism must not move any aggregate.
    let outputs = [
        r#"{"score":0.9,"max":1.0,"pass":true,"reasoning":"a"}"#,
        r#"{"score":0.4,"max":1.0,"pass":false,"reasoning":"b"}"#,
        r#"{"score":0.7,"max":1.0,"pass":true,"reasoning":"c"}"#,
    ];
    let seq = judge(&c, &outputs, 3).unwrap();
    let par = freeform_with(
        &FakeGen::new(&outputs),
        &c,
        &Prompt::plain("prompt"),
        "fake-model",
        3,
        3,
    )
    .unwrap();
    assert_eq!(seq.overall, par.overall);
    assert_eq!(seq.agreement, par.agreement);
    assert_eq!(seq.pass, par.pass);
    assert_eq!(seq.parse_failures, par.parse_failures);
}

/// **The paraphrased rubric.** Criteria text carries a stable identity, so two runs under edited
/// criteria are distinguishable by query. `judge_spec.rs` used to stamp `rubric_id: None` on every
/// freeform verdict.
#[test]
fn criteria_carry_a_stable_identity() {
    let a = FreeformContract::new("judge for factual accuracy");
    let again = FreeformContract::new("judge for factual accuracy");
    let edited = FreeformContract::new("judge for factual accuracy and tone");

    assert_eq!(
        a.id(),
        again.id(),
        "the same criteria are the same instrument"
    );
    assert_ne!(
        a.id(),
        edited.id(),
        "an edited word is a different instrument"
    );
    assert!(
        a.id().starts_with("freeform:"),
        "a derived identity must be tellable from a stored rubric id: {}",
        a.id()
    );
    // The threshold is not part of the identity: the criteria are the instrument, the bar is policy.
    assert_eq!(
        a.id(),
        FreeformContract::with_threshold(a.criteria(), 0.1).id()
    );

    // It is stamped onto the verdict, so the stored row names what scored it.
    let out = judge(&a, &[CONFIDENT_PASS], 1).unwrap();
    assert_eq!(out.contract_id, a.id());
    assert_eq!(out.threshold, DEFAULT_FREEFORM_THRESHOLD);

    // A clamped threshold is a configuration error caught at construction, not a gate nothing clears.
    assert_eq!(FreeformContract::with_threshold("x", 7.0).threshold(), 1.0);
    assert_eq!(FreeformContract::with_threshold("x", -1.0).threshold(), 0.0);
}

/// **One instrument.** The prompt calibration builds is the prompt the benchmark sends: the
/// reference-answer block is present iff the case carries one. `calibrate.rs` used to judge through
/// `build_judge_prompt`, whose signature had no `expected` at all — so the kappa that mints a
/// judge-trust verdict was measured on a prompt production never sent.
#[test]
fn the_freeform_prompt_carries_the_reference_when_the_case_has_one() {
    let c = FreeformContract::new("be accurate");
    let with_ref = build_freeform_prompt(&c, "q", Some("the reference answer"), "a");
    assert!(
        with_ref.text.contains("REFERENCE / EXPECTED ANSWER"),
        "the reference block is missing from the judged prompt"
    );
    assert!(with_ref.text.contains("and the reference answer"));

    let without = build_freeform_prompt(&c, "q", None, "a");
    assert!(!without.text.contains("REFERENCE / EXPECTED ANSWER"));

    // And it is the same builder the benchmark arm uses — byte-identical, not merely similar in
    // shape. A second paraphrase of the same criteria is a second instrument.
    let bench_shaped = build_eval_prompt(c.criteria(), "q", Some("the reference answer"), "a");
    assert_eq!(
        instruction_shape(&with_ref.text),
        instruction_shape(&bench_shaped.text),
        "calibration and the benchmark must judge through one prompt"
    );
}

/// The fence mints a fresh nonce per build, so two prompts over the same content differ only in
/// their delimiters. Strip anything containing the nonce marker to compare the instruction text.
fn instruction_shape(text: &str) -> String {
    text.lines()
        .filter(|l| !l.contains("<<<LT:"))
        .collect::<Vec<_>>()
        .join("\n")
}
