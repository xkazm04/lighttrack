//! Resolving a benchmark's targets before the first paid call — and generating from them.
//!
//! Until this module existed, `grep prompts crates/runner/src` found only tags: a version-triggered
//! run carried `{prompt_id, prompt_version}` in its report and generated from the target's *stored*
//! `system_prompt`, so the promotion gate passed or blocked on a run that merely claimed the
//! version. This is the missing fetch. It runs once, at run start, before any money is spent, so a
//! bad ref fails the run instead of half of it.
//!
//! It is also where a target stops having to be a model: an `Http` target is generated from here
//! through the engine's endpoint adapter, and everything downstream (pricing, family, labels) reads
//! the same [`ResolvedTarget`].

use anyhow::{Context, Result};
use serde::Deserialize;

use lighttrack_core::{BenchTarget, INPUT_PLACEHOLDER};
use lighttrack_engine::{
    generate, generate_deterministic, generate_http, EngineConfig, GenOutcome,
};

use crate::cli::Cli;
use crate::http::get;

/// The subset of `GET /v1/projects/:pid/prompts/:name` this needs. The route returns more (tag,
/// config, note); a narrow struct means a field added there cannot break a run.
#[derive(Debug, Deserialize)]
struct FetchedPrompt {
    version: u32,
    content: String,
}

/// A target with its registry content already fetched.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedTarget {
    pub(crate) target: BenchTarget,
    /// The registry content this target will generate with. `None` = no `prompt_ref`, so the
    /// target's own literal `system_prompt` stands (the pre-M10 behaviour, unchanged).
    pub(crate) content: Option<String>,
    /// The version [`content`](Self::content) came from — stamped into the run report as
    /// `resolved_prompt_version`, the evidence the promotion gate requires.
    pub(crate) resolved_version: Option<u32>,
}

impl ResolvedTarget {
    /// A target that resolves nothing: exactly what every run did before M10.
    pub(crate) fn literal(target: BenchTarget) -> Self {
        Self {
            target,
            content: None,
            resolved_version: None,
        }
    }

    /// How one case is presented to this target: `(system_prompt, user_input)`.
    ///
    /// A registry prompt containing `{{input}}` is a *template* — the case goes where the author put
    /// it, and there is no separate system turn. Without the placeholder the prompt is an
    /// instruction and the case stays the user turn, which is the shape `system_prompt` always had.
    /// Deliberately the whole templating language: anything richer is a rendering engine whose bugs
    /// would show up as quality regressions, and the registry's `config` is where a real one belongs.
    pub(crate) fn render(&self, input: &str) -> (Option<String>, String) {
        match &self.content {
            None => (self.target.system_prompt.clone(), input.to_string()),
            Some(c) if c.contains(INPUT_PLACEHOLDER) => (None, c.replace(INPUT_PLACEHOLDER, input)),
            Some(c) => (Some(c.clone()), input.to_string()),
        }
    }

    /// Generate one candidate for `input`. `pin` asks for deterministic sampling (one candidate per
    /// case); an `Http` target ignores it, having no knobs, and says so via its `Determinism`.
    pub(crate) fn generate(
        &self,
        engine: &EngineConfig,
        input: &str,
        expected: Option<&str>,
        pin: bool,
    ) -> lighttrack_engine::Result<GenOutcome> {
        let (system_prompt, user_input) = self.render(input);
        match self.target.http_url() {
            Some(url) => generate_http(url, system_prompt.as_deref(), &user_input, expected),
            None => {
                let call = if pin {
                    generate_deterministic
                } else {
                    generate
                };
                call(
                    engine,
                    &self.target.provider,
                    &self.target.model,
                    system_prompt.as_deref(),
                    &user_input,
                    None,
                )
            }
        }
    }
}

/// Fetch every target's `prompt_ref` from the registry.
///
/// `override_version` is `(prompt_name, version)` from a version-triggered job payload: the version
/// the run was enqueued to score wins over whatever the stored ref pins, but only for targets whose
/// ref names *that* prompt. Without the name half we could not tell which target of a matrix the
/// version belonged to — which is why the payload carries `prompt_name`.
///
/// A ref that cannot be fetched is a hard error. Falling back to the stored `system_prompt` would
/// reproduce exactly the failure this milestone exists to remove: a run that reads as having tested
/// a version while testing something else.
pub(crate) fn resolve_targets(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    project_id: &str,
    targets: &[BenchTarget],
    override_version: Option<(&str, u32)>,
) -> Result<Vec<ResolvedTarget>> {
    targets
        .iter()
        .map(|t| {
            let Some(r) = &t.prompt_ref else {
                return Ok(ResolvedTarget::literal(t.clone()));
            };
            let query = match override_version {
                Some((name, v)) if name == r.name => format!("?version={v}"),
                _ => r.query(),
            };
            let path = format!("/v1/projects/{project_id}/prompts/{}{query}", r.name);
            let fetched: FetchedPrompt = get(cli, http, &path).with_context(|| {
                format!(
                    "resolving prompt '{}' for target {} — a benchmark that cannot fetch its \
                     target's prompt must not run, or its score would describe other content",
                    r.name,
                    t.display_label()
                )
            })?;
            println!(
                "  resolved {} -> {}@v{}",
                t.display_label(),
                r.name,
                fetched.version
            );
            Ok(ResolvedTarget {
                target: t.clone(),
                content: Some(fetched.content),
                resolved_version: Some(fetched.version),
            })
        })
        .collect()
}

