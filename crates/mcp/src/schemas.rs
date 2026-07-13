//! `outputSchema` declarations for the read tools — they formalize the `structuredContent` contract
//! that `tool_rendered` returns: list tools wrap their array under `items`, single-entity tools return
//! the object directly. Schemas are deliberately permissive (`additionalProperties: true`, minimal
//! `required`) so the API gaining a field never trips a strict client. Only tools that actually return
//! `structuredContent` (i.e. have a renderer) declare a schema; the rest return plain text.

use serde_json::{json, Value};

/// The `outputSchema` for a tool's structured result, or `None` if the tool returns text only.
pub(crate) fn output_schema(tool: &str) -> Option<Value> {
    let s = match tool {
        "list_projects" => list_of(project()),
        "get_cost_summary" => list_of(cost_row()),
        "get_margin" => margin_resp(),
        "query_events" => list_of(event()),
        "get_event" => event(),
        "list_traces" => list_of(trace_summary()),
        "get_trace" => trace_detail(),
        "list_scores" => list_of(score()),
        "get_limit_status" => limit_status_resp(),
        "list_limits" => list_of(limit_rule()),
        "list_prices" => list_of(price_row()),
        "list_benchmarks" => list_of(benchmark()),
        "get_benchmark" => benchmark(),
        "get_benchmark_runs" => list_of(benchmark_run()),
        "check_benchmark_gate" => gate_resp(),
        "get_usecases" => list_of(usecase_row()),
        "get_forecast" => forecast_resp(),
        "list_jobs" => list_of(job()),
        "get_job" => job(),
        "list_datasets" => list_of(dataset()),
        "get_dataset" => dataset(),
        "list_dataset_items" => list_of(dataset_item()),
        "list_rubrics" => list_of(rubric()),
        "get_rubric" => rubric(),
        "get_collective_leaderboard" => collective_resp(),
        "get_collective_digest" => collective_digest_resp(),
        _ => return None,
    };
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::output_schema;

    #[test]
    fn new_tools_declare_an_output_schema() {
        for t in [
            "check_benchmark_gate",
            "get_usecases",
            "get_forecast",
            "get_collective_digest",
        ] {
            assert!(output_schema(t).is_some(), "{t} should declare an outputSchema");
        }
    }

    #[test]
    fn limit_status_schema_carries_the_rejected_block() {
        let s = output_schema("get_limit_status").expect("schema present");
        let props = &s["properties"];
        assert!(props.get("statuses").is_some());
        assert!(props.get("rejected").is_some(), "rejected block must be schema'd");
    }

    #[test]
    fn gate_schema_enumerates_the_four_verdicts() {
        let s = output_schema("check_benchmark_gate").unwrap();
        let variants = s["properties"]["status"]["enum"].as_array().unwrap();
        assert_eq!(variants.len(), 4);
    }
}

/// `tool_rendered` wraps a top-level array under `items`; mirror that here.
fn list_of(item: Value) -> Value {
    json!({
        "type": "object",
        "required": ["items"],
        "properties": { "items": { "type": "array", "items": item } }
    })
}

fn obj(props: Value) -> Value {
    json!({ "type": "object", "additionalProperties": true, "properties": props })
}

fn project() -> Value {
    obj(json!({
        "id": {"type":"string"}, "name": {"type":"string"}, "enabled": {"type":"boolean"},
        "redaction": {"type":"string"}, "created_at": {"type":"string"}
    }))
}

fn margin_resp() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "dimension": {"type":"string"}, "since": {"type":"string"}, "until": {"type":"string"},
            "total_revenue_usd": {"type":"number"}, "total_cost_usd": {"type":"number"},
            "total_margin_usd": {"type":"number"},
            "rows": { "type":"array", "items": obj(json!({
                "key": {"type":"string"}, "revenue_usd": {"type":"number"},
                "llm_cost_usd": {"type":"number"}, "gross_margin_usd": {"type":"number"},
                "margin_pct": {"type":["number","null"]}, "calls": {"type":"integer"}
            })) }
        }
    })
}

fn cost_row() -> Value {
    obj(json!({
        "project_id": {"type":"string"}, "provider": {"type":"string"}, "model": {"type":"string"},
        "calls": {"type":"integer"}, "input_tokens": {"type":"integer"},
        "output_tokens": {"type":"integer"}, "cost_usd": {"type":"number"}
    }))
}

fn event() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "ts": {"type":"string"},
        "provider": {"type":"string"}, "model": {"type":"string"}, "operation": {"type":"string"},
        "usage": {"type":"object"}, "cost_usd": {"type":["number","null"]},
        "latency_ms": {"type":["integer","null"]}, "status": {"type":"string"}
    }))
}

