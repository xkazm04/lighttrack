//! `Surface::Prompts`: the versioned prompt registry.
//!
//! The two properties worth pinning are the ones a runtime fetch depends on: a **label pointer**
//! survives the round-trip (a dropped `labels` map silently serves the wrong version to production),
//! and versions come back **newest first** with their content intact.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{new_id, Prompt, PromptVersion};

use crate::{Result, Store};

pub(super) fn sample_prompt(project: &str) -> Prompt {
    Prompt {
        id: new_id(),
        project_id: project.into(),
        name: format!("conf-{}", new_id()),
        benchmark_id: None,
        labels: Default::default(),
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
            .get_prompt_by_id(&p.id)?
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

    let versions = store.list_prompt_versions(&p.id)?;
    assert_eq!(versions.len(), 2, "both versions are listed");
    assert_eq!(
        versions[0].version, 2,
        "newest version first (a reversed order serves a stale prompt to every caller)"
    );
    let got = store
        .get_prompt_version(&p.id, 2)?
        .expect("get_prompt_version by number");
    assert_eq!(got.content, v2.content, "version content round-trips");
    assert_eq!(got.config, v2.config, "version config round-trips");
    assert!(
        store.get_prompt_version(&p.id, 99)?.is_none(),
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
    Ok(())
}
