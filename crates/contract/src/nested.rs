//! Full JSON schemas for the few write-tool arguments whose value is a nested object, or an array
//! of them.
//!
//! Everywhere else a parameter's type, enum and prose are enough. Here they are not: an agent that
//! is told `targets` is "an array" writes a benchmark whose runs can never satisfy the promotion
//! gate, because it cannot see that a target may carry a `prompt_ref` and that the literal field is
//! `system_prompt`, not `prompt`. These mirror exactly what the API deserialises — `RubricDimension`
//! (`crates/core/src/rubric.rs`) and `BenchmarkCase` / `BenchTarget` (`crates/core/src/score.rs`).
//!
//! Raw JSON text rather than a builder so the schema reads as the document an agent receives; the
//! contract's own test parses every one of them, so a typo is a failing build, not a silently
//! vaguer tool.

/// `create_rubric.dimensions` — weighted, anchored scoring dimensions. `key` + `description` are
/// required (neither has a serde default). `weight`, `anchors`, `floor`, `kind` (default `llm`)
/// and `check` are optional. `kind` is the closed set of how a dimension is scored; `check`
/// configures anything other than the judge and is omitted entirely for the default.
pub(crate) const RUBRIC_DIMENSIONS: &str = r#"{
    "type": "array",
    "description": "weighted scoring dimensions the judge fills per case",
    "items": {
        "type": "object",
        "required": ["key", "description"],
        "properties": {
            "key": {"type":"string","description":"stable identifier used in the judge's JSON output (e.g. correctness)"},
            "description": {"type":"string","description":"what this dimension measures"},
            "weight": {"type":"number","description":"relative weight in the overall score (default 1.0)"},
            "anchors": {"type":"array","items":{"type":"string"},"description":"anchored level descriptions, e.g. [\"1.0 = fully correct\", \"0.5 = minor error\", \"0 = wrong\"]"},
            "floor": {"type":"number","description":"gating floor: if this dimension scores below it, the case fails regardless of the overall"},
            "kind": {"type":"string","enum":["llm","exact","regex","numeric","json_valid","contains","exec","grounding"],"description":"how this dimension is scored. Omit for llm, the judge model, which is the default. exact, regex, numeric, json_valid and contains are local mechanical checks; exec runs check.cmd in a sandbox and scores the exit code; grounding is model-judged claim by claim against the case evidence. A spelling off this list is a 400 (optional)"},
            "check": {
                "type": "object",
                "description": "configuration for a non-default kind. Every field is optional, so an llm dimension omits the object. Ignored when kind is llm or omitted.",
                "properties": {
                    "expect": {"type":"string","description":"literal target for exact, contains, numeric, and optionally json_valid. Defaults to the case expected answer"},
                    "pattern": {"type":"string","description":"regex only: unanchored pattern the output must match somewhere"},
                    "tolerance": {"type":"number","description":"numeric only: absolute tolerance around the target. Omit for exact equality"},
                    "path": {"type":"string","description":"JSON Pointer (RFC 6901) selecting the part of a JSON output to check. Omit to check the whole output"},
                    "case_sensitive": {"type":"boolean","description":"compare case-sensitively. Default false"},
                    "trim": {"type":"boolean","description":"trim surrounding whitespace before comparing. Default true"},
                    "image": {"type":"string","description":"exec only: sandbox image, a UUID (exact pin) or tag:NAME (best-effort)"},
                    "cmd": {"type":"string","description":"exec only: command run in the sandbox. Its exit code is the verdict; 0 scores 1.0"},
                    "write": {"type":"string","description":"exec only: absolute path inside the sandbox where the candidate output is written before cmd runs"},
                    "timeout_secs": {"type":"integer","description":"exec only: wall-clock ceiling in seconds. A timeout is unavailable, not a fail"},
                    "env": {"type":"object","additionalProperties":{"type":"string"},"description":"exec only: literal fixture environment variables. Never host secrets; credential-shaped names are refused"}
                }
            }
        }
    }
}"#;

