//! Collective Model Intelligence: read the merged leaderboard, preview this instance's digest, and
//! contribute it to (or withdraw it from) a hub.

use anyhow::Result;
use reqwest::Method;
use serde_json::Value;

use crate::cli::{Cli, CollectiveCmd};
use crate::http::{call, client};

/// Hub URLs are operator-typed, so a trailing slash is tolerated rather than doubling the separator
/// in the endpoint path (and in the message printed back).
pub(crate) fn normalize_hub(hub: &str) -> &str {
    hub.trim_end_matches('/')
}

pub(crate) fn digest_path(min_cases: u32) -> String {
    format!("/v1/collective/digest?min_cases={min_cases}")
}

/// Leaderboard query string. The optional filters append only when set; the two rigor booleans are
/// opt-in narrowing flags — present ⇒ `=true`, absent ⇒ no filter (never `=false`, which would ask
/// the hub for the *un*-rigorous rows).
pub(crate) fn leaderboard_path(
    task_type: &Option<String>,
    provider: &Option<String>,
    judge: &Option<String>,
    determinism: &Option<String>,
    frozen: bool,
    tested: bool,
) -> String {
    let mut p = "/v1/collective/leaderboard".to_string();
    let mut sep = '?';
    for (k, v) in [
        ("task_type", task_type),
        ("provider", provider),
        ("judge", judge),
        ("determinism", determinism),
    ] {
        if let Some(val) = v {
            p.push_str(&format!("{sep}{k}={val}"));
            sep = '&';
        }
    }
    for (k, on) in [("frozen_dataset", frozen), ("significance_tested", tested)] {
        if on {
            p.push_str(&format!("{sep}{k}=true"));
            sep = '&';
        }
    }
    p
}

pub(crate) fn run(cli: &Cli, action: &CollectiveCmd) -> Result<()> {
    match action {
        CollectiveCmd::Leaderboard { task_type, provider, judge, determinism, frozen, tested } => {
            let p = leaderboard_path(task_type, provider, judge, determinism, *frozen, *tested);
            call(cli, Method::GET, &p, None, "get_collective_leaderboard")
        }
        CollectiveCmd::Digest { min_cases } => call(
            cli,
            Method::GET,
            &digest_path(*min_cases),
            None,
            "get_collective_digest",
        ),
        CollectiveCmd::Contribute { hub, min_cases, hub_key } => {
            contribute(cli, hub, *min_cases, hub_key.as_deref())
        }
        CollectiveCmd::Withdraw { hub, hub_key } => withdraw(hub, hub_key.as_deref()),
    }
}

/// Build this instance's digest (from its own API) and POST it to a hub's ingest endpoint. Two hops:
/// `GET /v1/collective/digest` here → `POST /v1/collective/ingest` there. Keeps cross-instance push in
/// the CLI rather than baking outbound calls into the API.
fn contribute(cli: &Cli, hub: &str, min_cases: u32, hub_key: Option<&str>) -> Result<()> {
    let client = client();

    let mut req = client.get(format!("{}{}", cli.base, digest_path(min_cases)));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        eprintln!("build digest failed: HTTP {} — {}", resp.status().as_u16(), resp.text()?);
        std::process::exit(1);
    }
    let digest: Value = resp.json()?;
    let n = digest_bucket_count(&digest);
    if n == 0 {
        println!("nothing to contribute: no (model, task) bucket reached the k≥{min_cases} floor yet.");
        return Ok(());
    }

    let hub_base = normalize_hub(hub);
    let mut req = client.post(format!("{hub_base}/v1/collective/ingest")).json(&digest);
    if let Some(k) = hub_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if status.is_success() {
        println!("contributed {n} bucket(s) to {hub_base}: {text}");
    } else {
        eprintln!("contribute failed: HTTP {} — {text}", status.as_u16());
        std::process::exit(1);
    }
    Ok(())
}

/// How many (model, task) buckets a digest carries. A digest with no `entries` array is treated as
/// empty rather than as an error: there is simply nothing to publish.
fn digest_bucket_count(digest: &Value) -> usize {
    digest.get("entries").and_then(Value::as_array).map(Vec::len).unwrap_or(0)
}

/// Ask a hub to delete everything this instance contributed. The hub identifies the source from the
/// key, so withdrawal needs exactly the credential the contribution was made with.
fn withdraw(hub: &str, hub_key: Option<&str>) -> Result<()> {
    let hub_base = normalize_hub(hub);
    let mut req = client().delete(format!("{hub_base}/v1/collective/contribution"));
    if let Some(k) = hub_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if status.is_success() {
        println!("withdrawn from {hub_base}: {text}");
    } else {
        eprintln!("withdraw failed: HTTP {} - {text}", status.as_u16());
        std::process::exit(1);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn leaderboard_path_is_bare_when_nothing_is_filtered() {
        assert_eq!(
            leaderboard_path(&None, &None, &None, &None, false, false),
            "/v1/collective/leaderboard"
        );
    }

    /// The first filter must open the query with `?` and every later one join with `&`, whichever
    /// subset the operator passed.
    #[test]
    fn first_filter_opens_the_query_whichever_it_is() {
        assert_eq!(
            leaderboard_path(&s("qa"), &None, &None, &None, false, false),
            "/v1/collective/leaderboard?task_type=qa"
        );
        assert_eq!(
            leaderboard_path(&None, &None, &None, &None, true, false),
            "/v1/collective/leaderboard?frozen_dataset=true"
        );
        assert_eq!(
            leaderboard_path(&None, &s("openai"), &s("anthropic"), &None, false, true),
            "/v1/collective/leaderboard?provider=openai&judge=anthropic&significance_tested=true"
        );
    }

    /// An unset rigor flag sends nothing at all — `frozen=false` would ask the hub for the rows that
    /// were *not* run against a frozen dataset, the opposite of "don't filter".
    #[test]
    fn unset_rigor_flags_are_omitted_never_sent_as_false() {
        let p = leaderboard_path(&s("qa"), &None, &None, &s("exact"), false, false);
        assert_eq!(p, "/v1/collective/leaderboard?task_type=qa&determinism=exact");
        assert!(!p.contains("frozen_dataset") && !p.contains("significance_tested"));
    }

    #[test]
    fn all_filters_together_keep_their_order() {
        assert_eq!(
            leaderboard_path(&s("qa"), &s("openai"), &s("google"), &s("sampled"), true, true),
            "/v1/collective/leaderboard?task_type=qa&provider=openai&judge=google\
             &determinism=sampled&frozen_dataset=true&significance_tested=true"
        );
    }

    #[test]
    fn digest_path_carries_the_k_anonymity_floor() {
        assert_eq!(digest_path(5), "/v1/collective/digest?min_cases=5");
    }

    #[test]
    fn normalize_hub_tolerates_trailing_slashes() {
        assert_eq!(normalize_hub("https://hub.example"), "https://hub.example");
        assert_eq!(normalize_hub("https://hub.example/"), "https://hub.example");
        assert_eq!(normalize_hub("https://hub.example///"), "https://hub.example");
    }

    #[test]
    fn digest_bucket_count_reads_entries_and_tolerates_its_absence() {
        assert_eq!(digest_bucket_count(&json!({ "entries": [1, 2, 3] })), 3);
        assert_eq!(digest_bucket_count(&json!({ "entries": [] })), 0);
        assert_eq!(digest_bucket_count(&json!({})), 0);
        // A non-array `entries` is not a bucket list — count it as nothing to publish, not a panic.
        assert_eq!(digest_bucket_count(&json!({ "entries": 7 })), 0);
    }
}
