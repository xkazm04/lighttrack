//! `lt alerts` — the fired-alert ledger from the operator's side.
//!
//! Before the ledger, "what did LightTrack tell us last week, and did anyone act on it" was a
//! question about the receiver's inbox. `list` answers the first half and `ack` records the second.

use anyhow::{bail, Result};
use reqwest::Method;
use serde_json::json;

use crate::cli::{AlertsCmd, Cli};
use crate::http::call;

/// Every kind the API's `?kind=` accepts, mirroring `AlertKind`'s wire literals. Checked
/// client-side so a typo costs nothing rather than a 400 the operator has to decode.
const KINDS: &[&str] = &[
    "limit_breach",
    "limit_warning",
    "forecast_alert",
    "relay_task_dead",
    "error_spike",
    "score_drop",
    "bench_run",
    "ingest_rejected",
];

pub(crate) fn run(cli: &Cli, action: &AlertsCmd) -> Result<()> {
    match action {
        AlertsCmd::List {
            project,
            kind,
            since,
            acked,
            open,
            limit,
            cursor,
        } => {
            if let Some(k) = kind {
                check_kind(k)?;
            }
            let path = list_path(
                project.as_deref(),
                kind.as_deref(),
                since.as_deref(),
                acked_filter(*acked, *open),
                *limit,
                cursor.as_deref(),
            );
            call(cli, Method::GET, &path, None, "list_alerts")
        }
        AlertsCmd::Ack { id, by } => call(
            cli,
            Method::POST,
            &format!("/v1/alerts/{id}/ack"),
            // An empty object rather than no body: the API takes `by` as optional, and sending an
            // object keeps the request shape identical whether or not the operator named themselves.
            Some(match by {
                Some(b) => json!({ "by": b }),
                None => json!({}),
            }),
            "",
        ),
        AlertsCmd::Channels { action } => crate::alert_channels::run(cli, action),
    }
}

/// `--acked` and `--open` are the two halves of one tri-state; clap keeps them exclusive.
fn acked_filter(acked: bool, open: bool) -> Option<bool> {
    match (acked, open) {
        (true, _) => Some(true),
        (_, true) => Some(false),
        _ => None,
    }
}

fn list_path(
    project: Option<&str>,
    kind: Option<&str>,
    since: Option<&str>,
    acked: Option<bool>,
    limit: usize,
    cursor: Option<&str>,
) -> String {
    let mut q = format!("/v1/alerts?limit={limit}");
    if let Some(p) = project {
        q.push_str(&format!("&project={p}"));
    }
    if let Some(k) = kind {
        q.push_str(&format!("&kind={k}"));
    }
    if let Some(s) = since {
        q.push_str(&format!("&since={s}"));
    }
    if let Some(a) = acked {
        q.push_str(&format!("&acked={a}"));
    }
    // The cursor comes back in the body as `next_cursor`; without sending it back, `list` can only
    // ever show page one — which reads as "that is all the alerts there were".
    if let Some(c) = cursor.filter(|s| !s.is_empty()) {
        q.push_str(&format!("&cursor={}", crate::query::encode(c)));
    }
    q
}

fn check_kind(kind: &str) -> Result<()> {
    if !KINDS.contains(&kind) {
        bail!(
            "unknown alert kind '{kind}': expected one of {}",
            KINDS.join(" | ")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_list_asks_only_for_a_page_size() {
        assert_eq!(
            list_path(None, None, None, None, 20, None),
            "/v1/alerts?limit=20"
        );
    }

    #[test]
    fn every_filter_reaches_the_query_string() {
        let q = list_path(
            Some("proj-a"),
            Some("score_drop"),
            Some("7d"),
            Some(false),
            5,
            Some("cur+1"),
        );
        assert!(q.starts_with("/v1/alerts?limit=5"));
        assert!(q.contains("&project=proj-a"), "{q}");
        assert!(q.contains("&kind=score_drop"), "{q}");
        assert!(q.contains("&since=7d"), "{q}");
        assert!(q.contains("&acked=false"), "{q}");
        assert!(
            q.contains("&cursor=cur%2B1"),
            "an opaque cursor is encoded: {q}"
        );
    }

    /// No `--cursor` must send no `cursor=` at all: an empty one is a position the API cannot
    /// resolve, and the honest first page is what a bare `list` means.
    #[test]
    fn an_absent_cursor_asks_for_the_first_page() {
        assert!(!list_path(None, None, None, None, 20, None).contains("cursor"));
        assert!(!list_path(None, None, None, None, 20, Some("")).contains("cursor"));
    }

    /// The tri-state is the point: without either flag the API must see no `acked=` at all, so both
    /// open and acknowledged alerts come back.
    #[test]
    fn neither_flag_means_no_ack_filter() {
        assert_eq!(acked_filter(false, false), None);
        assert_eq!(acked_filter(true, false), Some(true));
        assert_eq!(acked_filter(false, true), Some(false));
        assert!(!list_path(None, None, None, None, 20, None).contains("acked"));
    }

    #[test]
    fn an_unknown_kind_is_refused_before_the_round_trip() {
        assert!(check_kind("limit_breach").is_ok());
        let e = check_kind("limit-breach").unwrap_err().to_string();
        assert!(
            e.contains("limit_breach"),
            "the refusal must name what IS accepted: {e}"
        );
    }
}
