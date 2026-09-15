use std::sync::Mutex;
use std::time::Duration;

use serde_json::Value;

use lighttrack_engine::{Determinism, EngineError, GenOutcome, SchemaEnforcement};

use super::*;

/// A scripted generator: each provider answers with the next result in its queue.
pub(crate) struct Script {
    pub calls: Mutex<Vec<String>>,
    pub script: Mutex<Vec<(String, Result<String, EngineError>)>>,
}

impl Script {
    pub(crate) fn new(script: Vec<(&str, Result<&str, EngineError>)>) -> Script {
        Script {
            calls: Mutex::new(Vec::new()),
            script: Mutex::new(
                script
                    .into_iter()
                    .map(|(p, r)| (p.to_string(), r.map(str::to_string)))
                    .collect(),
            ),
        }
    }
}

pub(crate) fn outcome(text: &str, model: &str) -> GenOutcome {
    GenOutcome {
        output: text.to_string(),
        cost_usd: Some(0.01),
        model: model.to_string(),
        latency_ms: Some(5),
        input_tokens: Some(10),
        output_tokens: Some(3),
        reasoning_tokens: None,
        determinism: Determinism::BestEffort,
        schema: SchemaEnforcement::NotRequested,
    }
}

impl Generator for Script {
    fn generate(
        &self,
        target: &Target,
        _system: Option<&str>,
        _input: &str,
        _schema: Option<&Value>,
    ) -> lighttrack_engine::Result<GenOutcome> {
        self.calls.lock().unwrap().push(target.spec());
        let mut script = self.script.lock().unwrap();
        let pos = script
            .iter()
            .position(|(p, _)| *p == target.provider)
            .unwrap_or_else(|| panic!("no scripted answer for {}", target.provider));
        let (_, r) = script.remove(pos);
        r.map(|t| outcome(&t, &target.model))
    }
}

fn chain() -> Vec<Target> {
    vec![
        Target::parse("anthropic/claude-sonnet-5").unwrap(),
        Target::parse("codex/gpt-5.5").unwrap(),
    ]
}

fn prompt() -> Prompt {
    Prompt {
        system: None,
        input: "hi".into(),
        schema: None,
        transcript: false,
    }
}

fn limit() -> EngineError {
    EngineError::RateLimited {
        who: "claude".into(),
        retry_after: None,
    }
}

#[test]
fn classification_reads_the_seat_out_of_a_cli_stderr() {
    let hold = Duration::from_secs(300);
    let nz = |s: &str| EngineError::NonZero {
        code: 1,
        stderr: s.into(),
    };
    assert_eq!(
        classify(&nz("Claude usage limit reached. Resets at 3pm"), hold),
        Verdict::Exhausted(hold)
    );
    assert_eq!(
        classify(&nz("Not logged in · Please run /login"), hold),
        Verdict::Exhausted(hold)
    );
    assert_eq!(classify(&nz("segfault"), hold), Verdict::Transient);
    assert_eq!(
        classify(
            &EngineError::RateLimited {
                who: "x".into(),
                retry_after: Some(Duration::from_secs(42))
            },
            hold
        ),
        Verdict::Exhausted(Duration::from_secs(42))
    );
    assert_eq!(
        classify(
            &EngineError::BadRequest {
                who: "x".into(),
                status: 400,
                body: String::new()
            },
            hold
        ),
        Verdict::Terminal
    );
}

#[test]
fn a_usage_limit_falls_over_and_holds_the_seat_for_the_next_call() {
    let gen = Script::new(vec![
        ("anthropic", Err(limit())),
        ("codex", Ok("from codex")),
        ("codex", Ok("again")),
    ]);
    let cooldowns = Cooldowns::default();
    let run = ChainRun {
        gen: &gen,
        cooldowns: &cooldowns,
        default_hold: Duration::from_secs(300),
        simulate_exhausted: None,
    };
    let r = run.run(&chain(), &prompt());
    assert!(r.fell_back());
    assert_eq!(r.outcome().unwrap().1.output, "from codex");
    assert_eq!(r.attempts.len(), 2);

    // Second call: the primary is not even tried.
    let r = run.run(&chain(), &prompt());
    assert!(matches!(
        r.attempts[0].kind,
        AttemptKind::SkippedCooling { .. }
    ));
    assert_eq!(r.outcome().unwrap().1.output, "again");
    assert_eq!(gen.calls.lock().unwrap().len(), 3);
}

#[test]
fn a_terminal_failure_stops_the_chain_without_spending_the_second_seat() {
    let gen = Script::new(vec![(
        "anthropic",
        Err(EngineError::BadRequest {
            who: "claude".into(),
            status: 400,
            body: "bad schema".into(),
        }),
    )]);
    let cooldowns = Cooldowns::default();
    let run = ChainRun {
        gen: &gen,
        cooldowns: &cooldowns,
        default_hold: Duration::from_secs(300),
        simulate_exhausted: None,
    };
    let r = run.run(&chain(), &prompt());
    assert!(r.served.is_none());
    assert_eq!(r.attempts.len(), 1);
    assert_eq!(gen.calls.lock().unwrap().len(), 1);
    assert_eq!(cooldowns.snapshot().len(), 0);
}

#[test]
fn a_success_clears_an_earlier_hold_and_a_simulated_limit_never_calls_the_seat() {
    let gen = Script::new(vec![("codex", Ok("ok")), ("anthropic", Ok("back"))]);
    let cooldowns = Cooldowns::default();
    let run = ChainRun {
        gen: &gen,
        cooldowns: &cooldowns,
        default_hold: Duration::from_secs(300),
        simulate_exhausted: Some("anthropic"),
    };
    let r = run.run(&chain(), &prompt());
    assert!(r.fell_back());
    assert_eq!(cooldowns.snapshot().len(), 1);
    // Only codex was actually called: the simulation short-circuits the seat.
    assert_eq!(gen.calls.lock().unwrap().as_slice(), ["codex/gpt-5.5"]);

    cooldowns.clear("anthropic");
    let run = ChainRun {
        simulate_exhausted: None,
        ..run
    };
    let r = run.run(&chain(), &prompt());
    assert!(!r.fell_back());
    assert_eq!(cooldowns.snapshot().len(), 0);
    let rep = report(&r);
    assert_eq!(rep[0]["outcome"], "served");
}
