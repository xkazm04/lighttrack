//! The cross-tenant collision property, for **every** project-bearing entity type (M17).
//!
//! One property, asserted the same way everywhere: create a row under `pid` and a twin under
//! `other`, then read by id under three scopes.
//!
//! * [`Scope::Project`]`(pid)` sees exactly its own row.
//! * [`Scope::Project`]`(third)` — a project that owns neither — sees **nothing**. Not a refusal,
//!   not an error: `None`/empty, indistinguishable from an id that was never written. That is the
//!   whole point. A distinct answer here (the `403` the handlers used to produce after reading the
//!   row) is an existence oracle over caller-chosen ids, which is what D13 removed for traces and
//!   M17 removes everywhere else.
//! * [`Scope::Operator`] sees both.
//!
//! The trace collision case that used to be the only one of these lives on in [`super::traces`];
//! this section generalises it to the other fifteen entity types, and is where a sixteenth belongs.
//!
//! Guarded per surface rather than run inside one, because tenancy cuts across all of them: a
//! backend that does not declare `Surface::Schedules` simply skips the schedules block (its refusal
//! is already asserted by [`super::refusals`]).

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{
    new_id, AlertKind, Benchmark, BenchmarkRun, Dataset, DatasetItem, LabelSubject, Rubric, Score,
    ScoreKind,
};

use super::devices::sample_device;
use super::fixtures::{
    sample_alert, sample_alert_channel, sample_event, sample_policy, sample_rule,
};
use super::labels::sample_label;
use super::prompts::{sample_prompt, sample_version};
use super::relay::sample_task;
use super::schedules::sample_schedule;
use crate::{Capabilities, Result, Scope, Store, Surface};

/// The three scopes every case is read under: the owner, an unrelated tenant, and the operator.
struct Tenants {
    mine: String,
    theirs: String,
    third: String,
}

/// Assert the collision property for one entity type.
///
/// `read` answers "how many of these two ids does this scope see" — a count rather than a bool, so
/// a listing method and a point read can be checked by the same three assertions.
fn collide(
    what: &str,
    t: &Tenants,
    read: impl Fn(Scope<'_>) -> Result<(bool, bool)>,
) -> Result<()> {
    let (mine, theirs) = read(Scope::Project(&t.mine))?;
    assert!(mine, "{what}: a project must see its own row");
    assert!(
        !theirs,
        "{what}: a project must not see another project's row"
    );

    let (mine, theirs) = read(Scope::Project(&t.third))?;
    assert!(
        !mine && !theirs,
        "{what}: an unrelated project must see NEITHER row — and must not be able to tell a \
         foreign id from a missing one"
    );

    let (mine, theirs) = read(Scope::Operator)?;
    assert!(
        mine && theirs,
        "{what}: the operator scope must still see every project's rows"
    );
    Ok(())
}

pub(super) fn tenancy(store: &dyn Store, caps: &Capabilities) -> Result<()> {
    let t = Tenants {
        mine: new_id(),
        theirs: new_id(),
        third: new_id(),
    };

    if caps.has(Surface::EventsCore) {
        events(store, &t)?;
        benchmarks(store, &t)?;
        datasets(store, &t)?;
        rubrics(store, &t)?;
        jobs(store, &t)?;
        limit_rules(store, &t)?;
    }
    if caps.has(Surface::MarginPolicies) {
        margin_policies(store, &t)?;
    }
    if caps.has(Surface::Schedules) {
        schedules(store, &t)?;
    }
    if caps.has(Surface::Prompts) {
        prompts(store, &t)?;
    }
    if caps.has(Surface::Relay) {
        relay_tasks(store, &t)?;
    }
    if caps.has(Surface::Devices) {
        devices(store, &t)?;
    }
    if caps.has(Surface::Alerts) {
        alerts(store, &t)?;
    }
    if caps.has(Surface::AlertRouting) {
        alert_channels(store, &t)?;
    }
    if caps.has(Surface::Labels) {
        labels(store, &t)?;
    }
    Ok(())
}

fn events(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mine = sample_event(&t.mine, "m-mine", 1, 1, 0.01);
    let theirs = sample_event(&t.theirs, "m-theirs", 1, 1, 0.02);
    store.insert_event(&mine)?;
    store.insert_event(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_event", t, |s| {
        Ok((
            store.get_event(s, &a)?.is_some(),
            store.get_event(s, &b)?.is_some(),
        ))
    })?;
    // `scored_event_ids` is asked about both ids at once, so a leak shows up as an extra element
    // rather than as the wrong row — the shape that would make the online scorer skip another
    // project's unscored events, or re-judge its own.
    for (pid, ev) in [(&t.mine, &a), (&t.theirs, &b)] {
        store.insert_score(&Score {
            id: new_id(),
            project_id: pid.clone(),
            event_id: Some(ev.clone()),
            rubric: "correctness".into(),
            rubric_id: None,
            kind: ScoreKind::Rubric,
            value: 1.0,
            max: 1.0,
            pass: Some(true),
            reasoning: None,
            detail: None,
            run_id: None,
            case_index: None,
            scored_by: "conformance".into(),
            cost_usd: None,
            created_at: Utc::now(),
        })?;
    }
    collide("scored_event_ids", t, |s| {
        let ids = store.scored_event_ids(s, &[a.clone(), b.clone()])?;
        Ok((ids.contains(&a), ids.contains(&b)))
    })
}