fn trace_summary() -> Value {
    obj(json!({
        "trace_id": {"type":"string"}, "project_id": {"type":"string"},
        "started_at": {"type":"string"}, "ended_at": {"type":"string"},
        "duration_ms": {"type":"integer"}, "spans": {"type":"integer"},
        "cost_usd": {"type":"number"}, "input_tokens": {"type":"integer"},
        "output_tokens": {"type":"integer"}, "total_tokens": {"type":"integer"},
        "errors": {"type":"integer"}, "status": {"type":"string"},
        "models": {"type":"array"}
    }))
}

fn trace_detail() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "properties": {
            "trace_id": {"type":"string"}, "project_id": {"type":"string"},
            "started_at": {"type":"string"}, "ended_at": {"type":"string"},
            "duration_ms": {"type":"integer"}, "status": {"type":"string"},
            "models": {"type":"array"},
            "totals": obj(json!({
                "spans": {"type":"integer"}, "cost_usd": {"type":"number"},
                "input_tokens": {"type":"integer"}, "output_tokens": {"type":"integer"},
                "total_tokens": {"type":"integer"}, "errors": {"type":"integer"}
            })),
            "spans": {"type":"array"},
            "scores": {"type":"array", "items": score()}
        }
    })
}

fn score() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "event_id": {"type":["string","null"]},
        "rubric": {"type":"string"}, "value": {"type":"number"}, "max": {"type":"number"},
        "pass": {"type":["boolean","null"]}, "scored_by": {"type":"string"},
        "cost_usd": {"type":["number","null"]}, "created_at": {"type":"string"}
    }))
}

fn limit_status_resp() -> Value {
    json!({
        "type": "object",
        "required": ["statuses"],
        "additionalProperties": true,
        "properties": {
            "project_id": {"type":"string"},
            "throttled": {"type":"boolean"},
            "statuses": { "type":"array", "items": obj(json!({
                "metric": {"type":"string"}, "window": {"type":"string"}, "action": {"type":"string"},
                "current": {"type":"number"}, "threshold": {"type":"number"},
                "breached": {"type":"boolean"}, "ratio": {"type":"number"}
            })) },
            // Best-effort, process-local ledger of ingest attempts the caps turned away (429), with the
            // estimated missed cost per (metric, window[, scope]). Absent when nothing's been rejected.
            "rejected": { "type":"array", "items": obj(json!({
                "metric": {"type":"string"}, "window": {"type":"string"}, "scope": {"type":["object","null"]},
                "count": {"type":"integer"}, "est_missed_cost_usd": {"type":"number"},
                "first_ts": {"type":"string"}, "last_ts": {"type":"string"}
            })) }
        }
    })
}

fn gate_resp() -> Value {
    json!({
        "type": "object",
        "required": ["status"],
        "additionalProperties": true,
        "properties": {
            "status": {"type":"string","enum":["pass","regressed","no_baseline","no_runs"]},
            "run_id": {"type":["string","null"]}, "mean": {"type":["number","null"]},
            "baseline": {"type":["number","null"]}, "n": {"type":["integer","null"]}
        }
    })
}

fn usecase_row() -> Value {
    obj(json!({
        "name": {"type":["string","null"]}, "provider": {"type":"string"}, "model": {"type":"string"},
        "calls": {"type":"integer"}, "input_tokens": {"type":"integer"},
        "output_tokens": {"type":"integer"}, "cost_usd": {"type":"number"}
    }))
}

fn forecast_resp() -> Value {
    json!({
        "type": "object",
        "additionalProperties": true,
        "required": ["spend", "budgets", "margins"],
        "properties": {
            "project_id": {"type":"string"}, "generated_at": {"type":"string"},
            "dimension": {"type":"string"}, "horizon_days": {"type":"integer"},
            "lookback_days": {"type":"integer"},
            "spend": obj(json!({
                "cost_trend": {"type":"object"},
                "projected_daily_cost_usd": {"type":"number"},
                "projected_cost_next_7d_usd": {"type":"number"},
                "projected_cost_next_30d_usd": {"type":"number"}
            })),
            "budgets": { "type":"array", "items": obj(json!({
                "rule_id": {"type":"string"}, "metric": {"type":"string"}, "window": {"type":"string"},
                "threshold": {"type":"number"}, "current": {"type":"number"},
                "projected_daily": {"type":"number"}, "eta_days": {"type":["number","null"]}
            })) },
            "margins": { "type":"array", "items": obj(json!({
                "key": {"type":"string"}, "revenue_per_day": {"type":"number"},
                "cost_per_day": {"type":"number"}, "margin_usd": {"type":"number"},
                "currently_profitable": {"type":"boolean"},
                "eta_unprofitable_days": {"type":["number","null"]}
            })) },
            "alerts": { "type":"array", "items": obj(json!({
                "kind": {"type":"string"}, "severity": {"type":"string"}, "subject": {"type":"string"},
                "eta_days": {"type":"number"}, "message": {"type":"string"}
            })) }
        }
    })
}

