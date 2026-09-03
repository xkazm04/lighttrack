//! Projects, API keys and limit rules: the `EventsCore` floor plus the three lifecycle surfaces
//! layered on it (`ProjectAdmin`, `KeyAdmin`, `LimitLifecycle`), each independently declarable.

use chrono::Utc;

use lighttrack_core::{
    new_id, ApiKey, LimitAction, LimitMetric, LimitRule, LimitScope, LimitWindow, Project,
    Redaction, Threshold,
};

use crate::Scope;
use crate::{Result, Store};

pub(super) fn projects_keys_limits(store: &dyn Store, pid: &str) -> Result<()> {
    let proj = Project {
        id: pid.into(),
        name: "conf".into(),
        enabled: true,
        redaction: Redaction::None,
        // Non-default on purpose: pins that the consent flag round-trips on every backend (a backend
        // that drops it silently opts a project out of — or worse, into — collective contribution).
        collective_opt_in: true,
        require_trusted_judge: false,
        archived_at: None,
        created_at: Utc::now(),
    };
    store.create_project(&proj)?;
    let got = store.get_project(pid)?.expect("get_project Some");
    assert!(got.collective_opt_in, "collective_opt_in round-trips");
    assert!(store.get_project(&new_id())?.is_none(), "get_project None");
    assert!(
        store.list_projects()?.iter().any(|p| p.id == pid),
        "list_projects contains ours"
    );

    // Archiving is the documented `DELETE /v1/projects/:id`, so `archived_at` has to survive a write
    // on every backend — a backend that drops it turns "archived" back into "live" on the next read.
    let archived_id = new_id();
    let archived_at = Utc::now();
    store.create_project(&Project {
        id: archived_id.clone(),
        name: "conf-archived".into(),
        enabled: false,
        redaction: Redaction::None,
        collective_opt_in: false,
        require_trusted_judge: false,
        archived_at: Some(archived_at),
        created_at: Utc::now(),
    })?;
    let back = store
        .get_project(&archived_id)?
        .expect("archived project readable");
    assert_eq!(back.archived_at, Some(archived_at), "archived_at persists");
    assert!(!back.enabled, "an archived project is not enabled");
    assert!(
        got.archived_at.is_none(),
        "a live project has no archived_at"
    );

    let prefix: String = new_id().chars().take(8).collect();
    // Non-default on purpose (a *narrower* set than `default_scopes`): a backend that drops the
    // column reads the permissive back-compat default back, which is exactly the silent widening
    // this assertion exists to catch.
    let scopes = vec![lighttrack_core::Scope::Ingest];
    let expires_at = Utc::now() + chrono::Duration::hours(1);
    let key = ApiKey {
        id: new_id(),
        project_id: pid.into(),
        name: "default".into(),
        prefix: prefix.clone(),
        key_hash: "salt:hash".into(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked: false,
        scopes: scopes.clone(),
        expires_at: Some(expires_at),
    };
    store.create_api_key(&key)?;
    let found = store
        .find_api_key_by_prefix(&prefix)?
        .expect("find_api_key_by_prefix Some");
    assert_eq!(found.project_id, pid);
    assert_eq!(found.scopes, scopes, "the key's scopes round-trip narrow");
    assert_eq!(found.expires_at, Some(expires_at), "expires_at round-trips");
    assert!(
        store.find_api_key_by_prefix("zzzzzzzz")?.is_none(),
        "unknown prefix None"
    );
    store.touch_api_key(&key.id, Utc::now())?;

    let rule = LimitRule {
        id: new_id(),
        project_id: pid.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Hour,
        threshold: Threshold::Fixed(0.0015),
        action: LimitAction::Alert,
        enabled: true,
        warn_at: None,
        scope: None,
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    };
    store.create_limit_rule(&rule)?;
    let enabled = store.list_limit_rules(pid, true)?;
    assert_eq!(enabled.len(), 1);
    assert_eq!(enabled[0].metric, LimitMetric::CostUsd);
    let u = store.usage_since(pid, Utc::now() - chrono::Duration::hours(1))?;
    assert!(
        rule.evaluate(u.cost_usd).breached,
        "0.003 cost should breach 0.0015 threshold"
    );
    Ok(())
}

/// `Surface::ProjectAdmin` — replacing a project's mutable fields in place.
///
/// This is how a **redaction** policy is changed, so a backend that refuses it cannot tighten what
/// it stores; `PUT /v1/projects/:id` answering 501 on the backend production runs is exactly the
/// undecided gap the manifest exists to surface.
pub(super) fn project_admin(store: &dyn Store) -> Result<()> {
    let mut proj = Project {
        id: new_id(),
        name: "admin-before".into(),
        enabled: true,
        redaction: Redaction::None,
        collective_opt_in: false,
        require_trusted_judge: false,
        archived_at: None,
        created_at: Utc::now(),
    };
    store.create_project(&proj)?;

    proj.name = "admin-after".into();
    proj.enabled = false;
    proj.redaction = Redaction::Drop;
    proj.collective_opt_in = true;
    assert!(store.update_project(&proj)?, "update matches the row");

    let got = store.get_project(&proj.id)?.expect("project after update");
    assert_eq!(got.name, "admin-after", "name update persists");
    assert!(!got.enabled, "enabled update persists");
    assert_eq!(
        got.redaction,
        Redaction::Drop,
        "redaction update persists — the field this surface exists for"
    );
    assert!(got.collective_opt_in, "consent update persists");
    assert_eq!(
        got.created_at, proj.created_at,
        "created_at is immutable across an update"
    );
    assert!(
        !store.update_project(&Project {
            id: new_id(),
            ..proj.clone()
        })?,
        "updating an unknown id returns false (the API maps that to 404), never a silent insert"
    );
    Ok(())
}

/// `Surface::KeyAdmin` — listing a project's keys and revoking one. Both were write-only /
/// enforced-but-unsettable before the parity wave.
pub(super) fn key_admin(store: &dyn Store, pid: &str) -> Result<()> {
    let prefix: String = new_id().chars().take(8).collect();
    let key = ApiKey {
        id: new_id(),
        project_id: pid.into(),
        name: "key-admin".into(),
        prefix: prefix.clone(),
        key_hash: "salt:hash".into(),
        created_at: Utc::now(),
        last_used_at: None,
        revoked: false,
        scopes: lighttrack_core::default_scopes(),
        expires_at: None,
    };
    store.create_api_key(&key)?;
    store.touch_api_key(&key.id, Utc::now())?;

    // Rotation's grace window is a stamped expiry on the predecessor, not a background task, so the
    // stamp itself has to be a real, readable write on every backend.
    let grace_end = Utc::now() + chrono::Duration::seconds(30);
    assert!(
        store.set_api_key_expiry(&key.id, Some(grace_end))?,
        "stamping an expiry reports a row changed"
    );
    assert_eq!(
        store
            .find_api_key_by_prefix(&prefix)?
            .expect("still present")
            .expires_at,
        Some(grace_end),
        "the stamped expiry persisted"
    );
    assert!(
        store.set_api_key_expiry(&key.id, None)?,
        "an expiry can be cleared again"
    );
    assert!(
        store
            .find_api_key_by_prefix(&prefix)?
            .expect("still present")
            .expires_at
            .is_none(),
        "clearing an expiry persisted"
    );
    assert!(
        !store.set_api_key_expiry(&new_id(), Some(grace_end))?,
        "expiring an unknown id returns false"
    );

    // Key lifecycle: the project's keys are listable (with the last-use we just stamped), and a key
    // can be revoked — the two fields that were write-only / enforced-but-unsettable before this wave.
    let keys = store.list_api_keys(pid)?;
    assert!(
        keys.iter().any(|k| k.id == key.id),
        "list_api_keys contains our key"
    );
    assert!(
        keys.iter()
            .find(|k| k.id == key.id)
            .unwrap()
            .last_used_at
            .is_some(),
        "last_used_at is readable back"
    );
    assert!(
        store.set_api_key_revoked(&key.id, true)?,
        "revoke reports a row changed"
    );
    assert!(
        store
            .find_api_key_by_prefix(&prefix)?
            .expect("still present")
            .revoked,
        "revoked persisted"
    );
    assert!(
        !store.set_api_key_revoked(&new_id(), true)?,
        "revoking an unknown id returns false"
    );
    Ok(())
}

/// `Surface::LimitLifecycle` — reading, updating and deleting a rule after creation.
///
/// `warn_at` and `scope` are the fields that must persist faithfully: a backend that drops the scope
/// turns "cap gpt-4o at $X" into an unscoped project-wide cap — a semantic inversion, not an absence.
pub(super) fn limit_lifecycle(store: &dyn Store, pid: &str) -> Result<()> {
    // Scoped-rule lifecycle round-trip: `warn_at` + `scope` must persist faithfully — a backend
    // that drops them turns "cap gpt-4o at $X" into an unscoped project-wide cap (a semantic
    // inversion, not an absence) — and get/update/delete must work wherever create does.
    let scoped = LimitRule {
        id: new_id(),
        project_id: pid.into(),
        metric: LimitMetric::CostUsd,
        window: LimitWindow::Day,
        threshold: Threshold::Fixed(50.0),
        action: LimitAction::Throttle,
        enabled: true,
        warn_at: Some(0.8),
        scope: Some(LimitScope::Model("conf-scoped-model".into())),
        escalation: None,
        escalated_until: None,
        origin: None,
        expires_at: None,
    };
    store.create_limit_rule(&scoped)?;
    let got = store
        .get_limit_rule(Scope::Operator, &scoped.id)?
        .expect("get_limit_rule finds the rule");
    assert_eq!(got.warn_at, Some(0.8), "warn_at round-trips");
    assert_eq!(
        got.scope,
        Some(LimitScope::Model("conf-scoped-model".into())),
        "scope round-trips (dropping it silently widens a scoped cap to the whole project)"
    );
    let mut updated = got.clone();
    updated.threshold = Threshold::Fixed(75.0);
    updated.scope = Some(LimitScope::Provider("conf-prov".into()));
    assert!(
        store.update_limit_rule(Scope::Operator, &updated)?,
        "update matches the row"
    );
    let after = store
        .get_limit_rule(Scope::Operator, &scoped.id)?
        .expect("rule still present after update");
    assert_eq!(
        after.threshold,
        Threshold::Fixed(75.0),
        "threshold update persists"
    );
    assert_eq!(
        after.scope,
        Some(LimitScope::Provider("conf-prov".into())),
        "scope update persists"
    );
    // The key/customer dimensions must survive the same round-trip — a backend that dropped an
    // unknown `scope_kind` would silently promote a $5 staging cap to a project-wide one.
    for s in [
        LimitScope::ApiKey("conf-key-1".into()),
        LimitScope::Customer("conf-cus".into()),
    ] {
        let mut r = after.clone();
        r.scope = Some(s.clone());
        assert!(store.update_limit_rule(Scope::Operator, &r)?);
        assert_eq!(
            store
                .get_limit_rule(Scope::Operator, &scoped.id)?
                .and_then(|g| g.scope),
            Some(s.clone()),
            "{} scope round-trips",
            s.kind_str()
        );
    }
    assert!(
        store.delete_limit_rule(Scope::Operator, &scoped.id)?,
        "delete removes the rule"
    );
    assert!(
        store.get_limit_rule(Scope::Operator, &scoped.id)?.is_none(),
        "deleted rule is gone"
    );
    assert!(
        !store.delete_limit_rule(Scope::Operator, &new_id())?,
        "deleting an unknown id returns false"
    );
    Ok(())
}
