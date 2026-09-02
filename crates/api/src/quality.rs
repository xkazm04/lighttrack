//! `GET /v1/quality/prompts` — how good each **served prompt version** actually is.
//!
//! `GET /v1/costs/prompts` has always answered "did v4 cost less than v3", because
//! `cost_by_dimension` groups spend by the `metadata.prompt` tag every event produced with a
//! registry prompt carries. The other half of that question — "is v4 any *good*" — had no surface
//! at all: promotion was the last thing the registry observed about a version, and a regression in
//! production was visible only to whoever happened to scroll `/v1/scores`.
//!
//! This is the read half of the loop (`prompt_canary_sweep` is the acting half). It is deliberately
//! a *measurement*, not a scoreboard: every row carries `n` and a ~95% interval beside its mean, so
//! a version judged four times cannot be read as beating one judged four hundred.

use axum::{
    extract::{Query, State},
    http::HeaderMap,
    Json,
};
use chrono::{Duration, Utc};
use serde::{Deserialize, Serialize};

use lighttrack_core::Dimension;
use lighttrack_store::ScoreSummaryRow;

use crate::error::ApiError;
use crate::events_query::parse_opt_ts;
use crate::guards::{authenticate, resolve_read_project};
use crate::state::{spawn_db, AppState};

/// Window used when the caller names none. Long enough that a version promoted last week has
/// accumulated evidence, short enough that it describes what is being served now.
const DEFAULT_WINDOW_DAYS: i64 = 7;

#[derive(Deserialize)]
pub(crate) struct QualityParams {
    project: Option<String>,
    /// RFC3339 lower bound on the **verdict's** time. Defaults to 7 days ago.
    since: Option<String>,
    until: Option<String>,
    /// Narrow to one rubric — the only way two versions are compared on the same criteria.
    rubric_id: Option<String>,
}

/// One version's quality, with the evidence behind it.
///
/// `version` and `name` are split out of the tag because the tag is what the store groups on and a
/// name is what a human reads; both are returned so a client never has to parse `"x@v3"` itself and
/// get the split wrong for a prompt whose name contains an `@`.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct PromptQualityRow {
    /// The `metadata.prompt` tag, or `null` for events carrying none — an untagged bucket, kept so
    /// the parts sum to the whole. An operator reading a large untagged bucket has learned
    /// something real: their app is not stamping the tag.
    pub(crate) tag: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) version: Option<u32>,
    pub(crate) n: u64,
    pub(crate) mean: f64,
    pub(crate) pass_rate: f64,
    pub(crate) ci95_low: f64,
    pub(crate) ci95_high: f64,
    /// What the judged events in this bucket cost — the spend the quality number is about.
    pub(crate) cost_usd: f64,
}

/// Split a `"<name>@v<version>"` tag. `rsplit_once` on purpose: a prompt named `a@b` still resolves,
/// because the version suffix is the LAST `@v…` and not the first.
pub(crate) fn split_tag(tag: &str) -> Option<(String, u32)> {
    let (name, v) = tag.rsplit_once("@v")?;
    if name.is_empty() {
        return None;
    }
    Some((name.to_string(), v.parse().ok()?))
}

impl From<ScoreSummaryRow> for PromptQualityRow {
    fn from(r: ScoreSummaryRow) -> Self {
        let split = r.key.as_deref().and_then(split_tag);
        PromptQualityRow {
            name: split.as_ref().map(|(n, _)| n.clone()),
            version: split.map(|(_, v)| v),
            tag: r.key,
            n: r.n,
            mean: r.mean,
            pass_rate: r.pass_rate,
            ci95_low: r.ci95_low,
            ci95_high: r.ci95_high,
            cost_usd: r.cost_usd,
        }
    }
}

/// Per-served-version quality over a window. Read-scoped like every other rollup: a project key
/// sees only its own project.
pub(crate) async fn get_prompt_quality(
    State(st): State<AppState>,
    headers: HeaderMap,
    Query(q): Query<QualityParams>,
) -> Result<Json<Vec<PromptQualityRow>>, ApiError> {
    let p = authenticate(&st, &headers).await?;
    let project = resolve_read_project(&p, q.project.as_deref())?;
    let until = parse_opt_ts("until", q.until.as_deref())?;
    let since = parse_opt_ts("since", q.since.as_deref())?
        .unwrap_or_else(|| Utc::now() - Duration::days(DEFAULT_WINDOW_DAYS));
    if let Some(u) = until {
        if u < since {
            return Err(ApiError::bad_request(
                "window end precedes its start".to_string(),
            ));
        }
    }

    let store = st.store.clone();
    let rubric = q.rubric_id.clone();
    let rows = spawn_db(move || {
        store.score_summary_by_dimension(
            project.as_deref().into(),
            Dimension::Prompt,
            since,
            until,
            rubric.as_deref(),
        )
    })
    .await?;

    let mut out: Vec<PromptQualityRow> = rows.into_iter().map(PromptQualityRow::from).collect();
    // Newest version of each prompt first, then by name; the untagged bucket last, because it is
    // context rather than a version anyone promoted.
    out.sort_by(|a, b| match (&a.tag, &b.tag) {
        (None, None) => std::cmp::Ordering::Equal,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (Some(_), None) => std::cmp::Ordering::Less,
        (Some(_), Some(_)) => a
            .name
            .cmp(&b.name)
            .then_with(|| b.version.cmp(&a.version))
            .then_with(|| a.tag.cmp(&b.tag)),
    });
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_splits_on_its_last_version_suffix() {
        assert_eq!(
            split_tag("support-reply@v12"),
            Some(("support-reply".into(), 12))
        );
        // A name containing `@v` must still resolve to the trailing version, not the first one.
        assert_eq!(split_tag("a@v1@v2"), Some(("a@v1".into(), 2)));
        // Anything that is not the convention stays an opaque tag rather than being half-parsed.
        assert_eq!(split_tag("support-reply"), None);
        assert_eq!(split_tag("support-reply@vX"), None);
        assert_eq!(split_tag("@v3"), None);
    }

    fn row(key: Option<&str>) -> PromptQualityRow {
        PromptQualityRow::from(ScoreSummaryRow {
            key: key.map(str::to_string),
            n: 3,
            mean: 0.8,
            pass_rate: 1.0,
            ci95_low: 0.7,
            ci95_high: 0.9,
            cost_usd: 0.5,
        })
    }

    #[test]
    fn a_row_carries_both_the_tag_and_its_parts() {
        let r = row(Some("support-reply@v4"));
        assert_eq!(r.tag.as_deref(), Some("support-reply@v4"));
        assert_eq!(r.name.as_deref(), Some("support-reply"));
        assert_eq!(r.version, Some(4));
        assert_eq!(r.n, 3);

        // An untagged bucket is a row, not an omission: a large one means the app is not stamping
        // the tag, which is a finding rather than an absence.
        let u = row(None);
        assert!(u.tag.is_none() && u.name.is_none() && u.version.is_none());
    }

    /// A tag that does not follow the convention is still returned whole — a client that stamps its
    /// own scheme must see its numbers, not be silently dropped from the table.
    #[test]
    fn an_unconventional_tag_survives_intact() {
        let r = row(Some("legacy-prompt-id-7"));
        assert_eq!(r.tag.as_deref(), Some("legacy-prompt-id-7"));
        assert!(r.version.is_none());
    }
}