fn benchmarks(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| Benchmark {
        id: new_id(),
        project_id: pid.into(),
        name: "tenancy".into(),
        rubric: "is it right".into(),
        judge_model: "haiku".into(),
        target: json!([{ "provider": "anthropic", "model": "haiku" }]),
        dataset_ref: None,
        rubric_id: None,
        dataset: Vec::new(),
        baseline_score: None,
        created_at: Utc::now(),
    };
    let (mine, theirs) = (mk(&t.mine), mk(&t.theirs));
    store.create_benchmark(&mine)?;
    store.create_benchmark(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_benchmark", t, |s| {
        Ok((
            store.get_benchmark(s, &a)?.is_some(),
            store.get_benchmark(s, &b)?.is_some(),
        ))
    })?;
    // `benchmark_runs` carries no project of its own: the filter has to ride the parent.
    for bid in [&a, &b] {
        store.create_benchmark_run(&BenchmarkRun {
            id: new_id(),
            benchmark_id: bid.clone(),
            started_at: Utc::now(),
            finished_at: Some(Utc::now()),
            n_cases: 1,
            mean_score: Some(1.0),
            pass_rate: Some(1.0),
            cost_usd: 0.0,
            status: "passed".into(),
            p50_latency_ms: None,
            p95_latency_ms: None,
            total_tokens: None,
            report: json!({}),
        })?;
    }
    collide("list_benchmark_runs", t, |s| {
        Ok((
            !store.list_benchmark_runs(s, &a)?.is_empty(),
            !store.list_benchmark_runs(s, &b)?.is_empty(),
        ))
    })
}

fn datasets(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| Dataset {
        id: new_id(),
        project_id: pid.into(),
        name: "tenancy".into(),
        version: 1,
        frozen: false,
        source: None,
        created_at: Utc::now(),
    };
    let (mine, theirs) = (mk(&t.mine), mk(&t.theirs));
    store.create_dataset(&mine)?;
    store.create_dataset(&theirs)?;
    for d in [&mine, &theirs] {
        store.create_dataset_item(&DatasetItem {
            id: new_id(),
            dataset_id: d.id.clone(),
            input: "2+2".into(),
            output: None,
            expected: Some("4".into()),
            context: None,
            tags: Vec::new(),
            source_event_id: None,
            anonymization: json!({}),
        })?;
    }
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_dataset", t, |s| {
        Ok((
            store.get_dataset(s, &a)?.is_some(),
            store.get_dataset(s, &b)?.is_some(),
        ))
    })?;
    collide("list_dataset_items", t, |s| {
        Ok((
            !store.list_dataset_items(s, &a)?.is_empty(),
            !store.list_dataset_items(s, &b)?.is_empty(),
        ))
    })?;
    collide("list_datasets", t, |s| {
        let ids: Vec<String> = store.list_datasets(s)?.into_iter().map(|d| d.id).collect();
        Ok((ids.contains(&a), ids.contains(&b)))
    })
}

fn rubrics(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| Rubric {
        id: new_id(),
        project_id: pid.into(),
        name: "tenancy".into(),
        dimensions: Vec::new(),
        threshold: 0.7,
        version: 1,
        supersedes: None,
        created_at: Utc::now(),
    };
    let (mine, theirs) = (mk(&t.mine), mk(&t.theirs));
    store.create_rubric(&mine)?;
    store.create_rubric(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_rubric", t, |s| {
        Ok((
            store.get_rubric(s, &a)?.is_some(),
            store.get_rubric(s, &b)?.is_some(),
        ))
    })
}

