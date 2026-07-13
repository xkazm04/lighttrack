//! End-to-end renderer checks against representative API payloads. Run with `--nocapture` to eyeball
//! the actual Markdown the CLI / MCP server now emit.

use lighttrack_render::render;
use serde_json::json;

#[test]
fn cost_summary_table_and_total() {
    let v = json!([
        {"project_id":"qa-demo","provider":"anthropic","model":"claude-haiku-4-5",
         "calls":412,"input_tokens":88120,"output_tokens":20140,"cost_usd":0.83},
        {"project_id":"qa-demo","provider":"openai","model":"gpt-4o-mini",
         "calls":118,"input_tokens":40110,"output_tokens":9000,"cost_usd":0.04}
    ]);
    let md = render("get_cost_summary", &v).expect("renders");
    println!("\n=== get_cost_summary ===\n{md}");
    assert!(md.contains("| Project"));
    assert!(md.contains("88,120")); // thousands separator
    assert!(md.contains("Total: $0.87 across 530 calls"));
    // sorted by spend: the anthropic row (higher cost) precedes the openai row
    assert!(md.find("claude-haiku-4-5").unwrap() < md.find("gpt-4o-mini").unwrap());
}

#[test]
fn limit_status_badges_and_throttle() {
    let v = json!({
        "project_id":"qa-demo","throttled":true,
        "statuses":[
            {"metric":"cost_usd","window":"day","current":4.2,"threshold":5.0,"ratio":0.84,"breached":false,"action":"alert"},
            {"metric":"calls","window":"hour","current":1200.0,"threshold":1000.0,"ratio":1.2,"breached":true,"action":"throttle"}
        ]
    });
    let md = render("get_limit_status", &v).expect("renders");
    println!("\n=== get_limit_status ===\n{md}");
    assert!(md.contains("throttled"));
    assert!(md.contains("⚠️ warning")); // 84% of threshold (>= 0.8 heuristic)
    assert!(md.contains("❌ over")); // breached
    assert!(md.contains("1,200"));
}

#[test]
fn benchmark_runs_leaderboard_with_trend() {
    let v = json!([
        {"started_at":"2026-06-17T12:00:00Z","finished_at":"2026-06-17T12:01:00Z","status":"regressed",
         "mean_score":0.667,"pass_rate":0.667,"cost_usd":0.0123,"p50_latency_ms":8511,"total_tokens":123742,"n_cases":3},
        {"started_at":"2026-06-16T09:00:00Z","finished_at":"2026-06-16T09:02:00Z","status":"passed",
         "mean_score":0.92,"pass_rate":1.0,"cost_usd":0.0119,"p50_latency_ms":7800,"total_tokens":120001,"n_cases":3}
    ]);
    let md = render("get_benchmark_runs", &v).expect("renders");
    println!("\n=== get_benchmark_runs ===\n{md}");
    assert!(md.contains("mean trend"));
    assert!(md.contains("❌ regressed"));
    assert!(md.contains("✅ passed"));
    assert!(md.contains("123,742"));
}

#[test]
fn scores_list_mean_and_sparkline() {
    let v = json!([
        {"created_at":"2026-06-17T10:00:00Z","rubric":"capitals-qa","value":1.0,"max":1.0,"pass":true,"scored_by":"anthropic/claude-haiku","cost_usd":0.00012},
        {"created_at":"2026-06-17T09:00:00Z","rubric":"capitals-qa","value":0.0,"max":1.0,"pass":false,"scored_by":"anthropic/claude-haiku","cost_usd":0.00011}
    ]);
    let md = render("list_scores", &v).expect("renders");
    println!("\n=== list_scores ===\n{md}");
    assert!(md.contains("mean 0.50"));
    assert!(md.contains("✅"));
    assert!(md.contains("❌"));
    assert!(md.contains("$0.00012"));
}

#[test]
fn events_list_and_single_detail() {
    let list = json!([
        {"id":"ev_1abc","ts":"2026-06-17T12:34:56Z","provider":"anthropic","model":"claude-haiku-4-5",
         "usage":{"input":900,"output":120},"cost_usd":0.0008,"latency_ms":740,"status":"success"},
        {"id":"ev_2def","ts":"2026-06-17T12:30:00Z","provider":"openai","model":"gpt-4o-mini",
         "usage":{"input":300,"output":50},"status":"error","error":"timeout"}
    ]);
    let md = render("query_events", &list).expect("renders");
    println!("\n=== query_events ===\n{md}");
    assert!(md.contains("anthropic/claude-haiku-4-5"));
    assert!(md.contains("ev_1abc")); // full id preserved for follow-up
    assert!(md.contains("❌")); // errored event flagged

    let one = json!({
        "id":"ev_1abc","ts":"2026-06-17T12:34:56Z","provider":"anthropic","model":"claude-haiku-4-5",
        "operation":"chat","usage":{"input":900,"output":120},"cost_usd":0.0008,"latency_ms":740,
        "status":"success","input":"What is the capital of France?","output":"Paris."
    });
    let detail = render("get_event", &one).expect("renders");
    println!("\n=== get_event ===\n{detail}");
    assert!(detail.contains("### Event `ev_1abc`"));
    assert!(detail.contains("capital of France"));
    assert!(detail.contains("Paris."));
}

