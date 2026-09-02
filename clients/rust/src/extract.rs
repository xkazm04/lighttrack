//! Pull model + token usage out of a native provider response.
//!
//! Pure functions over `serde_json::Value`, split out of the `track_*_json` methods so the shared
//! contract fixtures (`clients/contract/fixtures/extractors.json`) can assert them without a client
//! or a network. Extraction was the most-triplicated code in the three SDKs and the place drift was
//! least visible: it fails by recording `model = "unknown"` and zero tokens, which looks like a
//! quiet call rather than a broken reader.

use serde_json::Value;

/// What every extractor returns. `cached` is `None` for *unknown*, which is not the same as `0`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extracted {
    pub model: Option<String>,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cached_input_tokens: Option<u64>,
}

fn s(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// OpenAI chat/completions and responses.
pub fn extract_openai(resp: &Value) -> Extracted {
    let u = &resp["usage"];
    Extracted {
        model: s(&resp["model"]),
        input_tokens: u["prompt_tokens"]
            .as_u64()
            .or_else(|| u["input_tokens"].as_u64())
            .unwrap_or(0),
        output_tokens: u["completion_tokens"]
            .as_u64()
            .or_else(|| u["output_tokens"].as_u64())
            .unwrap_or(0),
        // The Responses API renamed the pair AND moved the cache counter: `input_tokens_details`,
        // not `prompt_tokens_details`. Reading only the older place reported every cached Responses
        // call as uncached, which the price book then charged at full input rate.
        cached_input_tokens: u["prompt_tokens_details"]["cached_tokens"]
            .as_u64()
            .or_else(|| u["input_tokens_details"]["cached_tokens"].as_u64()),
    }
}

/// Anthropic messages.
pub fn extract_anthropic(resp: &Value) -> Extracted {
    let u = &resp["usage"];
    Extracted {
        model: s(&resp["model"]),
        input_tokens: u["input_tokens"].as_u64().unwrap_or(0),
        output_tokens: u["output_tokens"].as_u64().unwrap_or(0),
        // `cache_read_input_tokens` only. `cache_creation_input_tokens` is a different, billed thing.
        cached_input_tokens: u["cache_read_input_tokens"].as_u64(),
    }
}

/// Gemini `generateContent`.
///
/// Both spellings, deliberately. The REST/JS shape is camelCase; the google-genai Python objects and
/// their `to_json_dict()` are snake_case for exactly the same fields. This reader took only the
/// camelCase form, so every Rust user feeding it a google-genai dict recorded `unknown` and zeroes —
/// a silent hole in the usage ledger, not an error anyone would see.
pub fn extract_gemini(resp: &Value) -> Extracted {
    let u = if resp["usageMetadata"].is_object() {
        &resp["usageMetadata"]
    } else {
        &resp["usage_metadata"]
    };
    let dual = |camel: &str, snake: &str| u[camel].as_u64().or_else(|| u[snake].as_u64());
    Extracted {
        model: s(&resp["modelVersion"]).or_else(|| s(&resp["model_version"])),
        input_tokens: dual("promptTokenCount", "prompt_token_count").unwrap_or(0),
        output_tokens: dual("candidatesTokenCount", "candidates_token_count").unwrap_or(0),
        cached_input_tokens: dual("cachedContentTokenCount", "cached_content_token_count"),
    }
}