/// The queue's row had no tenant at all before M17, so `GET /v1/jobs` handed every project's
/// payloads to whoever could reach the route. `project_id` is nullable: a job with none is an
/// operator/legacy row that only [`Scope::Operator`] reads back.
fn jobs(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: Option<&str>| {
        let mut j = super::jobs::new_job();
        j.project_id = pid.map(str::to_string);
        j
    };
    let (mine, theirs, operator) = (mk(Some(&t.mine)), mk(Some(&t.theirs)), mk(None));
    store.create_job(&mine)?;
    store.create_job(&theirs)?;
    store.create_job(&operator)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_job", t, |s| {
        Ok((
            store.get_job(s, &a)?.is_some(),
            store.get_job(s, &b)?.is_some(),
        ))
    })?;
    collide("list_jobs", t, |s| {
        let ids: Vec<String> = store
            .list_jobs(s, None, 1000)?
            .into_iter()
            .map(|j| j.id)
            .collect();
        Ok((ids.contains(&a), ids.contains(&b)))
    })?;
    // The project-less job: the operator's, and nobody else's.
    assert!(
        store.get_job(Scope::Operator, &operator.id)?.is_some(),
        "an operator job must be readable by the operator"
    );
    assert!(
        store
            .get_job(Scope::Project(&t.mine), &operator.id)?
            .is_none(),
        "a project scope must not read a project-less (operator/legacy) job"
    );
    Ok(())
}

fn limit_rules(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| {
        let mut r = sample_rule();
        r.project_id = pid.to_string();
        r
    };
    let (mine, theirs) = (mk(&t.mine), mk(&t.theirs));
    store.create_limit_rule(&mine)?;
    store.create_limit_rule(&theirs)?;
    if !store.capabilities().has(Surface::LimitLifecycle) {
        return Ok(());
    }
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_limit_rule", t, |s| {
        Ok((
            store.get_limit_rule(s, &a)?.is_some(),
            store.get_limit_rule(s, &b)?.is_some(),
        ))
    })?;
    // A delete is a read with consequences: the wrong scope must change nothing.
    assert!(
        !store.delete_limit_rule(Scope::Project(&t.third), &a)?,
        "a foreign scope must not delete another project's limit rule"
    );
    assert!(
        store.get_limit_rule(Scope::Operator, &a)?.is_some(),
        "and the rule must still be there afterwards"
    );
    Ok(())
}

fn margin_policies(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| {
        let mut p = sample_policy();
        p.project_id = pid.to_string();
        p
    };
    let (mine, theirs) = (mk(&t.mine), mk(&t.theirs));
    store.create_margin_policy(&mine)?;
    store.create_margin_policy(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_margin_policy", t, |s| {
        Ok((
            store.get_margin_policy(s, &a)?.is_some(),
            store.get_margin_policy(s, &b)?.is_some(),
        ))
    })
}

fn schedules(store: &dyn Store, t: &Tenants) -> Result<()> {
    let (mine, theirs) = (sample_schedule(&t.mine), sample_schedule(&t.theirs));
    store.create_schedule(&mine)?;
    store.create_schedule(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_schedule", t, |s| {
        Ok((
            store.get_schedule(s, &a)?.is_some(),
            store.get_schedule(s, &b)?.is_some(),
        ))
    })?;
    assert!(
        !store.delete_schedule(Scope::Project(&t.third), &a)?,
        "a foreign scope must not delete another project's schedule"
    );
    Ok(())
}

fn prompts(store: &dyn Store, t: &Tenants) -> Result<()> {
    let (mine, theirs) = (sample_prompt(&t.mine), sample_prompt(&t.theirs));
    store.create_prompt(&mine)?;
    store.create_prompt(&theirs)?;
    store.create_prompt_version(&sample_version(&mine.id, 1))?;
    store.create_prompt_version(&sample_version(&theirs.id, 1))?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_prompt_by_id", t, |s| {
        Ok((
            store.get_prompt_by_id(s, &a)?.is_some(),
            store.get_prompt_by_id(s, &b)?.is_some(),
        ))
    })?;
    // `prompt_versions` carries no project of its own: the filter rides the parent prompt.
    collide("list_prompt_versions", t, |s| {
        Ok((
            !store.list_prompt_versions(s, &a)?.is_empty(),
            !store.list_prompt_versions(s, &b)?.is_empty(),
        ))
    })?;
    collide("get_prompt_version", t, |s| {
        Ok((
            store.get_prompt_version(s, &a, 1)?.is_some(),
            store.get_prompt_version(s, &b, 1)?.is_some(),
        ))
    })
}

fn relay_tasks(store: &dyn Store, t: &Tenants) -> Result<()> {
    let (mine, theirs) = (sample_task(&t.mine, 3), sample_task(&t.theirs, 3));
    store.create_relay_task(&mine)?;
    store.create_relay_task(&theirs)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_relay_task", t, |s| {
        Ok((
            store.get_relay_task(s, &a)?.is_some(),
            store.get_relay_task(s, &b)?.is_some(),
        ))
    })?;
    assert!(
        store
            .cancel_relay_task(Scope::Project(&t.third), &a)?
            .is_none(),
        "a foreign scope must not cancel another project's relay task"
    );
    Ok(())
}

