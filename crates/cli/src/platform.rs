//! The doors that describe the deployment rather than its data: liveness, the generated OpenAPI
//! document, what the store backend actually implements, and the two operational status views.
//!
//! `capabilities` is the one worth reaching for first when a surface comes back empty: a 501
//! `unsupported` there means "not ported on this backend", never "you have none".

use anyhow::Result;
use reqwest::Method;

use crate::cli::{Cli, IngestCmd, StorageCmd};
use crate::http::call;

pub(crate) fn health(cli: &Cli) -> Result<()> {
    call(cli, Method::GET, "/health", None, "")
}

pub(crate) fn openapi(cli: &Cli) -> Result<()> {
    call(cli, Method::GET, "/openapi.json", None, "")
}

pub(crate) fn capabilities(cli: &Cli) -> Result<()> {
    call(
        cli,
        Method::GET,
        "/v1/capabilities",
        None,
        "get_capabilities",
    )
}

pub(crate) fn ingest(cli: &Cli, action: &IngestCmd) -> Result<()> {
    match action {
        IngestCmd::Status => call(cli, Method::GET, "/v1/ingest/status", None, ""),
    }
}

pub(crate) fn storage(cli: &Cli, action: &StorageCmd) -> Result<()> {
    match action {
        StorageCmd::Status => call(cli, Method::GET, "/v1/storage/status", None, ""),
    }
}
