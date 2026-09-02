//! Collective Model Intelligence: read the merged leaderboard, preview this instance's digest, and
//! contribute it to (or withdraw it from) a hub.

use anyhow::Result;
use reqwest::Method;
use serde_json::{json, Value};

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
        CollectiveCmd::Leaderboard {
            task_type,
            provider,
            judge,
            determinism,
            frozen,
            tested,
        } => {
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
        CollectiveCmd::Contribute {
            hub,
            min_cases,
            hub_key,
            hub_key_ref,
            force,
            direct,
        } => {
            if *direct {
                contribute_direct(cli, hub, *min_cases, hub_key.as_deref())
            } else {
                call(
                    cli,
                    Method::POST,
                    "/v1/collective/contribute",
                    Some(contribute_body(
                        hub,
                        *min_cases,
                        hub_key_ref.as_deref(),
                        *force,
                    )),
                    "post_collective_contribute",
                )
            }
        }
        CollectiveCmd::History { limit, cursor } => call(
            cli,
            Method::GET,
            &history_path(*limit, cursor.as_deref()),
            None,
            "get_collective_contributions",
        ),
        CollectiveCmd::Withdraw {
            hub,
            hub_key,
            all,
            hub_key_ref,
        } => {
            if *all {
                call(
                    cli,
                    Method::DELETE,
                    &withdraw_all_path(hub.as_deref(), hub_key_ref.as_deref()),
                    None,
                    "delete_collective_contribution_all",
                )
            } else {
                let Some(hub) = hub else {
                    anyhow::bail!(
                        "`lt collective withdraw` needs --hub <url>, or --all to withdraw from \
                         every hub the contribution ledger says holds our data"
                    );
                };
                withdraw(hub, hub_key.as_deref())
            }
        }
    }
}

/// The body of a ledgered contribution. The hub key is referenced **by env-var name**, never sent:
/// a request body is logged, retried and stored in ways a credential must not be.
pub(crate) fn contribute_body(
    hub: &str,
    min_cases: u32,
    hub_key_ref: Option<&str>,
    force: bool,
) -> Value {
    let mut b = json!({ "hub": normalize_hub(hub), "min_cases": min_cases });
    if force {
        b["force"] = json!(true);
    }
    if let Some(r) = hub_key_ref {
        b["hub_key_ref"] = json!(r);
    }
    b
}

pub(crate) fn history_path(limit: usize, cursor: Option<&str>) -> String {
    match cursor {
        Some(c) if !c.is_empty() => {
            format!("/v1/collective/contributions?limit={limit}&cursor={c}")
        }
        _ => format!("/v1/collective/contributions?limit={limit}"),
    }
}

/// `?all=1`, plus the hub the operator named (the ledger stores an opaque hash, not an address) and
/// the env-var name the server should read its key from.
pub(crate) fn withdraw_all_path(hub: Option<&str>, hub_key_ref: Option<&str>) -> String {
    let mut p = "/v1/collective/contribution?all=1".to_string();
    if let Some(h) = hub.filter(|h| !h.is_empty()) {
        p.push_str(&format!("&hub={}", urlencode(normalize_hub(h))));
    }
    if let Some(r) = hub_key_ref.filter(|r| !r.is_empty()) {
        p.push_str(&format!("&hub_key_ref={r}"));
    }
    p
}

/// Minimal percent-encoding for a URL carried **inside** a query value: only the characters that
/// would otherwise end the value or be read as structure.
fn urlencode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            ':' => "%3A".to_string(),
            '/' => "%2F".to_string(),
            '?' => "%3F".to_string(),
            '&' => "%26".to_string(),
            '=' => "%3D".to_string(),
            '#' => "%23".to_string(),
            ' ' => "%20".to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// The pre-M22 client-side push, kept behind `--direct` for an air-gapped hub the API itself cannot
