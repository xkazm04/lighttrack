//! The definition catalog every backend must carry: prices, benchmarks, datasets, rubrics.

use chrono::Utc;
use serde_json::json;

use lighttrack_core::{
    new_id, Benchmark, BenchmarkCase, BenchmarkRun, Dataset, DatasetItem, DimensionCheck,
    DimensionKind, ModelPriceRow, Rubric, RubricDimension,
};

use crate::Scope;
use crate::{Result, Store};

pub(super) fn prices(store: &dyn Store) -> Result<()> {
    let model = format!("conf-{}", new_id());
    let mut row = ModelPriceRow {
        provider: "conformance".into(),
        model: model.clone(),
        input_per_mtok: 1.0,
        output_per_mtok: 2.0,
        cached_input_per_mtok: Some(0.1),
        effective_from: Utc::now(),
        verified_at: None,
        note: None,
        source_url: None,
    };
    store.upsert_price(&row)?;
    let found = store
        .list_prices()?
        .into_iter()
        .find(|p| p.provider == "conformance" && p.model == model)
        .expect("upserted price present");
    assert!((found.input_per_mtok - 1.0).abs() < 1e-9);

    // Conflict path: a second upsert on the same (provider, model, effective_from) corrects that
    // one point on the timeline in place. Adding a *later* row instead appends — see
    // `conformance::pricing`, which is where the dated-book semantics are pinned.
    row.output_per_mtok = 9.0;
    store.upsert_price(&row)?;
    let updated = store
        .list_prices()?
        .into_iter()
        .find(|p| p.model == model)
        .expect("price still present");
    assert!(
        (updated.output_per_mtok - 9.0).abs() < 1e-9,
        "upsert ON CONFLICT updates"
    );
    Ok(())
}

pub(super) fn benchmarks(store: &dyn Store, pid: &str) -> Result<()> {
    let target = json!([{ "provider": "anthropic", "model": "haiku" }]);
    let b = Benchmark {
        id: new_id(),
        project_id: pid.into(),
        name: "bench".into(),
        rubric: "is it right".into(),
        judge_model: "haiku".into(),
        target: target.clone(),
        dataset_ref: None,
        rubric_id: None,
        dataset: vec![BenchmarkCase {
            input: "2+2".into(),
            expected: Some("4".into()),
            output: Some("4".into()),
        }],
        baseline_score: Some(0.8),
        created_at: Utc::now(),
    };
    store.create_benchmark(&b)?;
    let got = store
        .get_benchmark(Scope::Operator, &b.id)?
        .expect("get_benchmark Some");
    assert_eq!(got.name, "bench");
    assert_eq!(got.dataset.len(), 1);
    assert_eq!(got.target, target, "benchmark target round-trip");
    assert!(store.list_benchmarks(pid)?.iter().any(|x| x.id == b.id));

    let run = BenchmarkRun {
        id: new_id(),
        benchmark_id: b.id.clone(),
        started_at: Utc::now(),
        finished_at: Some(Utc::now()),
        n_cases: 1,
        mean_score: Some(1.0),
        pass_rate: Some(1.0),
        cost_usd: 0.005,
        status: "passed".into(),
        p50_latency_ms: Some(100),
        p95_latency_ms: Some(200),
        total_tokens: Some(123),
        report: json!({ "note": "ok" }),
    };
    store.create_benchmark_run(&run)?;
    let runs = store.list_benchmark_runs(Scope::Operator, &b.id)?;
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].n_cases, 1);
    assert_eq!(runs[0].total_tokens, Some(123));
    assert_eq!(
        runs[0].report,
        json!({ "note": "ok" }),
        "run report round-trip"
    );
    Ok(())
}

pub(super) fn datasets(store: &dyn Store, pid: &str) -> Result<()> {
    let d = Dataset {
        id: new_id(),
        project_id: pid.into(),
        name: "ds".into(),
        version: 1,
        frozen: false,
        source: Some("conf".into()),
        created_at: Utc::now(),
        parent_id: None,
    };
    store.create_dataset(&d)?;
    assert!(store.get_dataset(Scope::Operator, &d.id)?.is_some());
    assert!(store
        .list_datasets(pid.into())?
        .iter()
        .any(|x| x.id == d.id));

    let item = DatasetItem {
        id: new_id(),
        dataset_id: d.id.clone(),
        input: "2+2".into(),
        output: None,
        expected: Some("4".into()),
        context: None,
        tags: vec!["t".into()],
        source_event_id: None,
        anonymization: json!({ "method": "regex", "redactions": 0 }),
        input_hash: None,
    };
    store.create_dataset_item(&item)?;
    let items = store.list_dataset_items(Scope::Operator, &d.id)?;
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].expected, Some("4".to_string()));
    assert_eq!(
        items[0].anonymization,
        json!({ "method": "regex", "redactions": 0 })
    );

    store.set_dataset_frozen(Scope::Operator, &d.id, true)?;
    assert!(
        store
            .get_dataset(Scope::Operator, &d.id)?
            .expect("dataset")
            .frozen,
        "frozen after set"
    );
    Ok(())
}

pub(super) fn rubrics(store: &dyn Store, pid: &str) -> Result<()> {
    let r = Rubric {
        id: new_id(),
        project_id: pid.into(),
        name: "rub".into(),
        version: 1,
        supersedes: None,
        dimensions: vec![
            RubricDimension {
                key: "correct".into(),
                description: "is it right".into(),
                weight: 1.0,
                anchors: vec!["1.0 = yes".into()],
                floor: Some(0.5),
                kind: DimensionKind::Llm,
                check: DimensionCheck::default(),
            },
            // A deterministic dimension: its kind + config must survive the round-trip on every
            // backend, or a mixed rubric would silently degrade to all-LLM after a reload.
            RubricDimension {
                key: "answer".into(),
                description: "exact answer".into(),
                weight: 2.0,
                anchors: vec![],
                floor: Some(1.0),
                kind: DimensionKind::Numeric,
                check: DimensionCheck {
                    expect: Some("42".into()),
                    tolerance: Some(0.1),
                    ..Default::default()
                },
            },
        ],
        threshold: 0.7,
        created_at: Utc::now(),
    };
    store.create_rubric(&r)?;
    let got = store
        .get_rubric(Scope::Operator, &r.id)?
        .expect("get_rubric Some");
    assert_eq!(got.dimensions.len(), 2);
    assert_eq!(got.dimensions[0].key, "correct");
    assert_eq!(got.dimensions[0].floor, Some(0.5));
    assert_eq!(got.dimensions[0].kind, DimensionKind::Llm);
    assert_eq!(got.dimensions[1].kind, DimensionKind::Numeric);
    assert_eq!(got.dimensions[1].check.expect.as_deref(), Some("42"));
    assert_eq!(got.dimensions[1].check.tolerance, Some(0.1));
    assert!(store.list_rubrics(pid)?.iter().any(|x| x.id == r.id));
    Ok(())
}
