//! `docs/API.md`, generated from the table.
//!
//! The counts in `README.md` and `docs/ROADMAP.md` were hand-kept and had drifted badly — both said
//! "28 read tools + 15 write tools" against an actual 43 and 21. A number a human maintains about a
//! list a machine generates is a number that will be wrong; so the matrix is generated, the test
//! below fails when the checked-in file is stale, and the prose documents point at it instead of
//! restating it.
//!
//! Regenerate with `LIGHTTRACK_WRITE_API_MATRIX=1 cargo test -p lighttrack-contract matrix`.

use crate::types::{Access, Endpoint, TypeRef};

/// The whole document.
pub fn markdown() -> String {
    let all: Vec<&Endpoint> = crate::endpoints().collect();
    let mut s = String::new();
    s.push_str(
        "# API surface\n\n\
         <!-- GENERATED FILE — do not edit. Source: `crates/contract/src/endpoints/`.\n\
         \x20    Regenerate: `LIGHTTRACK_WRITE_API_MATRIX=1 cargo test -p lighttrack-contract matrix` -->\n\n\
         Every HTTP endpoint this deployment serves, who may call it, and which of the three client\n\
         surfaces reaches it. The axum router, the MCP tool catalog, the `lt` verb tree and the\n\
         Markdown renderer are all generated from or held to the same table, so this document cannot\n\
         describe a route that does not exist, or miss one that does.\n\n",
    );

    s.push_str(&summary(&all));
    s.push_str("\n## Endpoints\n\n");
    s.push_str(
        "`Scope` is the capability a **project key** needs; `admin` means no project key reaches it\n\
         whatever its scopes, and `—` means the door authenticates something that is not a LightTrack\n\
         principal at all. A blank MCP or CLI cell is not an oversight where the row is marked 🔒:\n\
         those are machine doors (an SDK's ingest, a device agent's lease, a provider's webhook).\n\n",
    );
    s.push_str("| Method | Path | Scope | MCP tool | CLI | Renderer |\n");
    s.push_str("|---|---|---|---|---|---|\n");
    for e in &all {
        s.push_str(&row(e));
    }
    s.push('\n');
    s.push_str(&untyped_note(&all));
    s
}

fn summary(all: &[&Endpoint]) -> String {
    let paths = crate::route_paths().len();
    let mcp = all.iter().filter(|e| e.mcp.is_some()).count();
    let writes = all.iter().filter(|e| e.mcp.is_some() && e.mutating).count();
    let cli = all.iter().filter(|e| e.cli.is_some()).count();
    let rendered = all.iter().filter(|e| e.render_kind.is_some()).count();
    let machine = all.iter().filter(|e| e.machine).count();
    let paged = all.iter().filter(|e| e.paged).count();
    let uncovered = all
        .iter()
        .filter(|e| e.mcp.is_none() && e.cli.is_none() && !e.machine)
        .count();
    format!(
        "| | |\n|---|---|\n\
         | Endpoints (method × path) | {} |\n\
         | Distinct `/v1` routes | {paths} |\n\
         | MCP tools | {mcp} ({} read, {writes} write) |\n\
         | CLI verbs | {cli} |\n\
         | Endpoints with a Markdown renderer | {rendered} |\n\
         | Machine doors (SDK / device / provider) | {machine} |\n\
         | Paged reads | {paged} |\n\
         | Reachable from neither MCP nor CLI | {uncovered} |\n",
        all.len(),
        mcp - writes,
    )
}

fn row(e: &Endpoint) -> String {
    let scope = match e.access {
        Access::Admin => "admin".to_string(),
        Access::Key(k) => format!("`{}`", k.as_str()),
        Access::Unauthenticated => "—".to_string(),
    };
    let mcp = match &e.mcp {
        Some(t) => format!("`{}`", t.name),
        None if e.machine => "🔒".to_string(),
        None => String::new(),
    };
    let cli = match e.cli {
        Some(p) => format!("`lt {}`", p.join(" ")),
        None => String::new(),
    };
    let render = e.render_kind.map(|k| format!("`{k}`")).unwrap_or_default();
    format!(
        "| {} | `{}` | {scope} | {mcp} | {cli} | {render} |\n",
        e.method.as_str().to_uppercase(),
        e.path,
    )
}

/// The honest footnote. Most handlers build their response with `serde_json::json!` and have no
/// struct to point at; the contract describes those in prose rather than inventing a DTO per
/// handler across the whole API. Saying how many there are is the difference between a known debt
/// and an unexamined one.
fn untyped_note(all: &[&Endpoint]) -> String {
    let untyped = all
        .iter()
        .filter(|e| matches!(e.response, TypeRef::Untyped(_)))
        .count();
    let typed = all
        .iter()
        .filter(|e| matches!(e.response, TypeRef::Named(_) | TypeRef::ArrayOf(_)))
        .count();
    format!(
        "## Response types\n\n\
         {typed} of {} endpoints return a named type that derives `schemars::JsonSchema`, so\n\
         `/openapi.json` describes their fields. The other {untyped} build their body with\n\
         `serde_json::json!` and have no struct to point at; the contract describes each in prose and\n\
         the generated document carries that prose instead of a field list. Turning one into a named\n\
         type is a strict improvement that needs no coordination — add the struct, derive\n\
         `JsonSchema`, bind it in `crates/api/src/schema_registry.rs`, and point the row at it.\n",
        all.len(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECKED_IN: &str = include_str!("../../../docs/API.md");

    /// The stale check. A generated document that nobody regenerates is a hand-kept document with
    /// extra steps, which is precisely what the counts in README used to be.
    #[test]
    fn the_checked_in_matrix_is_current() {
        let current = markdown();
        if std::env::var("LIGHTTRACK_WRITE_API_MATRIX").is_ok() {
            let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/API.md");
            std::fs::write(path, &current).expect("write docs/API.md");
            return;
        }
        assert_eq!(
            CHECKED_IN.replace("\r\n", "\n"),
            current,
            "docs/API.md is stale. Regenerate: LIGHTTRACK_WRITE_API_MATRIX=1 cargo test -p \
             lighttrack-contract matrix"
        );
    }

    /// The document is only worth generating if it says the things a reader came for.
    #[test]
    fn the_matrix_names_every_route_and_discloses_the_untyped_ones() {
        let md = markdown();
        for e in crate::endpoints() {
            assert!(md.contains(e.path), "{} is missing from the matrix", e.path);
        }
        assert!(md.contains("Reachable from neither MCP nor CLI | 0"));
        assert!(md.contains("## Response types"));
    }
}
