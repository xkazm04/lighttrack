//! Derive the published gate table from the workflow instead of trusting a human to re-copy it.
//!
//! `CONTRIBUTING.md` carries a "Job (check name) | Blocking" table, and `.github/workflows/ci.yml`
//! carries the jobs it describes. Until this guard existed the two were kept in sync by hand — and
//! the audit that produced it found them *out* of sync: the doc published clippy/fmt as "advisory —
//! pre-existing debt" while the workflow had both blocking, and omitted two jobs entirely. A
//! projection nobody checks is a claim, not a projection.
//!
//! The workflow is the authority. This test reads both files at compile time (`include_str!`, so a
//! missing file is a build error rather than a skipped check) and asserts, in both directions:
//!
//! * every CI job appears in the table, and every table row names a real CI job — no additions, no
//!   removals, no typos in a check name that branch protection also spells;
//! * the table's Blocking column matches the workflow's `continue-on-error:` for that job.
//!
//! It deliberately parses the YAML by hand rather than pulling in a YAML dependency: the shape it
//! needs is two-space-indented job ids with a `name:` and an optional `continue-on-error:`, the
//! parse is ten lines, and `lighttrack-core` stays dependency-light (see deny.toml's bans policy).

use std::collections::BTreeMap;

const CI_YML: &str = include_str!("../../../.github/workflows/ci.yml");
const CONTRIBUTING: &str = include_str!("../../../CONTRIBUTING.md");

/// The table's header row, verbatim. Also the anchor: if someone re-titles the columns, the parse
/// finds nothing and the "no rows" assertion fires instead of silently checking an empty set.
const TABLE_HEADER: &str = "| Job (check name) | Blocking |";

/// `check name -> blocking?` as the workflow defines it.
///
/// Reads the `jobs:` block: a job id is a key indented exactly two spaces, its `name:` and
/// `continue-on-error:` are indented four. A job with `continue-on-error: true` does not block.
fn jobs_from_workflow(yaml: &str) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    let mut in_jobs = false;
    let mut current: Option<String> = None;
    let mut name: Option<String> = None;
    let mut advisory = false;

    let mut flush =
        |current: &mut Option<String>, name: &mut Option<String>, advisory: &mut bool| {
            if let Some(id) = current.take() {
                let n = name.take().unwrap_or(id);
                out.insert(n, !*advisory);
            }
            *advisory = false;
        };

    for line in yaml.lines() {
        if line.starts_with("jobs:") {
            in_jobs = true;
            continue;
        }
        if !in_jobs {
            continue;
        }
        // A top-level key ends the jobs block.
        if !line.starts_with(' ') && !line.trim().is_empty() {
            break;
        }
        let indent = line.len() - line.trim_start().len();
        let body = line.trim_start();
        if body.starts_with('#') || body.is_empty() {
            continue;
        }
        if indent == 2 && body.ends_with(':') {
            flush(&mut current, &mut name, &mut advisory);
            current = Some(body.trim_end_matches(':').to_string());
        } else if indent == 4 {
            if let Some(v) = body.strip_prefix("name:") {
                name = Some(v.trim().trim_matches('"').trim_matches('\'').to_string());
            } else if let Some(v) = body.strip_prefix("continue-on-error:") {
                advisory = v.trim() == "true";
            }
        }
    }
    flush(&mut current, &mut name, &mut advisory);
    out
}

