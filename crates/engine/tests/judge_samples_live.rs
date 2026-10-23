//! Rubric self-consistency against a **real** seeded model, through the real engine dispatch.
//!
//! Ignored by default: it needs a local OpenAI-compatible server that honours `temperature` and
//! `seed` (Ollama works) and a small chat model, so it can never be a gate. Run it on purpose:
//!
//! `LIGHTTRACK_OPENAI_BASE=http://localhost:11434 OPENAI_API_KEY=local LT_LIVE_JUDGE_MODEL=qwen2.5:1.5b`
//! `cargo test -p lighttrack-engine --test judge_samples_live -- --ignored --nocapture`
//!
//! It exists because `agreement` is the one reliability number a sampled verdict carries, and the
//! unit tests can only feed it canned replies that differ because the fixture says so. Whether the
//! samples a real provider returns can differ at all is a property of the request, not of the
//! aggregation: N draws pinned to one temperature and one seed over one prompt are one draw billed N
//! times, and their agreement reads 1.0 however ambiguous the case is.

use lighttrack_core::{Rubric, RubricDimension};
use lighttrack_engine::{run_rubric_judge, EngineConfig};

fn dim(key: &str, description: &str, anchors: &[&str]) -> RubricDimension {
    serde_json::from_value(serde_json::json!({
        "key": key,
        "description": description,
        "anchors": anchors,
    }))
    .expect("dimension literal")
}

/// Grounding cases chosen to be ambiguous: partly supported, hedged, or over-reaching, so a judge
/// that genuinely samples has room to disagree with itself.
const CASES: &[(&str, &str)] = &[
    (
        "Context: The bridge opened in 1932 and was widened in 1968.\nQuestion: When did the bridge open and who designed it?",
        "It opened in 1932, designed by a local firm, and was widened later.",
    ),
    (
        "Context: The trial enrolled 40 adults; the drug lowered blood pressure in most of them.\nQuestion: Does the drug work?",
        "Yes, the drug is proven to lower blood pressure in adults.",
    ),
    (
        "Context: Revenue rose 4% while costs rose 9%.\nQuestion: Was it a good year?",
        "Revenue grew, so it was a good year overall, though costs also increased.",
    ),
    (
        "Context: The library is open weekdays 9-5. Weekend hours vary by season.\nQuestion: Is it open Saturday?",
        "It may be open on Saturday depending on the season.",
    ),
];

#[test]
#[ignore = "needs a local seeded OpenAI-compatible model; run with --ignored"]
fn sampled_rubric_verdicts_can_disagree_with_themselves() {
    let model = std::env::var("LT_LIVE_JUDGE_MODEL").unwrap_or_else(|_| "qwen2.5:1.5b".into());
    let mut rubric: Rubric = serde_json::from_value(serde_json::json!({
        "id": "live", "project_id": "live", "name": "grounding", "dimensions": [],
    }))
    .expect("rubric literal");
    rubric.dimensions = vec![
        dim(
            "faithfulness",
            "every claim in the answer is supported by the context",
            &[
                "1.0 = every claim supported",
                "0.5 = some claim unsupported",
                "0.0 = contradicts the context",
            ],
        ),
        dim(
            "completeness",
            "the answer addresses every part of the question",
            &["1.0 = all parts", "0.5 = some parts", "0.0 = none"],
        ),
    ];
    let samples = 5;
    let mut split = 0;
    for (i, (input, output)) in CASES.iter().enumerate() {
        let out = run_rubric_judge(
            &EngineConfig::default(),
            "openai",
            &model,
            &rubric,
            input,
            None,
            output,
            samples,
            1,
        )
        .expect("a local model judges");
        eprintln!(
            "case {i}: overall={:.3} agreement={:.3} samples={} parsed={} determinism={}",
            out.overall,
            out.agreement,
            out.samples,
            out.samples_parsed,
            out.determinism.as_str()
        );
        if out.agreement < 1.0 {
            split += 1;
        }
    }
    eprintln!("cases whose samples disagreed: {split}/{}", CASES.len());
}
