//! `Surface::Alerts` and `Surface::AlertRouting`: the product's own audit trail.
//!
//! The load-bearing assertion is [`alerts`]'s first block. Two `Alerter`s over one store — which in
//! production means two Cloud Run replicas evaluating the same breach in the same second — must
//! produce **one** admitted alert. That is a property of the store's dedup step, not of either
//! process's memory, so it is asserted here rather than in the API: an in-process cooldown map
//! cannot express it at all, which is exactly why alerts used to double up per replica.

use std::time::Duration;

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{new_id, AlertKind, ChannelKind, Delivery, Severity};

use super::fixtures::{sample_alert, sample_alert_channel};
use crate::{AlertAdmission, AlertFilter, Result, Store};

pub(super) fn alerts(store: &dyn Store, pid: &str) -> Result<()> {
    let cooldown = Duration::from_secs(3600);
    let key = format!("{pid}:cost_usd:hour");

    // Two independent alerters, one store: the first admits, the second is told to stay quiet.
    let first = sample_alert(pid, AlertKind::LimitBreach, &key);
    assert_eq!(
        store.insert_alert_dedup(&first, cooldown)?,
        AlertAdmission::Admitted,
        "the first alert for a key must be admitted"
    );
    let second = sample_alert(pid, AlertKind::LimitBreach, &key);
    match store.insert_alert_dedup(&second, cooldown)? {
        AlertAdmission::Suppressed { fired_at } => assert!(
            (fired_at - first.fired_at).num_seconds().abs() <= 1,
            "the suppression must name the alert that is already live"
        ),
        AlertAdmission::Admitted => panic!(
            "a second alert on the same dedup key inside the cooldown must be suppressed — this \
             is what makes a multi-replica deployment alert once rather than once per replica"
        ),
    }
    assert!(
        store.get_alert(&second.id)?.is_none(),
        "a suppressed alert must not leave a row: the ledger counts incidents, not attempts"
    );

    // A different key is a different condition, and a zero cooldown means no deduplication at all.
    let other = sample_alert(pid, AlertKind::ScoreDrop, &format!("{key}:other"));
    assert_eq!(
        store.insert_alert_dedup(&other, cooldown)?,
        AlertAdmission::Admitted
    );
    let uncooled = sample_alert(pid, AlertKind::ErrorSpike, &key);
    assert_eq!(
        store.insert_alert_dedup(&uncooled, Duration::ZERO)?,
        AlertAdmission::Admitted,
        "a zero cooldown is a request for no deduplication, not for total suppression"
    );

    // Round-trip: the row carries what fired, at what severity, with its payload intact.
    let got = store.get_alert(&first.id)?.expect("get_alert Some");
    assert_eq!(got.kind, AlertKind::LimitBreach);
    assert_eq!(got.severity, Severity::Critical, "breaches are critical");
    assert_eq!(got.dedup_key, key);
    assert_eq!(got.payload, json!({ "text": "conformance", "n": 1 }));
    assert!(got.delivered.is_empty() && got.acked_at.is_none());

    // Delivery outcomes accumulate — including the failures, which are the ones an operator needs.
    assert!(store.mark_delivery(
        &first.id,
        &Delivery {
            channel_id: "env:webhook".into(),
            ok: false,
            status: Some("503".into()),
            at: Utc::now(),
        },
    )?);
    assert!(store.mark_delivery(
        &first.id,
        &Delivery {
            channel_id: "env:ntfy".into(),
            ok: true,
            status: Some("200".into()),
            at: Utc::now(),
        },
    )?);
    let got = store.get_alert(&first.id)?.expect("get after deliveries");
    assert_eq!(
        got.delivered.len(),
        2,
        "a second outcome must not clobber the first"
    );
    assert!(
        !got.fully_delivered(),
        "one failed channel means the alert was not fully delivered"
    );
    assert!(
        !store.mark_delivery(&new_id(), &got.delivered[0])?,
        "marking delivery on an alert that does not exist says so"
    );

    // Ack and resolution are separate facts: someone saw it, and something came of it.
    assert!(store.ack_alert(&first.id, "ops@example.test", Utc::now())?);
    assert!(store.attach_alert_resolution(&first.id, &json!({ "ok": true, "cost_usd": 0.02 }))?);
    let got = store.get_alert(&first.id)?.expect("get after ack");
    assert_eq!(got.acked_by.as_deref(), Some("ops@example.test"));
    assert!(got.acked_at.is_some());
    assert_eq!(
        got.resolution,
        Some(json!({ "ok": true, "cost_usd": 0.02 }))
    );
    assert!(!store.ack_alert(&new_id(), "nobody", Utc::now())?);
    assert!(!store.attach_alert_resolution(&new_id(), &json!({}))?);

    // Listing is project-scoped and narrows on kind / acked, newest first.
    let mine = AlertFilter {
        project: Some(pid.to_string()),
        ..Default::default()
    };
    let page = store.list_alerts(&mine)?;
    assert_eq!(page.len(), 3, "three admitted alerts for this project");
    assert!(
        page.windows(2).all(|w| w[0].fired_at >= w[1].fired_at),
        "the ledger reads newest-first"
    );
    assert!(
        store
            .list_alerts(&AlertFilter {
                project: Some(new_id()),
                ..Default::default()
            })?
            .is_empty(),
        "another project must not see this project's alerts"
    );
    let by_kind = store.list_alerts(&AlertFilter {
        project: Some(pid.to_string()),
        kind: Some(AlertKind::ScoreDrop),
        ..Default::default()
    })?;
    assert_eq!(by_kind.len(), 1);
    assert_eq!(by_kind[0].id, other.id);
    let open = store.list_alerts(&AlertFilter {
        project: Some(pid.to_string()),
        acked: Some(false),
        ..Default::default()
    })?;
    assert_eq!(open.len(), 2, "the acked one drops out of the open list");
    assert!(open.iter().all(|a| a.acked_at.is_none()));
    assert_eq!(
        store
            .list_alerts(&AlertFilter {
                project: Some(pid.to_string()),
                acked: Some(true),
                ..Default::default()
            })?
            .len(),
        1
    );
    // `since` is a lower bound on fired_at, so a window that starts after everything is empty.
    assert!(store
        .list_alerts(&AlertFilter {
            project: Some(pid.to_string()),
            since: Some(Utc::now() + chrono::Duration::hours(1)),
            ..Default::default()
        })?
        .is_empty());

    // Paging: one row at a time, walking the cursor, must visit each alert exactly once.
    let mut seen = Vec::new();
    let mut cursor = None;
    for _ in 0..5 {
        let page = store.list_alerts(&AlertFilter {
            project: Some(pid.to_string()),
            limit: 1,
            cursor: cursor.clone(),
            ..Default::default()
        })?;
        let Some(a) = page.into_iter().next() else {
            break;
        };
        cursor = Some(crate::codec::encode_event_cursor(
            &crate::codec::fmt_ts(a.fired_at),
            &a.id,
        ));
        seen.push(a.id);
    }
    seen.sort();
    seen.dedup();
    assert_eq!(seen.len(), 3, "keyset paging must not skip or repeat a row");
    Ok(())
}

