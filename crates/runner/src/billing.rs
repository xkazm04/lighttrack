//! `lt-runner billing sync` — backfill/reconcile revenue from a billing provider into LightTrack.
//!
//! Stripe today: pulls paid invoices since a cutoff, normalizes them with `lighttrack-billing`
//! (the same code the webhook uses), and POSTs each to `/v1/revenue` (idempotent by id). Needs
//! `STRIPE_API_KEY`. Network-bound, so unverified in CI without live creds.

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use lighttrack_billing::fx::shared_fx;
use lighttrack_billing::stripe::normalize_invoice;

use crate::cli::Cli;
use crate::http::post;

pub(crate) fn sync(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    provider: &str,
    project: &str,
    days: i64,
) -> Result<()> {
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let since = since_cutoff(now, days);
    match provider {
        "stripe" => sync_stripe(cli, http, project, since),
        other => Err(anyhow!("unsupported billing provider for sync: {other}")),
    }
}

/// Unix-seconds lower bound for the invoice pull: `now - days*86400`. A negative `--days` is clamped
/// to 0 so a bad flag can never set the cutoff into the future (which would pull everything).
fn since_cutoff(now: i64, days: i64) -> i64 {
    now - days.max(0) * 86_400
}

fn sync_stripe(cli: &Cli, http: &reqwest::blocking::Client, project: &str, since: i64) -> Result<()> {
    let key = std::env::var("STRIPE_API_KEY").context("STRIPE_API_KEY is not set")?;
    let mut starting_after: Option<String> = None;
    let mut total = 0usize;

    loop {
        let mut params: Vec<(String, String)> = vec![
            ("status".into(), "paid".into()),
            ("limit".into(), "100".into()),
            ("created[gte]".into(), since.to_string()),
        ];
        if let Some(after) = &starting_after {
            params.push(("starting_after".into(), after.clone()));
        }

        let resp: Value = http
            .get("https://api.stripe.com/v1/invoices")
            .query(&params)
            .bearer_auth(&key)
            .send()?
            .error_for_status()
            .context("Stripe invoices request failed")?
            .json()
            .context("decoding Stripe response")?;

        let data = resp.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
        if data.is_empty() {
            break;
        }
        let fx = shared_fx();
        for inv in &data {
            if let Some(mut ev) = normalize_invoice(inv, &fx) {
                ev.project_id = project.to_string();
                post(cli, http, "/v1/revenue", &serde_json::to_value(&ev)?)?;
                total += 1;
            }
        }
        if !resp.get("has_more").and_then(Value::as_bool).unwrap_or(false) {
            break;
        }
        starting_after = data
            .last()
            .and_then(|i| i.get("id"))
            .and_then(Value::as_str)
            .map(str::to_string);
    }

    println!("synced {total} paid invoice(s) from stripe → project {project}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::since_cutoff;

    const DAY: i64 = 86_400;

    #[test]
    fn subtracts_whole_days() {
        assert_eq!(since_cutoff(1_000_000, 1), 1_000_000 - DAY);
        assert_eq!(since_cutoff(1_000_000, 30), 1_000_000 - 30 * DAY);
    }

    #[test]
    fn zero_days_is_now() {
        assert_eq!(since_cutoff(1_000_000, 0), 1_000_000);
    }

    #[test]
    fn negative_days_clamped_to_now_not_future() {
        // A negative look-back must not push the cutoff past `now`.
        assert_eq!(since_cutoff(1_000_000, -5), 1_000_000);
    }
}