/// `create_benchmark.dataset` — inline cases. Only `input` is required; `expected` (reference) and
/// `output` (candidate to judge) are optional.
pub(crate) const BENCHMARK_DATASET: &str = r#"{
    "type": "array",
    "description": "inline benchmark cases",
    "items": {
        "type": "object",
        "required": ["input"],
        "properties": {
            "input": {"type":"string","description":"the case prompt / input"},
            "expected": {"type":"string","description":"golden reference answer the judge can compare against (optional)"},
            "output": {"type":"string","description":"a pre-captured candidate response to judge; omit to generate from targets (optional)"},
            "difficulty": {"type":"string","enum":["easy","medium","hard"],"description":"how hard this case is meant to be, on a closed ordered ladder. Omit for ungraded - ungraded is a state of its own, never 'medium'. A spelling off this ladder is a 400, not a silent downgrade, so grade deliberately or leave it out. Grading is what lets a run report which tier actually separated the targets: a tier every target passes measured nothing and is money spent to learn nothing (optional)"}
        }
    }
}"#;

/// `create_benchmark.targets` — the provider/model comparison matrix. `provider` + `model` are
/// required; `system_prompt` (the variant under test) and `label` optional. Note the field is
/// `system_prompt`, not `prompt`.
pub(crate) const BENCHMARK_TARGETS: &str = r#"{
    "type": "array",
    "description": "comparison matrix: one row per target under test. A target is a (provider, model) with either a literal system_prompt or a prompt_ref that resolves from the registry at run time, or an HTTP endpoint of your own (kind.type=http) that receives {input, expected?, system_prompt?} and returns {output, usage?, latency_ms?, cost_usd?}.",
    "items": {
        "type": "object",
        "required": ["provider", "model"],
        "properties": {
            "provider": {"type":"string","description":"e.g. anthropic, openai; for an http target, a label for whatever answers there"},
            "model": {"type":"string"},
            "system_prompt": {"type":"string","description":"literal system/instruction prompt variant under test"},
            "label": {"type":"string","description":"display label; defaults to provider/model, or provider/model@effort when an effort is set"},
            "effort": {"type":"string","enum":["low","medium","high","xhigh","max"],"description":"reasoning effort for this target. THIS IS AN AXIS: the same model at two efforts is two commensurable rows, which is how you ask whether the expensive setting buys anything on your cases. Omit for the provider's default - an absent effort is a different fact from any named level. Not every adapter can honour every level; one that cannot refuses loudly rather than silently sending the default (optional)"},
            "prompt_ref": {
                "type": "object",
                "description": "resolve this target's prompt from the registry at run start instead of using the literal system_prompt. Required for the promotion gate to certify the version it ran: a run only reports resolved_prompt_version when it fetched the content. Pass at most one of version/label.",
                "required": ["name"],
                "properties": {
                    "name": {"type":"string","description":"registry name, must already exist in this project"},
                    "version": {"type":"integer","description":"pin an exact version"},
                    "label": {"type":"string","description":"resolve through a label, e.g. production"}
                }
            },
            "limits": {
                "type": "object",
                "description": "per-case service ceilings this target must hold to BESIDES scoring well. A case whose generation exceeds one FAILS, however well the judge scored it - which is the part a rubric cannot express: the same answer at 8s and 4c a case is not the same product as one at 1s and a tenth of a cent. A ceiling that could not be checked (an unpriced model has no cost) is reported as unchecked, never as a pass. Compare mode only (optional).",
                "properties": {
                    "max_cost_usd": {"type":"number","description":"most one case's generation may cost, USD per candidate - so a --gen-samples 3 run is held to the same per-call bar, not three times it"},
                    "max_latency_ms": {"type":"integer","description":"longest one case's generation may take, milliseconds per candidate"}
                }
            },
            "kind": {
                "type": "object",
                "description": "how this target produces output. Omit for a model call.",
                "required": ["type"],
                "properties": {
                    "type": {"type":"string","enum":["model","http"]},
                    "url": {"type":"string","description":"https endpoint (http target only). Private, loopback and link-local addresses are refused."}
                }
            }
        }
    }
}"#;

#[cfg(test)]
#[path = "nested_tests.rs"]
mod tests;