/// `check name -> blocking?` as CONTRIBUTING.md publishes it.
///
/// A row's first cell is the check name in backticks, optionally followed by prose ("— the detached
/// `clients/rust` project"), so only the FIRST backticked span is the name. The Blocking cell is
/// `yes` or a bolded `**no**` with a pointer to the explanation below the table.
fn rows_from_doc(md: &str) -> BTreeMap<String, bool> {
    let mut out = BTreeMap::new();
    let mut in_table = false;
    for line in md.lines() {
        if line.trim() == TABLE_HEADER {
            in_table = true;
            continue;
        }
        if !in_table {
            continue;
        }
        if !line.trim_start().starts_with('|') {
            break; // the table ended
        }
        let cells: Vec<&str> = line.trim().trim_matches('|').split('|').collect();
        if cells.len() < 2 {
            continue;
        }
        let first = cells[0].trim();
        if first.starts_with("---") {
            continue; // the separator row
        }
        let Some(name) = first.split('`').nth(1) else {
            panic!("gate-table row has no backticked check name: {line}");
        };
        let verdict = cells[1].to_lowercase();
        let blocking = match (verdict.contains("yes"), verdict.contains("no")) {
            (true, false) => true,
            (false, true) => false,
            _ => panic!("gate-table Blocking cell is neither yes nor no: {line}"),
        };
        out.insert(name.to_string(), blocking);
    }
    out
}

#[test]
fn the_published_gate_table_matches_the_workflow() {
    let ci = jobs_from_workflow(CI_YML);
    let doc = rows_from_doc(CONTRIBUTING);

    assert!(
        ci.len() >= 5,
        "parsed only {} jobs from ci.yml — the parser lost the jobs block, not the workflow: {ci:?}",
        ci.len()
    );
    assert!(
        !doc.is_empty(),
        "found no rows under `{TABLE_HEADER}` in CONTRIBUTING.md — was the table renamed or removed?"
    );

    let missing: Vec<&String> = ci.keys().filter(|k| !doc.contains_key(*k)).collect();
    assert!(
        missing.is_empty(),
        "ci.yml defines job(s) the CONTRIBUTING gate table does not publish: {missing:?}\n\
         ci.yml is the authority — add a row for each in the same PR that added the job."
    );

    let extra: Vec<&String> = doc.keys().filter(|k| !ci.contains_key(*k)).collect();
    assert!(
        extra.is_empty(),
        "the CONTRIBUTING gate table names check(s) ci.yml does not define: {extra:?}\n\
         A row for a job that no longer exists is worse than no row: branch protection is \
         configured from these exact names."
    );

    let disagree: Vec<String> = ci
        .iter()
        .filter(|(k, blocking)| doc.get(*k) != Some(blocking))
        .map(|(k, blocking)| {
            format!(
                "{k}: ci.yml says {}, the table says {}",
                if *blocking { "blocking" } else { "advisory" },
                if doc[k] { "blocking" } else { "advisory" }
            )
        })
        .collect();
    assert!(
        disagree.is_empty(),
        "the gate table's Blocking column contradicts ci.yml's `continue-on-error:`:\n  {}",
        disagree.join("\n  ")
    );
}

/// The guard's own seeded-failure proof: both parsers must actually *find* something and actually
/// *disagree* when the surfaces drift, so a future refactor that quietly makes either return an
/// empty map turns this test into a tautology instead of a gate.
#[test]
fn the_guard_can_go_red() {
    // A workflow whose advisory job the doc publishes as blocking.
    let ci = "jobs:\n  a:\n    name: gate a\n  b:\n    name: gate b\n    continue-on-error: true\n";
    let parsed = jobs_from_workflow(ci);
    assert_eq!(parsed.get("gate a"), Some(&true));
    assert_eq!(
        parsed.get("gate b"),
        Some(&false),
        "continue-on-error ⇒ advisory"
    );

    let md = format!("{TABLE_HEADER}\n| --- | --- |\n| `gate a` — with prose | yes |\n| `gate b` | yes |\n\ntext after\n");
    let rows = rows_from_doc(&md);
    assert_eq!(
        rows.get("gate a"),
        Some(&true),
        "prose after the name is not part of it"
    );
    assert_eq!(rows.get("gate b"), Some(&true));
    assert_ne!(
        parsed.get("gate b"),
        rows.get("gate b"),
        "this is the drift the real test must catch"
    );

    // A row for a job that does not exist is caught by name, not just by verdict.
    let rows = rows_from_doc(&format!(
        "{TABLE_HEADER}\n| --- | --- |\n| `gate c` | yes |\n\n"
    ));
    assert!(!parsed.contains_key("gate c"));
    assert!(rows.contains_key("gate c"));
}