#[test]
fn benchmark_and_rubric_detail() {
    let bench = json!({
        "id":"bm_1","project_id":"qa-demo","name":"capitals-qa","judge_model":"haiku",
        "rubric":"Award 1.0 only for the exact capital city.","rubric_id":null,
        "dataset":[{"input":"France?"},{"input":"Japan?"}],"baseline_score":0.9,
        "created_at":"2026-06-10T08:00:00Z"
    });
    let md = render("get_benchmark", &bench).expect("renders");
    println!("\n=== get_benchmark ===\n{md}");
    assert!(md.contains("### Benchmark `capitals-qa`"));
    assert!(md.contains("**Cases:** 2"));
    assert!(md.contains("**Baseline:** 0.90"));

    let rubric = json!({
        "id":"rb_1","project_id":"qa-demo","name":"answer-quality","threshold":0.7,
        "created_at":"2026-06-10T08:00:00Z",
        "dimensions":[
            {"key":"correctness","description":"Is the answer factually right?","weight":2.0,"floor":0.5},
            {"key":"concision","description":"Is it free of padding?","weight":1.0}
        ]
    });
    let md = render("get_rubric", &rubric).expect("renders");
    println!("\n=== get_rubric ===\n{md}");
    assert!(md.contains("### Rubric `answer-quality`"));
    assert!(md.contains("| correctness"));
    assert!(md.contains("2.00")); // weight
    assert!(md.contains("0.50")); // floor
}

#[test]
fn dataset_list_and_items() {
    let list = json!([
        {"id":"ds_1","project_id":"qa-demo","name":"prod-sample","version":2,"frozen":true,
         "source":"events:recent","created_at":"2026-06-12T09:00:00Z"}
    ]);
    let md = render("list_datasets", &list).expect("renders");
    println!("\n=== list_datasets ===\n{md}");
    assert!(md.contains("prod-sample"));
    assert!(md.contains("🔒"));

    let items = json!([
        {"id":"it_1","dataset_id":"ds_1","input":"What is the capital of France?","expected":"Paris","tags":["geo"]},
        {"id":"it_2","dataset_id":"ds_1","input":"2+2?","expected":"4","tags":[]}
    ]);
    let md = render("list_dataset_items", &items).expect("renders");
    println!("\n=== list_dataset_items ===\n{md}");
    assert!(md.contains("**2 item(s)**"));
    assert!(md.contains("Paris"));
    assert!(md.contains("geo"));
}

#[test]
fn compare_leaderboard_picks_best() {
    let v = json!({
        "n_cases": 3,
        "targets": [
            {"label":"claude/concise","mean":0.93,"pass_rate":1.0,"agreement":0.96,
             "gen_cost_usd":0.0012,"judge_cost_usd":0.0021,"p50_latency_ms":740,"errored":0},
            {"label":"claude/verbose","mean":0.51,"pass_rate":0.33,"agreement":0.80,
             "gen_cost_usd":0.0019,"judge_cost_usd":0.0022,"p50_latency_ms":910,"errored":0},
            {"label":"openai/dead","mean":0.0,"pass_rate":0.0,"agreement":1.0,
             "gen_cost_usd":0.0,"judge_cost_usd":0.0,"p50_latency_ms":null,"errored":3}
        ]
    });
    let md = render("compare", &v).expect("renders");
    println!("\n=== compare ===\n{md}");
    assert!(md.contains("### Comparison — 3 case(s)"));
    assert!(md.contains("| claude/concise"));
    // all-errored target is excluded from "best"; concise (0.93) wins over verbose
    assert!(md.contains("**Best mean: claude/concise (0.93)**"));
}

#[test]
fn margin_report_flags_unprofitable() {
    let v = json!({
        "dimension":"customer","since":"2026-06-01T00:00:00Z","until":"2026-07-01T00:00:00Z",
        "total_revenue_usd":119.0,"total_cost_usd":143.37,"total_margin_usd":-24.37,
        "rows":[
            {"key":"heavy","revenue_usd":99.0,"llm_cost_usd":142.5,"gross_margin_usd":-43.5,"margin_pct":-0.439,"calls":9000},
            {"key":"acme","revenue_usd":20.0,"llm_cost_usd":0.87,"gross_margin_usd":19.13,"margin_pct":0.956,"calls":412}
        ]
    });
    let md = render("get_margin", &v).expect("renders");
    println!("\n=== get_margin ===\n{md}");
    assert!(md.contains("### Margin by customer"));
    assert!(md.contains("🔴 heavy"));
    assert!(md.contains("🟢 acme"));
    assert!(md.contains("-$43.50")); // sign before symbol
    assert!(md.contains("Total: $119.00 revenue − $143.37 cost = -$24.37 margin"));
}

#[test]
fn unknown_kind_falls_back_to_none() {
    assert!(render("no_such_tool", &json!({"x":1})).is_none());
    // empty collections render a friendly note rather than an empty table
    assert_eq!(render("list_projects", &json!([])).unwrap(), "_No projects._");
}