/// reach. Nothing is recorded and nothing is hash-gated — the ack is printed and discarded, which is
/// precisely the gap `POST /v1/collective/contribute` closes.
///
/// Two hops: `GET /v1/collective/digest` here → `POST /v1/collective/ingest` there.
fn contribute_direct(cli: &Cli, hub: &str, min_cases: u32, hub_key: Option<&str>) -> Result<()> {
    let client = client();

    let mut req = client.get(format!("{}{}", cli.base, digest_path(min_cases)));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        eprintln!(
            "build digest failed: HTTP {} — {}",
            resp.status().as_u16(),
            resp.text()?
        );
        std::process::exit(1);
    }
    let digest: Value = resp.json()?;
    let n = digest_bucket_count(&digest);
    if n == 0 {
        println!(
            "nothing to contribute: no (model, task) bucket reached the k≥{min_cases} floor yet."
        );
        return Ok(());
    }

    let hub_base = normalize_hub(hub);
    let mut req = client
        .post(format!("{hub_base}/v1/collective/ingest"))
        .json(&digest);
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
    digest
        .get("entries")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0)
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
        assert_eq!(
            p,
            "/v1/collective/leaderboard?task_type=qa&determinism=exact"
        );
        assert!(!p.contains("frozen_dataset") && !p.contains("significance_tested"));
    }

    #[test]
    fn all_filters_together_keep_their_order() {
        assert_eq!(
            leaderboard_path(
                &s("qa"),
                &s("openai"),
                &s("google"),
                &s("sampled"),
                true,
                true
            ),
            "/v1/collective/leaderboard?task_type=qa&provider=openai&judge=google\
             &determinism=sampled&frozen_dataset=true&significance_tested=true"
        );
    }

    #[test]
    fn digest_path_carries_the_k_anonymity_floor() {
        assert_eq!(digest_path(5), "/v1/collective/digest?min_cases=5");
    }

    /// The credential must be named, never carried: this body is logged by proxies and stored by
    /// the schedule the same push can run from.
    #[test]
    fn the_contribute_body_references_the_key_and_never_contains_it() {
        let b = contribute_body("https://hub.example/", 5, Some("MY_HUB_KEY"), false);
        assert_eq!(
            b["hub"], "https://hub.example",
            "the slash is normalized off"
        );
        assert_eq!(b["min_cases"], 5);
        assert_eq!(b["hub_key_ref"], "MY_HUB_KEY");
        assert!(
            b.get("force").is_none(),
            "force is sent only when asked: {b}"
        );
        assert!(
            b.get("hub_key").is_none(),
            "the key itself must never be in the body: {b}"
        );
        assert_eq!(
            contribute_body("https://hub.example", 5, None, true)["force"],
            true
        );
    }

    #[test]
    fn history_pages_forward_only_when_given_a_cursor() {
        assert_eq!(
            history_path(20, None),
            "/v1/collective/contributions?limit=20"
        );
        assert_eq!(
            history_path(5, Some("abc123")),
            "/v1/collective/contributions?limit=5&cursor=abc123"
        );
        // An empty cursor is "no cursor", not a page-zero request the server has to reject.
        assert_eq!(
            history_path(5, Some("")),
            "/v1/collective/contributions?limit=5"
        );
    }

    /// A hub URL sits *inside* a query value, so its `:` and `/` must be encoded or the server
    /// reads a truncated address and reports the real hub as unresolvable.
    #[test]
    fn withdraw_all_encodes_the_hub_it_carries() {
        assert_eq!(
            withdraw_all_path(None, None),
            "/v1/collective/contribution?all=1"
        );
        let p = withdraw_all_path(Some("https://hub.example/"), Some("MY_HUB_KEY"));
        assert_eq!(
            p,
            "/v1/collective/contribution?all=1&hub=https%3A%2F%2Fhub.example&hub_key_ref=MY_HUB_KEY"
        );
        assert!(
            !p.contains("://"),
            "the raw separator must not survive: {p}"
        );
    }

    #[test]
    fn normalize_hub_tolerates_trailing_slashes() {
        assert_eq!(normalize_hub("https://hub.example"), "https://hub.example");
        assert_eq!(normalize_hub("https://hub.example/"), "https://hub.example");
        assert_eq!(
            normalize_hub("https://hub.example///"),
            "https://hub.example"
        );
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