fn collective_digest_resp() -> Value {
    json!({
        "type": "object",
        "required": ["entries"],
        "additionalProperties": true,
        "properties": {
            "schema_version": {"type":"integer"}, "contributor_id": {"type":"string"},
            "generated_at": {"type":"string"}, "min_cases": {"type":"integer"},
            "entries": { "type":"array", "items": obj(json!({
                "provider": {"type":"string"}, "model": {"type":"string"}, "task_type": {"type":"string"},
                "quality": {"type":"number"}, "pass_rate": {"type":"number"},
                "avg_cost_usd": {"type":"number"}, "p50_latency_ms": {"type":["integer","null"]},
                "n_runs": {"type":"integer"}, "n_cases": {"type":"integer"}
            })) }
        }
    })
}

fn limit_rule() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "metric": {"type":"string"},
        "window": {"type":"string"}, "threshold": {"type":"number"}, "action": {"type":"string"},
        "enabled": {"type":"boolean"}
    }))
}

fn price_row() -> Value {
    obj(json!({
        "provider": {"type":"string"}, "model": {"type":"string"},
        "input_per_mtok": {"type":"number"}, "output_per_mtok": {"type":"number"},
        "cached_input_per_mtok": {"type":["number","null"]}, "effective_date": {"type":"string"},
        "source_url": {"type":["string","null"]}
    }))
}

fn benchmark() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "name": {"type":"string"},
        "rubric": {"type":"string"}, "rubric_id": {"type":["string","null"]},
        "judge_model": {"type":"string"}, "dataset_ref": {"type":["string","null"]},
        "dataset": {"type":"array"}, "baseline_score": {"type":["number","null"]},
        "created_at": {"type":"string"}
    }))
}

fn benchmark_run() -> Value {
    obj(json!({
        "id": {"type":"string"}, "benchmark_id": {"type":"string"}, "started_at": {"type":"string"},
        "finished_at": {"type":["string","null"]}, "n_cases": {"type":"integer"},
        "mean_score": {"type":["number","null"]}, "pass_rate": {"type":["number","null"]},
        "cost_usd": {"type":"number"}, "status": {"type":"string"},
        "p50_latency_ms": {"type":["integer","null"]}, "p95_latency_ms": {"type":["integer","null"]},
        "total_tokens": {"type":["integer","null"]}
    }))
}

fn job() -> Value {
    obj(json!({
        "id": {"type":"string"}, "type": {"type":"string"}, "status": {"type":"string"},
        "attempts": {"type":"integer"}, "max_attempts": {"type":"integer"},
        "progress": {"type":["string","null"]}, "error": {"type":["string","null"]},
        "result": {}, "created_at": {"type":"string"}, "updated_at": {"type":"string"}
    }))
}

fn dataset() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "name": {"type":"string"},
        "version": {"type":"integer"}, "frozen": {"type":"boolean"},
        "source": {"type":["string","null"]}, "created_at": {"type":"string"}
    }))
}

fn dataset_item() -> Value {
    obj(json!({
        "id": {"type":"string"}, "dataset_id": {"type":"string"}, "input": {"type":"string"},
        "output": {"type":["string","null"]}, "expected": {"type":["string","null"]},
        "context": {"type":["string","null"]}, "tags": {"type":"array"},
        "source_event_id": {"type":["string","null"]}
    }))
}

fn rubric() -> Value {
    obj(json!({
        "id": {"type":"string"}, "project_id": {"type":"string"}, "name": {"type":"string"},
        "threshold": {"type":"number"}, "created_at": {"type":"string"},
        "dimensions": { "type":"array", "items": obj(json!({
            "key": {"type":"string"}, "description": {"type":"string"}, "weight": {"type":"number"},
            "anchors": {"type":"array"}, "floor": {"type":["number","null"]}
        })) }
    }))
}

fn collective_resp() -> Value {
    json!({
        "type": "object",
        "required": ["rows"],
        "additionalProperties": true,
        "properties": {
            "contributors": {"type":"integer"}, "n_models": {"type":"integer"},
            "task_type": {"type":["string","null"]},
            "rows": { "type":"array", "items": obj(json!({
                "provider": {"type":"string"}, "model": {"type":"string"}, "task_type": {"type":"string"},
                "quality": {"type":"number"}, "pass_rate": {"type":"number"},
                "avg_cost_usd": {"type":"number"}, "p50_latency_ms": {"type":["integer","null"]},
                "n_contributors": {"type":"integer"}, "n_runs": {"type":"integer"}, "n_cases": {"type":"integer"}
            })) }
        }
    })
}
