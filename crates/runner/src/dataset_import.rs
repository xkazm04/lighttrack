//! `dataset import` and the versioned side of `dataset build` (M24).
//!
//! The pre-M24 builder is a client loop: fetch the newest `n` events, scrub each one, POST it. That
//! shape can express exactly one sampling strategy, which is why `docs/BENCHMARK_FRAMEWORK.md` §1
//! promised four and shipped `recent`. A stratified quota or a uniform draw is a statement about the
//! *matched population*, and a client that has already fetched a page has thrown that away — so the
//! other three run on the server, and this module is the caller.
//!
//! The client loop is still here and still the default when `--llm-scrub` is asked for: the LLM
//! anonymization pass is a paid model call, and a store method is not the place to make one.

use anyhow::{Context, Result};
use serde_json::{json, Value};

use lighttrack_core::{Dataset, ImportSpec};

use crate::cli::Cli;
use crate::http::{get, post};

/// Resolve the dataset an import should write into, forking when the newest version is frozen.
///
/// This is what makes a *recurring* sampler coherent. The old cycle named each run's dataset after
/// its watermark, so a year of online sampling left 300 unrelated corpora that no version pin could
/// relate to one another. Now one name accumulates versions: v1 frozen after its window, v2 forked
/// from it for the next, and `dataset_version` finally moves.
pub(crate) fn open_version(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    project: &str,
    name: &str,
) -> Result<Dataset> {
    let versions: Vec<Dataset> = get(
        cli,
        http,
        &format!(
            "/v1/projects/{project}/datasets/versions?name={}",
            enc(name)
        ),
    )
    .with_context(|| format!("listing versions of dataset '{name}'"))?;

    match versions.first() {
        // Newest version is still open: keep filling it.
        Some(d) if !d.frozen => Ok(d.clone()),
        // Newest version is frozen — that is a checkpoint, not a dead end.
        Some(d) => {
            let raw = post(
                cli,
                http,
                &format!("/v1/datasets/{}/fork", d.id),
                &json!({}),
            )
            .with_context(|| format!("forking dataset '{name}' v{}", d.version))?;
            let forked: Dataset = serde_json::from_value(raw)?;
            println!("forked '{name}' v{} -> v{}", d.version, forked.version);
            Ok(forked)
        }
        None => {
            let raw = post(
                cli,
                http,
                &format!("/v1/projects/{project}/datasets"),
                &json!({ "name": name, "source": "events:recent" }),
            )
            .with_context(|| format!("creating dataset '{name}'"))?;
            Ok(serde_json::from_value(raw)?)
        }
    }
}

/// Run one server-side import into `dataset_id`. Returns how many cases were actually written —
/// which is not the number matched when `dedupe` is on, and the distinction is the point.
pub(crate) fn import_into(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    dataset_id: &str,
    spec: &ImportSpec,
) -> Result<u32> {
    let out: Value = post(
        cli,
        http,
        &format!("/v1/datasets/{dataset_id}/items/import"),
        &serde_json::to_value(spec)?,
    )
    .with_context(|| format!("importing into dataset '{dataset_id}'"))?;
    Ok(out.get("imported").and_then(Value::as_u64).unwrap_or(0) as u32)
}

/// `dataset import`: open (or fork) the named version, mine rows into it, optionally freeze.
pub(crate) fn run_import(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    project: &str,
    name: &str,
    spec: &ImportSpec,
    freeze: bool,
) -> Result<u32> {
    let ds = open_version(cli, http, project, name)?;
    let built = import_into(cli, http, &ds.id, spec)?;
    println!(
        "imported {built} case(s) into '{name}' v{} ({}, dedupe={})",
        ds.version,
        spec.source_tag(),
        spec.dedupe
    );
    if freeze && built > 0 {
        post(
            cli,
            http,
            &format!("/v1/datasets/{}/freeze", ds.id),
            &json!({}),
        )
        .map(|_: Value| ())
        .with_context(|| format!("freezing dataset '{name}' v{}", ds.version))?;
        println!("froze '{name}' v{}", ds.version);
    }
    Ok(built)
}

/// Percent-encode the few characters a dataset name can carry that would break a query string. Not
/// a general encoder: names are operator-chosen labels, and the failure this prevents is a `&` in
/// one silently truncating the parameter.
pub(crate) fn enc(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '&' => "%26".to_string(),
            '?' => "%3F".to_string(),
            '#' => "%23".to_string(),
            '+' => "%2B".to_string(),
            ' ' => "%20".to_string(),
            '/' => "%2F".to_string(),
            c => c.to_string(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::enc;

    /// A name carrying a `&` used to truncate the query parameter, which resolves to a *different*
    /// dataset's version history — the worst possible way to be wrong here.
    #[test]
    fn a_name_with_query_syntax_in_it_is_encoded() {
        assert_eq!(enc("golden&prod"), "golden%26prod");
        assert_eq!(enc("golden/checkout"), "golden%2Fcheckout");
        assert_eq!(enc("plain-name"), "plain-name");
    }
}
