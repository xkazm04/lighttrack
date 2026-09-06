//! Eval corpus lineage (M24): the version history of a dataset name, forking one, and mining rows
//! into one.
//!
//! Only the lineage verbs live here. Creating a dataset and appending a case by hand are already
//! `lt-runner dataset build` and the API's own routes; what had no CLI at all was the thing that
//! makes a frozen golden set extendable, and the thing that turns production traffic into cases
//! without a client loop.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::{json, Value};

use crate::cli::{Cli, DatasetsCmd};
use crate::http::call;

pub(crate) fn run(cli: &Cli, action: &DatasetsCmd) -> Result<()> {
    match action {
        DatasetsCmd::Versions { project, name } => call(
            cli,
            Method::GET,
            &format!(
                "/v1/projects/{project}/datasets/versions?name={}",
                enc(name)
            ),
            None,
            "list_datasets",
        ),
        DatasetsCmd::Fork { id } => call(
            cli,
            Method::POST,
            &format!("/v1/datasets/{id}/fork"),
            Some(json!({})),
            "get_dataset",
        ),
        DatasetsCmd::Import {
            id,
            from,
            strategy,
            n,
            below,
            model,
            status,
            dedupe,
        } => {
            let body = import_body(
                from,
                strategy,
                *n,
                *below,
                model.as_deref(),
                status.as_deref(),
                *dedupe,
            )?;
            call(
                cli,
                Method::POST,
                &format!("/v1/datasets/{id}/items/import"),
                Some(body),
                "list_dataset_items",
            )
        }
        DatasetsCmd::Promote { id, label_id } => call(
            cli,
            Method::POST,
            &format!("/v1/datasets/{id}/items/from-label"),
            Some(json!({ "label_id": label_id })),
            "",
        ),
        DatasetsCmd::Labels { id } => call(
            cli,
            Method::GET,
            &format!("/v1/datasets/{id}/labels"),
            None,
            "list_labels",
        ),
        DatasetsCmd::Items { id, difficulty } => {
            let q = match difficulty.as_deref() {
                Some(d) => format!("?difficulty={}", enc(tier(d)?)),
                None => String::new(),
            };
            call(
                cli,
                Method::GET,
                &format!("/v1/datasets/{id}/items{q}"),
                None,
                "list_dataset_items",
            )
        }
        DatasetsCmd::Add {
            id,
            input,
            expected,
            output,
            difficulty,
        } => {
            let mut body = json!({ "input": input });
            if let Some(e) = expected {
                body["expected"] = json!(e);
            }
            if let Some(o) = output {
                body["output"] = json!(o);
            }
            // Absent stays absent. Writing `"difficulty": null` would be the same thing on the wire
            // today, but sending the key at all invites a future reader to treat "stated as null"
            // as different from "not stated" — and the two must never diverge.
            if let Some(d) = difficulty {
                body["difficulty"] = json!(tier(d)?);
            }
            call(
                cli,
                Method::POST,
                &format!("/v1/datasets/{id}/items"),
                Some(body),
                "",
            )
        }
    }
}

/// The three rungs, spelled as the wire spells them.
///
/// Restated here for the reason [`STRATEGIES`] is: `lt` depends on the render crate and a HTTP
/// client, never on `lighttrack-core`, so it stays a thin operator tool that ships without the
/// engine. The cost is this list; the server rejects anything it does not know.
const TIERS: &[&str] = &["easy", "medium", "hard"];

/// Normalise one tier spelling, refusing an unknown one here rather than sending it.
///
/// A silent fallback would be worse here than anywhere else in this file: an operator who typed
/// `--difficulty hardd` and got the whole corpus back would read a mixed set as the hard tier and
/// conclude the cheap model handles the hard cases.
fn tier(s: &str) -> Result<&'static str> {
    let want = s.trim().to_ascii_lowercase();
    TIERS
        .iter()
        .copied()
        .find(|t| *t == want)
        .ok_or_else(|| anyhow::anyhow!("unknown --difficulty {s:?}: expected one of {TIERS:?}"))
}

