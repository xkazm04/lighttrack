//! The CLI's half of the contract: every verb the endpoint table declares must exist in the clap
//! tree, and every `paged` endpoint's verb must accept `--cursor`.
//!
//! The verb tree stays hand-written — a runtime-built `Command` would give up clap's derive-time
//! checking and the typed dispatch in `main.rs` for no gain a test cannot provide. What the table
//! gives instead is the property that was missing: nobody can add a route, declare an operator
//! reaches it from the CLI, and then not write the verb. Before this, `limits usage`, `margin
//! trend`, `rollup`, `forecast`, `storage status` and a dozen others were declared reachable by
//! nothing at all, and the only way to notice was to go looking.

#[cfg(test)]
mod tests {
    use clap::CommandFactory;

    use crate::cli::Cli;

    /// Walk the clap tree to the subcommand at `path`, if it exists.
    fn find(path: &[&str]) -> Option<clap::Command> {
        let mut cmd = Cli::command();
        for (i, seg) in path.iter().enumerate() {
            let next = cmd
                .get_subcommands()
                .find(|c| c.get_name() == *seg)?
                .clone();
            cmd = next;
            if i + 1 == path.len() {
                return Some(cmd);
            }
        }
        None
    }

    /// The coverage property. An endpoint that says `lt margin trend` reaches it, and no such verb,
    /// is a promise the contract makes and the binary breaks.
    #[test]
    fn every_declared_cli_verb_exists() {
        let missing: Vec<String> = lighttrack_contract::endpoints()
            .filter_map(|e| e.cli.map(|c| (e.id, c)))
            .filter(|(_, path)| find(path).is_none())
            .map(|(id, path)| format!("{id} -> lt {}", path.join(" ")))
            .collect();
        assert!(
            missing.is_empty(),
            "the contract says these endpoints are reachable from the CLI, and the verb does not \
             exist: {missing:#?}"
        );
    }

    /// The other direction is deliberately NOT asserted: the CLI has verbs that are operator
    /// conveniences over several endpoints at once, and forcing a 1:1 mapping would push those out
    /// of the tool. What must hold is that no *declared* verb is missing, which is the test above.
    /// This one keeps the tree itself honest — a subcommand with no help text is unusable.
    #[test]
    fn every_verb_has_help_text() {
        fn walk(cmd: &clap::Command, path: &mut Vec<String>, out: &mut Vec<String>) {
            for sub in cmd.get_subcommands() {
                path.push(sub.get_name().to_string());
                if sub.get_about().is_none() {
                    out.push(path.join(" "));
                }
                walk(sub, path, out);
                path.pop();
            }
        }
        let mut out = Vec::new();
        walk(&Cli::command(), &mut Vec::new(), &mut out);
        assert!(out.is_empty(), "these verbs have no help text: {out:?}");
    }

    /// The module doc at the top of `main.rs` is the operator's quick reference, and it is a
    /// hand-maintained list of invocations: every `lt …` example on it must still parse, or the
    /// documentation teaches a verb or flag the binary no longer has. Read from the source so the
    /// doc and the check cannot drift apart.
    #[test]
    fn every_documented_example_invocation_parses() {
        let src = include_str!("main.rs");
        let examples: Vec<&str> = src
            .lines()
            .take_while(|l| l.starts_with("//!"))
            .flat_map(|l| l.trim_start_matches("//!").split('|'))
            .map(str::trim)
            .filter(|s| s.starts_with("lt "))
            .collect();
        assert!(
            examples.len() > 20,
            "the example block was not found: {examples:?}"
        );
        let mut failed = Vec::new();
        for ex in examples {
            // A trailing parenthetical is commentary, not arguments; a single-quoted argument is
            // one shell word.
            let line = ex.split("  (").next().unwrap_or(ex);
            let argv = shell_words(line);
            if let Err(e) = <Cli as clap::Parser>::try_parse_from(argv) {
                failed.push(format!("{ex}: {}", e.kind()));
            }
        }
        assert!(
            failed.is_empty(),
            "documented examples that no longer parse: {failed:#?}"
        );
    }

    /// Minimal shell splitting for the examples: whitespace-separated, single quotes group.
    fn shell_words(line: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut cur = String::new();
        let mut quoted = false;
        for ch in line.chars() {
            match ch {
                '\'' => quoted = !quoted,
                c if c.is_whitespace() && !quoted => {
                    if !cur.is_empty() {
                        out.push(std::mem::take(&mut cur));
                    }
                }
                c => cur.push(c),
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// A paged endpoint whose verb cannot take a cursor can show you the first page and nothing
    /// else — which reads as "that is all the data", the worst failure an observability tool has.
    #[test]
    fn every_paged_endpoints_verb_accepts_a_cursor() {
        let missing: Vec<String> = lighttrack_contract::endpoints()
            .filter(|e| e.paged)
            .filter_map(|e| e.cli.map(|c| (e.id, c)))
            .filter(|(_, path)| {
                find(path).is_none_or(|cmd| !cmd.get_arguments().any(|a| a.get_id() == "cursor"))
            })
            .map(|(id, path)| format!("{id} -> lt {}", path.join(" ")))
            .collect();
        assert!(
            missing.is_empty(),
            "these verbs reach a paged endpoint but take no --cursor, so they can only ever show \
             the first page: {missing:#?}"
        );
    }
}
