//! `lt prompts` — the registry's read side, and the one question promotion used to leave
//! unanswered: is the version we promoted actually any good?

use anyhow::Result;
use reqwest::Method;

use crate::cli::{Cli, PromptsCmd};
use crate::http::call;

/// `/v1/quality/prompts` with the optional narrowings. Every parameter is optional: "how are my
/// served versions scoring" has a useful answer before an operator knows which window or rubric to
/// ask about.
pub(crate) fn quality_path(
    project: &Option<String>,
    since: &Option<String>,
    until: &Option<String>,
    rubric_id: &Option<String>,
) -> String {
    let mut p = "/v1/quality/prompts".to_string();
    let mut sep = '?';
    for (k, v) in [
        ("project", project),
        ("since", since),
        ("until", until),
        ("rubric_id", rubric_id),
    ] {
        if let Some(val) = v.as_deref().filter(|s| !s.is_empty()) {
            p.push_str(&format!("{sep}{k}={val}"));
            sep = '&';
        }
    }
    p
}

pub(crate) fn run(cli: &Cli, action: &PromptsCmd) -> Result<()> {
    match action {
        PromptsCmd::List { project } => call(
            cli,
            Method::GET,
            &format!("/v1/projects/{project}/prompts"),
            None,
            "list_prompts",
        ),
        PromptsCmd::Quality {
            project,
            since,
            until,
            rubric_id,
        } => call(
            cli,
            Method::GET,
            &quality_path(project, since, until, rubric_id),
            None,
            "get_prompt_quality",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn the_quality_path_narrows_only_when_asked() {
        assert_eq!(
            quality_path(&None, &None, &None, &None),
            "/v1/quality/prompts"
        );
        assert_eq!(
            quality_path(&s("p1"), &None, &None, &None),
            "/v1/quality/prompts?project=p1"
        );
        // A later parameter alone must open the query string, not join an absent one with `&`.
        assert_eq!(
            quality_path(&None, &None, &None, &s("rub-1")),
            "/v1/quality/prompts?rubric_id=rub-1"
        );
        assert_eq!(
            quality_path(&s("p1"), &s("2026-01-01T00:00:00Z"), &s("2026-02-01T00:00:00Z"), &s("r")),
            "/v1/quality/prompts?project=p1&since=2026-01-01T00:00:00Z&until=2026-02-01T00:00:00Z&rubric_id=r"
        );
        // An empty string is the shape a shell hands over for an unset variable; it must not
        // become `?project=`, which the API would read as a project named "".
        assert_eq!(
            quality_path(&s(""), &None, &None, &None),
            "/v1/quality/prompts"
        );
    }
}