pub(super) fn alert_routing(store: &dyn Store, pid: &str) -> Result<()> {
    let global = sample_alert_channel(None);
    let mine = sample_alert_channel(Some(pid));
    store.create_alert_channel(&global)?;
    store.create_alert_channel(&mine)?;

    let got = store
        .get_alert_channel(&mine.id)?
        .expect("get_alert_channel Some");
    assert_eq!(got.kind, ChannelKind::Webhook);
    assert_eq!(got.target, "https://receiver.invalid/hook");
    assert_eq!(got.min_severity, Severity::Warning);
    assert_eq!(got.kinds, vec![AlertKind::LimitBreach]);
    assert_eq!(
        got.secret_hash.as_deref(),
        Some("0".repeat(64).as_str()),
        "the signing key must survive the round-trip, or nothing can be signed"
    );
    assert!(got.enabled);

    // The two listings are disjoint by design: `Some(p)` is the project's own, `None` the globals.
    let project_only = store.list_alert_channels(Some(pid))?;
    assert!(project_only.iter().any(|c| c.id == mine.id));
    assert!(
        !project_only.iter().any(|c| c.id == global.id),
        "a project listing must not silently include the global channels"
    );
    assert!(store
        .list_alert_channels(None)?
        .iter()
        .any(|c| c.id == global.id));

    // `channels_for` is the union — which is what makes an existing env-only deployment unchanged.
    let routed = store.channels_for(Some(pid))?;
    assert!(routed.iter().any(|c| c.id == mine.id));
    assert!(routed.iter().any(|c| c.id == global.id));
    let unrelated = store.channels_for(Some(&new_id()))?;
    assert!(unrelated.iter().any(|c| c.id == global.id));
    assert!(
        !unrelated.iter().any(|c| c.id == mine.id),
        "another project gets the globals and its own, never this project's"
    );

    // The severity floor and kind filter travel with the row, so routing decisions are the same on
    // every replica that reads it.
    assert!(got.accepts(AlertKind::LimitBreach, Severity::Critical));
    assert!(!got.accepts(AlertKind::ScoreDrop, Severity::Critical));

    assert!(store.delete_alert_channel(&mine.id)?);
    assert!(store.get_alert_channel(&mine.id)?.is_none());
    assert!(
        !store.delete_alert_channel(&mine.id)?,
        "a second delete finds nothing"
    );
    assert!(store.delete_alert_channel(&global.id)?);
    Ok(())
}