/// The four sampling strategies and the two sources, spelled as the wire spells them.
///
/// Restated here rather than imported from `lighttrack-core`: `lt` deliberately depends on nothing
/// but the render crate and a HTTP client, so it stays a thin operator tool that can be built and
/// shipped without the engine. The cost is this list, and the server rejects anything it does not
/// know — so the worst a drift can produce is a 400 with the accepted set in it.
const STRATEGIES: &[&str] = &["recent", "random", "stratified", "errors"];
const SOURCES: &[&str] = &["events", "scores"];

/// Build the `ImportSpec` body, refusing an unknown spelling here rather than sending it.
///
/// A silent fallback is the failure worth avoiding: an operator who typed `--strategy startified`
/// and got `recent` would read the resulting corpus as stratified and draw conclusions from a
/// sample that never was one.
#[allow(clippy::too_many_arguments)]
fn import_body(
    from: &str,
    strategy: &str,
    n: usize,
    below: Option<f64>,
    model: Option<&str>,
    status: Option<&str>,
    dedupe: bool,
) -> Result<Value> {
    if !SOURCES.contains(&from) {
        bail!("unknown --from {from:?}: expected one of {SOURCES:?}");
    }
    if !STRATEGIES.contains(&strategy) {
        bail!("unknown --strategy {strategy:?}: expected one of {STRATEGIES:?}");
    }
    // `--below` is a failure question by construction; asking it while sampling `recent` would mine
    // the newest cases that happen to be bad rather than the bad ones.
    let strategy = if below.is_some() && strategy == "recent" {
        "errors"
    } else {
        strategy
    };

    let mut filter = serde_json::Map::new();
    if let Some(b) = below {
        filter.insert("below".into(), json!(b));
    }
    if let Some(m) = model {
        filter.insert("model".into(), json!(m));
    }
    if let Some(s) = status {
        filter.insert("status".into(), json!(s));
    }
    let mut body = json!({
        "from": from,
        "strategy": strategy,
        "n": n,
        "dedupe": dedupe,
    });
    if !filter.is_empty() {
        body["filter"] = Value::Object(filter);
    }
    Ok(body)
}

/// Percent-encode the characters a dataset name can carry that would break a query string. Not a
/// general encoder: names are operator-chosen labels, and the failure this prevents is a `&` in one
/// silently truncating the parameter — which resolves to a *different* name's history.
fn enc(s: &str) -> String {
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
    use super::*;

    #[test]
    fn an_unknown_strategy_or_source_is_refused_before_the_request() {
        assert!(import_body("events", "startified", 10, None, None, None, false).is_err());
        assert!(import_body("labels", "recent", 10, None, None, None, false).is_err());
        assert!(import_body("scores", "errors", 10, None, None, None, true).is_ok());
    }

    /// `--below` without a strategy is a failure question, and must not be answered with the newest
    /// cases that happen to be bad.
    #[test]
    fn below_promotes_a_default_recent_sample_to_errors() {
        let body = import_body("scores", "recent", 10, Some(0.4), None, None, false).expect("body");
        assert_eq!(body["strategy"], "errors");
        assert_eq!(body["filter"]["below"], 0.4);
        // An explicit strategy is never overridden.
        let body =
            import_body("scores", "stratified", 10, Some(0.4), None, None, false).expect("body");
        assert_eq!(body["strategy"], "stratified");
    }

    #[test]
    fn the_filter_is_absent_when_nothing_narrows() {
        let body = import_body("events", "random", 25, None, None, None, true).expect("body");
        assert!(
            body.get("filter").is_none(),
            "no empty filter object: {body}"
        );
        assert_eq!(body["n"], 25);
        assert_eq!(body["dedupe"], true);
    }

    /// The tier reaches the wire in exactly one spelling, and a typo never becomes a full listing.
    #[test]
    fn a_tier_is_normalised_and_an_unknown_one_is_refused_before_the_request() {
        assert_eq!(tier("Hard").expect("hard"), "hard");
        assert_eq!(tier("  medium ").expect("medium"), "medium");
        assert!(tier("hardd").is_err());
        assert!(tier("").is_err());
        // No fourth rung is silently accepted: the ladder the server enforces is the one `lt` sends.
        assert!(tier("extreme").is_err());
    }

    #[test]
    fn a_name_with_query_syntax_in_it_is_encoded() {
        assert_eq!(enc("golden&prod"), "golden%26prod");
        assert_eq!(enc("golden/checkout"), "golden%2Fcheckout");
        assert_eq!(enc("plain-name"), "plain-name");
    }
}
