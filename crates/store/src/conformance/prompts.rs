//! `Surface::Prompts`: the versioned prompt registry.
//!
//! The two properties worth pinning are the ones a runtime fetch depends on: a **label pointer**
//! survives the round-trip (a dropped `labels` map silently serves the wrong version to production),
//! and versions come back **newest first** with their content intact.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{
    new_id, CanaryPolicy, Prompt, PromptVersion, REASON_CANARY_REGRESSED, REASON_PROMOTE,
};

use crate::Scope;
use crate::{Result, Store};

pub(super) fn sample_prompt(project: &str) -> Prompt {
    Prompt {
        id: new_id(),
        project_id: project.into(),
        name: format!("conf-{}", new_id()),
        benchmark_id: None,
        labels: Default::default(),
        canary: None,
        label_history: Vec::new(),
        created_at: Utc::now(),
        updated_at: Utc::now(),
    }
}

pub(super) fn sample_version(prompt_id: &str, version: u32) -> PromptVersion {
    PromptVersion {
        id: new_id(),
        prompt_id: prompt_id.into(),
        version,
        content: format!("you are a helpful assistant (v{version})"),
        config: json!({ "model": "claude-haiku-4-5" }),
        note: Some(format!("cut v{version}")),
        created_at: Utc::now(),
    }
}

pub(super) fn prompts(store: &dyn Store) -> Result<()> {
    let project = new_id();
    let mut p = sample_prompt(&project);
    store.create_prompt(&p)?;

    let by_name = store
        .get_prompt(&project, &p.name)?
        .expect("get_prompt by registry name — the runtime fetch path");
    assert_eq!(by_name.id, p.id);
    assert_eq!(
        store
            .get_prompt_by_id(Scope::Operator, &p.id)?
            .expect("get_prompt_by_id")
            .name,
        p.name
    );
    assert!(
        store.get_prompt(&project, "no-such-prompt")?.is_none(),
        "an unknown name is None, not an error"
    );
    assert!(
        store.list_prompts(&project)?.iter().any(|x| x.id == p.id),
        "list_prompts contains ours"
    );
    assert!(
        store.list_prompts(&new_id())?.is_empty(),
        "prompts are scoped to their project"
    );

    store.create_prompt_version(&sample_version(&p.id, 1))?;
    let v2 = sample_version(&p.id, 2);
    store.create_prompt_version(&v2)?;

    let versions = store.list_prompt_versions(Scope::Operator, &p.id)?;
    assert_eq!(versions.len(), 2, "both versions are listed");
    assert_eq!(
        versions[0].version, 2,
        "newest version first (a reversed order serves a stale prompt to every caller)"
    );
    let got = store
        .get_prompt_version(Scope::Operator, &p.id, 2)?
        .expect("get_prompt_version by number");
    assert_eq!(got.content, v2.content, "version content round-trips");
    assert_eq!(got.config, v2.config, "version config round-trips");
    assert!(
        store
            .get_prompt_version(Scope::Operator, &p.id, 99)?
            .is_none(),
        "an unknown version number is None"
    );

    // The label pointer is the whole point of the registry: `label=production` has to resolve to the
    // version someone promoted, so a backend that drops the map serves whatever it feels like.
    p.labels.insert("production".into(), 2);
    p.updated_at = Utc::now();
    store.update_prompt(&p)?;
    assert_eq!(
        store
            .get_prompt(&project, &p.name)?
            .expect("prompt after update")
            .labels
            .get("production"),
        Some(&2),
        "label pointer round-trips"
    );

    // …and a prompt created WITH a policy keeps it. Separate from the update path on purpose: an
    // INSERT that silently omits the two new columns leaves every registry entry uncanaried and the
    // sweep finds nothing to do, which looks exactly like a healthy deployment.
    let mut fresh = sample_prompt(&project);
    fresh.canary = Some(CanaryPolicy::default());
    store.create_prompt(&fresh)?;
    assert_eq!(
        store
            .get_prompt(&project, &fresh.name)?
            .expect("the freshly created prompt")
            .canary,
        fresh.canary,
        "a canary policy set at creation survives the INSERT"
    );

    // M23: the canary policy and the label ledger ride on the same row. A backend that drops either
    // one turns an auto-reverting canary into a silent no-op — the policy stops being read, or the
    // revert has no previous version to fall back to.
    p.canary = Some(CanaryPolicy {
        label: "canary".into(),
        production_label: "production".into(),
        min_n: 7,
        window_secs: 3_600,
        max_drop: 0.11,
        auto_revert: true,
    });
    p.set_label("production", 1, REASON_PROMOTE);
    p.set_label("production", 2, REASON_CANARY_REGRESSED);
    store.update_prompt(&p)?;

    let back = store
        .get_prompt(&project, &p.name)?
        .expect("prompt after the canary update");
    assert_eq!(
        back.canary.as_ref().map(|c| c.min_n),
        Some(7),
        "the canary policy round-trips"
    );
    assert_eq!(back.canary, p.canary, "…every field of it");
    assert_eq!(back.label_history.len(), 2, "the label ledger round-trips");
    assert_eq!(
        back.label_history[1].reason.as_deref(),
        Some(REASON_CANARY_REGRESSED),
        "why a label moved is the half a pointer cannot carry"
    );
    assert_eq!(
        back.previous_version("production"),
        Some(1),
        "an auto-revert reads its rollback target out of the stored ledger"
    );

    // Clearing the policy must actually clear it: a canary that cannot be turned off keeps acting.
    let mut cleared = back;
    cleared.canary = None;
    store.update_prompt(&cleared)?;
    assert!(
        store
            .get_prompt(&project, &p.name)?
            .expect("prompt after clearing")
            .canary
            .is_none(),
        "a removed canary policy stays removed"
    );
    Ok(())
}
