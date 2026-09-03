//! **The** description of the LightTrack HTTP API — one table, four generated surfaces.
//!
//! Before this crate the route surface was described by hand five times: the axum router, the MCP
//! tool catalog with its inline JSON schemas, the clap verb tree, the Markdown render dispatch, and
//! `ROUTE_SCOPES`'s auth table. Drift was not hypothetical — an `outputSchema` lagged its input
//! schema, several render kinds had no producer, and sixteen routes were reachable from neither the
//! MCP server nor the CLI. Nothing held them together because nothing *could*: they were five
//! independent lists of strings.
//!
//! Here they are one list. [`endpoints`] yields every endpoint with its access rule, parameters,
//! response shape, and which of MCP / CLI / renderer covers it; [`openapi`] renders the OpenAPI
//! document from it; [`mcp`] builds the tool catalog from it; the API, CLI and renderer hold
//! themselves to it in tests that fail when a surface and the table disagree.
//!
//! Adding an endpoint therefore means adding a row — and the tests then say, in order, that the
//! router serves it, that someone decided who may call it, and whether an agent or an operator can
//! reach it at all.

mod dsl;
mod endpoints;
pub mod matrix;
pub mod mcp;
mod nested;
pub mod openapi;
mod types;

pub use types::{Access, Endpoint, JsonTy, KeyScope, McpTool, Method, Param, ParamKind, TypeRef};

/// Every endpoint in the contract, in router order.
pub fn endpoints() -> impl Iterator<Item = &'static Endpoint> {
    endpoints::GROUPS.iter().copied().flatten()
}

/// The endpoint with this id, if the table has one.
pub fn endpoint(id: &str) -> Option<&'static Endpoint> {
    endpoints().find(|e| e.id == id)
}

/// Every distinct `/v1/...` route path the contract declares. This is the set the API's bijection
/// test holds `build_router` to.
pub fn route_paths() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = endpoints()
        .map(|e| e.path)
        .filter(|p| p.starts_with("/v1"))
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// The two endpoints (`read`, `write`) declared for one path: the `GET`, and the first of
/// `POST`/`PUT`/`DELETE`. The shape `ROUTE_SCOPES` used to carry as two columns on one row.
pub fn access_for(path: &str) -> (Option<Access>, Option<Access>) {
    let mut read = None;
    let mut write = None;
    for e in endpoints().filter(|e| e.path == path) {
        if e.method.is_read() {
            read = Some(e.access);
        } else if write.is_none() {
            write = Some(e.access);
        }
    }
    (read, write)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_endpoint_id_is_unique() {
        let mut seen: Vec<&str> = endpoints().map(|e| e.id).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(
            before,
            seen.len(),
            "duplicate endpoint id — ids are OpenAPI operationIds and must address exactly one row"
        );
    }

    #[test]
    fn no_method_is_declared_twice_for_one_path() {
        let mut seen: Vec<(&str, &str)> =
            endpoints().map(|e| (e.path, e.method.as_str())).collect();
        let before = seen.len();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(before, seen.len(), "two rows claim the same method+path");
    }

    /// A row that names no method at all was a typo in the old table; a row that names a path
    /// without a leading slash is the same class of mistake.
    #[test]
    fn every_row_is_addressable() {
        for e in endpoints() {
            assert!(!e.id.is_empty(), "an endpoint has no id");
            assert!(e.path.starts_with('/'), "{}: path must be absolute", e.id);
            assert!(!e.doc.is_empty(), "{}: undocumented endpoint", e.id);
        }
    }

    /// Every `:segment` of a path must be a declared `Path` parameter, or the generated OpenAPI
    /// document is invalid and the MCP tool cannot bind the id it needs.
    #[test]
    fn every_path_segment_is_a_declared_parameter() {
        for e in endpoints() {
            for seg in e.path.split('/').filter(|s| s.starts_with(':')) {
                let name = &seg[1..];
                assert!(
                    e.params
                        .iter()
                        .any(|p| p.kind == ParamKind::Path && p.name == name),
                    "{}: path segment ':{name}' has no declared Path param",
                    e.id
                );
            }
        }
    }

    /// A `mutating` row must not be annotated read-only over MCP, and a non-mutating one must be:
    /// that annotation is what an agent host reads before deciding what it may call unattended.
    #[test]
    fn the_mcp_read_only_hint_follows_the_mutating_flag() {
        for e in endpoints() {
            if let Some(t) = &e.mcp {
                assert_eq!(
                    t.read_only, !e.mutating,
                    "{}: readOnlyHint contradicts `mutating`",
                    e.id
                );
            }
        }
    }

    /// A tool argument that names no parameter would be silently dropped when the tool is called.
    #[test]
    fn every_mcp_argument_names_a_real_parameter() {
        for e in endpoints() {
            if let Some(t) = &e.mcp {
                for a in t.args {
                    assert!(
                        e.param(a).is_some(),
                        "{}: MCP tool '{}' exposes '{a}', which is not a parameter of it",
                        e.id,
                        t.name
                    );
                }
            }
        }
    }

    /// The coverage property the whole item exists to establish: no route is reachable from
    /// neither the MCP server nor the CLI. An operator or an agent can get at every one of them.
    #[test]
    fn every_endpoint_is_reachable_from_mcp_or_the_cli() {
        let orphans: Vec<&str> = endpoints()
            .filter(|e| e.mcp.is_none() && e.cli.is_none() && !e.machine)
            .map(|e| e.id)
            .collect();
        assert!(
            orphans.is_empty(),
            "these endpoints are reachable from neither MCP nor the CLI, and none of them is \
             declared a machine door: {orphans:?}"
        );
    }
}