/// Devices are the one entity where a project scope legitimately sees more than its own rows: an
/// operator-wide device (`project_id IS NULL`) serves every project's tasks, so it stays visible
/// exactly as `list_devices` shows it. What a tenant must never see is **another tenant's** device.
fn devices(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mk = |pid: &str| {
        let mut d = sample_device("tenancy", &["conf/*"]);
        d.project_id = Some(pid.to_string());
        d
    };
    let (mine, theirs, shared) = (
        mk(&t.mine),
        mk(&t.theirs),
        sample_device("shared", &["c/*"]),
    );
    store.create_device(&mine)?;
    store.create_device(&theirs)?;
    store.create_device(&shared)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_device", t, |s| {
        Ok((
            store.get_device(s, &a)?.is_some(),
            store.get_device(s, &b)?.is_some(),
        ))
    })?;
    assert!(
        store
            .get_device(Scope::Project(&t.third), &shared.id)?
            .is_some(),
        "an operator-wide device is part of every project's fleet, and `get_device` must agree \
         with `list_devices` about that"
    );
    assert!(
        !store.revoke_device(Scope::Project(&t.third), &shared.id)?,
        "but no single tenant may retire a device that serves them all"
    );
    Ok(())
}

fn alerts(store: &dyn Store, t: &Tenants) -> Result<()> {
    let (mine, theirs) = (
        sample_alert(&t.mine, AlertKind::LimitBreach, &new_id()),
        sample_alert(&t.theirs, AlertKind::LimitBreach, &new_id()),
    );
    store.insert_alert_dedup(&mine, std::time::Duration::ZERO)?;
    store.insert_alert_dedup(&theirs, std::time::Duration::ZERO)?;
    let (a, b) = (mine.id.clone(), theirs.id.clone());
    collide("get_alert", t, |s| {
        Ok((
            store.get_alert(s, &a)?.is_some(),
            store.get_alert(s, &b)?.is_some(),
        ))
    })?;
    assert!(
        !store.ack_alert(Scope::Project(&t.third), &a, "nobody", Utc::now())?,
        "a foreign scope must not acknowledge another project's alert"
    );
    Ok(())
}

/// Alert channels answer to a scope rather than to a project id: a tenant reads its own, the
/// operator reads the project-less ones it configured. The two sets are disjoint by design (their
/// union is `channels_for`), so the operator arm here is "sees the global one", not "sees both".
fn alert_channels(store: &dyn Store, t: &Tenants) -> Result<()> {
    let mine = sample_alert_channel(Some(&t.mine));
    let theirs = sample_alert_channel(Some(&t.theirs));
    let global = sample_alert_channel(None);
    store.create_alert_channel(&mine)?;
    store.create_alert_channel(&theirs)?;
    store.create_alert_channel(&global)?;
    assert!(
        store
            .get_alert_channel(Scope::Project(&t.mine), &mine.id)?
            .is_some(),
        "a project reads its own channel"
    );
    for id in [&theirs.id, &global.id] {
        assert!(
            store
                .get_alert_channel(Scope::Project(&t.mine), id)?
                .is_none(),
            "and neither another project's nor the operator's"
        );
    }
    assert!(
        store
            .get_alert_channel(Scope::Operator, &global.id)?
            .is_some(),
        "the operator reads the project-less channels"
    );
    assert!(
        !store.delete_alert_channel(Scope::Project(&t.third), &mine.id)?,
        "a foreign scope must not delete another project's channel"
    );
    Ok(())
}

fn labels(store: &dyn Store, t: &Tenants) -> Result<()> {
    if !store.capabilities().has(Surface::EventsCore) {
        return Ok(());
    }
    let mk = |pid: &str| Dataset {
        id: new_id(),
        project_id: pid.into(),
        name: "labelled".into(),
        version: 1,
        frozen: false,
        source: None,
        created_at: Utc::now(),
    };
    let (dm, dt) = (mk(&t.mine), mk(&t.theirs));
    store.create_dataset(&dm)?;
    store.create_dataset(&dt)?;
    for (d, owner) in [(&dm, &t.mine), (&dt, &t.theirs)] {
        let item = DatasetItem {
            id: new_id(),
            dataset_id: d.id.clone(),
            input: "q".into(),
            output: None,
            expected: None,
            context: None,
            tags: Vec::new(),
            source_event_id: None,
            anonymization: json!({}),
        };
        store.create_dataset_item(&item)?;
        store.insert_label(&sample_label(
            owner,
            LabelSubject::DatasetItem(item.id.clone()),
            1.0,
        ))?;
    }
    let (a, b) = (dm.id.clone(), dt.id.clone());
    collide("labels_for_dataset", t, |s| {
        Ok((
            !store.labels_for_dataset(s, &a)?.is_empty(),
            !store.labels_for_dataset(s, &b)?.is_empty(),
        ))
    })
}