/// The single version to record as the run's `resolved_prompt_version`.
///
/// A matrix may resolve several prompts, and the gate asks about one. When the job named a prompt
/// (`prompt_name`), that prompt's target decides. Otherwise the run reports a version only when the
/// matrix agrees on exactly one — a disagreement is reported as nothing rather than as an arbitrary
/// pick, because the gate treats a recorded number as proof.
pub(crate) fn run_resolved_version(
    resolved: &[ResolvedTarget],
    prompt_name: Option<&str>,
) -> Option<u32> {
    if let Some(name) = prompt_name {
        let named = resolved
            .iter()
            .find(|r| r.target.prompt_ref.as_ref().is_some_and(|p| p.name == name));
        if let Some(r) = named {
            return r.resolved_version;
        }
    }
    let mut versions: Vec<u32> = resolved.iter().filter_map(|r| r.resolved_version).collect();
    versions.sort_unstable();
    versions.dedup();
    match versions.as_slice() {
        [only] => Some(*only),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use lighttrack_core::PromptRef;

    fn target(name: Option<&str>) -> BenchTarget {
        let mut t: BenchTarget = serde_json::from_value(serde_json::json!({
            "provider": "openai", "model": "gpt-4o", "system_prompt": "stored literal"
        }))
        .unwrap();
        t.prompt_ref = name.map(|n| PromptRef {
            name: n.into(),
            version: None,
            label: None,
        });
        t
    }

    fn resolved(name: Option<&str>, content: Option<&str>, version: Option<u32>) -> ResolvedTarget {
        ResolvedTarget {
            target: target(name),
            content: content.map(str::to_string),
            resolved_version: version,
        }
    }

    #[test]
    fn an_unresolved_target_still_uses_its_literal_system_prompt() {
        let (sys, input) = ResolvedTarget::literal(target(None)).render("hello");
        assert_eq!(sys.as_deref(), Some("stored literal"));
        assert_eq!(input, "hello", "the case is the user turn, as before");
    }

    #[test]
    fn a_resolved_prompt_replaces_the_literal_and_a_template_swallows_the_case() {
        // An instruction-shaped prompt becomes the system turn and the literal is ignored — the
        // whole point: a run must not generate from stale stored content.
        let (sys, input) = resolved(Some("p"), Some("you are terse"), Some(2)).render("hello");
        assert_eq!(sys.as_deref(), Some("you are terse"));
        assert_eq!(input, "hello");

        // A template puts the case where its author put it, and there is no system turn to leak the
        // template itself into.
        let (sys, input) =
            resolved(Some("p"), Some("Q: {{input}}\nA:"), Some(2)).render("refund window?");
        assert_eq!(sys, None);
        assert_eq!(input, "Q: refund window?\nA:");
    }

    #[test]
    fn the_run_reports_the_version_of_the_prompt_the_job_named() {
        let matrix = vec![
            resolved(Some("support-reply"), Some("a"), Some(7)),
            resolved(Some("triage"), Some("b"), Some(3)),
        ];
        assert_eq!(
            run_resolved_version(&matrix, Some("support-reply")),
            Some(7),
            "the named prompt's target decides, not the first or the newest"
        );
        assert_eq!(run_resolved_version(&matrix, Some("triage")), Some(3));
        // A matrix resolving two different versions and no name to disambiguate reports NOTHING:
        // the gate reads a recorded number as proof, so an arbitrary pick would be a false proof.
        assert_eq!(run_resolved_version(&matrix, None), None);
        assert_eq!(
            run_resolved_version(&matrix, Some("nobody")),
            None,
            "a name no target carries falls through to the agreement rule, not to target 0"
        );
        // One prompt across the matrix needs no name.
        let agreed = vec![
            resolved(Some("support-reply"), Some("a"), Some(7)),
            resolved(Some("support-reply"), Some("a"), Some(7)),
        ];
        assert_eq!(run_resolved_version(&agreed, None), Some(7));
        // Nothing resolved at all → nothing reported, which is what makes the gate refuse.
        assert_eq!(
            run_resolved_version(&[ResolvedTarget::literal(target(None))], Some("p")),
            None
        );
    }
}
