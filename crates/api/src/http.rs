//! The API's outbound HTTP seam: a thin `post_json`, and nothing else.
//!
//! The API is overwhelmingly an *inbound* service; the two places it dials out are alert delivery
//! (which owns its own client, because a webhook has redirect and body-size rules of its own) and
//! the collective contribution push. This module is the second one. It exists as a module rather
//! than an inline `reqwest` call so the timeout, the redirect policy and the body cap are decided
//! once — an outbound call from a request handler is a place a server hangs.

use std::time::Duration;

use serde::Serialize;

/// How long an outbound push may take before it is abandoned. Generous — a hub merges a digest
/// synchronously — but finite, because the caller is a request handler (or a job) and neither may
/// wait on a hub that is simply gone.
const TIMEOUT: Duration = Duration::from_secs(30);

/// Cap on the answer we read back. An ack is a small JSON object; a hub (or something wearing its
/// URL) answering with a gigabyte must not become this process's memory problem.
const MAX_BODY: usize = 64 * 1024;

/// What one outbound call produced. There is no `Result` here on purpose: a transport failure is
/// not an error the *handler* propagates — it is an outcome the ledger records, which is the whole
/// point of the ledger.
pub(crate) struct Answer {
    /// `None` when the request never got a reply (DNS, TLS, connect, timeout).
    pub(crate) status: Option<u16>,
    /// The response body (truncated to [`MAX_BODY`]), or the transport error's message.
    pub(crate) body: String,
}

impl Answer {
    pub(crate) fn ok(&self) -> bool {
        self.status.is_some_and(|s| (200..300).contains(&s))
    }
}

/// Build the client used for outbound pushes. Redirects are **not** followed: a hub that redirects
/// is a hub whose URL the operator should fix, and a silently-followed redirect is how a bearer
/// token ends up at a host nobody configured.
pub(crate) fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(TIMEOUT)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default()
}

/// POST `body` as JSON to `url`, optionally bearer-authenticated. Never fails: see [`Answer`].
pub(crate) async fn post_json<T: Serialize>(
    http: &reqwest::Client,
    url: &str,
    bearer: Option<&str>,
    body: &T,
) -> Answer {
    let mut req = http.post(url).json(body);
    if let Some(k) = bearer {
        req = req.bearer_auth(k);
    }
    send(req).await
}

/// DELETE `url`, optionally bearer-authenticated. Never fails: see [`Answer`].
pub(crate) async fn delete(http: &reqwest::Client, url: &str, bearer: Option<&str>) -> Answer {
    let mut req = http.delete(url);
    if let Some(k) = bearer {
        req = req.bearer_auth(k);
    }
    send(req).await
}

async fn send(req: reqwest::RequestBuilder) -> Answer {
    match req.send().await {
        Err(e) => Answer {
            status: None,
            body: e.to_string(),
        },
        Ok(resp) => {
            let status = resp.status().as_u16();
            Answer {
                status: Some(status),
                body: capped_body(resp).await,
            }
        }
    }
}

async fn capped_body(resp: reqwest::Response) -> String {
    match resp.text().await {
        Ok(mut t) => {
            if t.len() > MAX_BODY {
                t.truncate(MAX_BODY);
                t.push_str("…(truncated)");
            }
            t
        }
        Err(e) => format!("(unreadable response body: {e})"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_2xx_counts_as_landed() {
        let a = |s: Option<u16>| Answer {
            status: s,
            body: String::new(),
        };
        assert!(a(Some(200)).ok());
        assert!(a(Some(204)).ok());
        assert!(!a(Some(429)).ok(), "a min_interval refusal did not land");
        assert!(!a(Some(500)).ok());
        assert!(
            !a(Some(301)).ok(),
            "redirects are not followed, so not landed"
        );
        assert!(!a(None).ok(), "no answer at all is not a landing");
    }
}
