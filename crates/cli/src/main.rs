//! `lt` — LightTrack operator CLI. A thin HTTP client over the API.
//!
//! Global options (also read from env):
//!   --base  LIGHTTRACK_URL  (default http://127.0.0.1:8787)
//!   --key   LIGHTTRACK_KEY  (admin key for management, or a project key for scoped reads)
//!
//! Examples:
//!   lt projects create --name billing-demo
//!   lt keys create --project <id> --name app-key
//!   lt limits set --project <id> --metric cost_usd --window day --threshold 5 --action alert
//!   lt limits status --project <id>
//!   lt costs --project <id>
//!   lt events --project <id> --limit 20

use std::io::IsTerminal;

use anyhow::Result;
use clap::{Parser, Subcommand};
use reqwest::Method;
use serde_json::{json, Value};

#[derive(Parser)]
#[command(name = "lt", about = "LightTrack operator CLI")]
struct Cli {
    #[arg(long, env = "LIGHTTRACK_URL", default_value = "http://127.0.0.1:8787")]
    base: String,
    #[arg(long, env = "LIGHTTRACK_KEY")]
    key: Option<String>,
    /// Print raw JSON instead of the rendered table view (also implied when stdout is piped).
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Manage projects.
    Projects {
        #[command(subcommand)]
        action: ProjectsCmd,
    },
    /// Manage API keys.
    Keys {
        #[command(subcommand)]
        action: KeysCmd,
    },
    /// Manage and inspect limit rules.
    Limits {
        #[command(subcommand)]
        action: LimitsCmd,
    },
    /// Cost/usage rollup.
    Costs {
        #[arg(long)]
        project: Option<String>,
    },
    /// Recent events.
    Events {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Recent agent traces (events grouped by trace_id): end-to-end cost, latency, tokens, spans.
    Traces {
        #[arg(long)]
        project: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// One trace by id: rolled-up totals, the span tree, and any scores within it.
    Trace {
        id: String,
    },
    /// Profit margin: revenue − LLM cost by customer or product (default window: last 30 days).
    Margin {
        #[arg(long, default_value = "customer")]
        by: String,
        #[arg(long)]
        project: Option<String>,
        /// RFC3339 window start (default 30d ago).
        #[arg(long)]
        since: Option<String>,
        /// RFC3339 window end (default now).
        #[arg(long)]
        until: Option<String>,
    },
    /// Collective Model Intelligence: the shared real-world model leaderboard (network effect).
    Collective {
        #[command(subcommand)]
        action: CollectiveCmd,
    },
}

#[derive(Subcommand)]
enum CollectiveCmd {
    /// Show the merged public leaderboard (quality × cost × latency across contributors).
    Leaderboard {
        /// Filter to one task-type bucket (e.g. qa, summarization, coding).
        #[arg(long = "task-type")]
        task_type: Option<String>,
        #[arg(long)]
        provider: Option<String>,
    },
    /// Preview this instance's privacy-safe digest — what `contribute` would publish (admin key).
    Digest {
        /// k-anonymity floor: only publish (model, task) buckets with at least this many cases.
        #[arg(long = "min-cases", default_value_t = 5)]
        min_cases: u32,
    },
    /// Build this instance's digest and contribute it to a leaderboard hub (opt-in).
    Contribute {
        /// Base URL of the hub that accepts contributions (its API).
        #[arg(long)]
        hub: String,
        #[arg(long = "min-cases", default_value_t = 5)]
        min_cases: u32,
        /// Optional bearer key for the hub (if it runs in enforced auth mode).
        #[arg(long = "hub-key")]
        hub_key: Option<String>,
    },
}

#[derive(Subcommand)]
enum ProjectsCmd {
    Create {
        #[arg(long)]
        name: String,
    },
    List,
}

#[derive(Subcommand)]
enum KeysCmd {
    Create {
        #[arg(long)]
        project: String,
        #[arg(long, default_value = "default")]
        name: String,
    },
}

#[derive(Subcommand)]
enum LimitsCmd {
    Set {
        #[arg(long)]
        project: String,
        #[arg(long)]
        metric: String,
        #[arg(long)]
        window: String,
        #[arg(long)]
        threshold: f64,
        #[arg(long, default_value = "alert")]
        action: String,
    },
    List {
        #[arg(long)]
        project: String,
    },
    Status {
        #[arg(long)]
        project: String,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Projects { action } => match action {
            ProjectsCmd::Create { name } => {
                call(&cli, Method::POST, "/v1/projects", Some(json!({ "name": name })), "")
            }
            ProjectsCmd::List => call(&cli, Method::GET, "/v1/projects", None, "list_projects"),
        },
        Cmd::Keys { action } => match action {
            KeysCmd::Create { project, name } => call(
                &cli,
                Method::POST,
                &format!("/v1/projects/{project}/keys"),
                Some(json!({ "name": name })),
                "",
            ),
        },
        Cmd::Limits { action } => match action {
            LimitsCmd::Set {
                project,
                metric,
                window,
                threshold,
                action,
            } => call(
                &cli,
                Method::POST,
                &format!("/v1/projects/{project}/limits"),
                Some(json!({
                    "metric": metric, "window": window,
                    "threshold": threshold, "action": action
                })),
                "",
            ),
            LimitsCmd::List { project } => call(
                &cli,
                Method::GET,
                &format!("/v1/projects/{project}/limits"),
                None,
                "list_limits",
            ),
            LimitsCmd::Status { project } => call(
                &cli,
                Method::GET,
                &format!("/v1/limits/status?project={project}"),
                None,
                "get_limit_status",
            ),
        },
        Cmd::Costs { project } => call(
            &cli,
            Method::GET,
            &path_with_project("/v1/costs", project),
            None,
            "get_cost_summary",
        ),
        Cmd::Events { project, limit } => {
            let mut p = format!("/v1/events?limit={limit}");
            if let Some(proj) = project {
                p.push_str(&format!("&project={proj}"));
            }
            call(&cli, Method::GET, &p, None, "query_events")
        }
        Cmd::Traces { project, limit } => {
            let mut p = format!("/v1/traces?limit={limit}");
            if let Some(proj) = project {
                p.push_str(&format!("&project={proj}"));
            }
            call(&cli, Method::GET, &p, None, "list_traces")
        }
        Cmd::Trace { id } => {
            call(&cli, Method::GET, &format!("/v1/traces/{id}"), None, "get_trace")
        }
        Cmd::Margin { by, project, since, until } => {
            let mut p = format!("/v1/margin?by={by}");
            for (k, v) in [("project", project), ("since", since), ("until", until)] {
                if let Some(val) = v {
                    p.push_str(&format!("&{k}={val}"));
                }
            }
            call(&cli, Method::GET, &p, None, "get_margin")
        }
        Cmd::Collective { action } => match action {
            CollectiveCmd::Leaderboard { task_type, provider } => {
                let mut p = "/v1/collective/leaderboard".to_string();
                let mut sep = '?';
                for (k, v) in [("task_type", task_type), ("provider", provider)] {
                    if let Some(val) = v {
                        p.push_str(&format!("{sep}{k}={val}"));
                        sep = '&';
                    }
                }
                call(&cli, Method::GET, &p, None, "get_collective_leaderboard")
            }
            CollectiveCmd::Digest { min_cases } => call(
                &cli,
                Method::GET,
                &format!("/v1/collective/digest?min_cases={min_cases}"),
                None,
                "get_collective_digest",
            ),
            CollectiveCmd::Contribute { hub, min_cases, hub_key } => {
                contribute(&cli, hub, *min_cases, hub_key.as_deref())
            }
        },
    }
}

/// Build this instance's digest (from its own API) and POST it to a hub's ingest endpoint. Two hops:
/// `GET /v1/collective/digest` here → `POST /v1/collective/ingest` there. Keeps cross-instance push in
/// the CLI rather than baking outbound calls into the API.
fn contribute(cli: &Cli, hub: &str, min_cases: u32, hub_key: Option<&str>) -> Result<()> {
    let client = reqwest::blocking::Client::new();

    let mut req = client.get(format!("{}/v1/collective/digest?min_cases={min_cases}", cli.base));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    if !resp.status().is_success() {
        eprintln!("build digest failed: HTTP {} — {}", resp.status().as_u16(), resp.text()?);
        std::process::exit(1);
    }
    let digest: Value = resp.json()?;
    let n = digest.get("entries").and_then(Value::as_array).map(Vec::len).unwrap_or(0);
    if n == 0 {
        println!("nothing to contribute: no (model, task) bucket reached the k≥{min_cases} floor yet.");
        return Ok(());
    }

    let hub_base = hub.trim_end_matches('/');
    let mut req = client.post(format!("{hub_base}/v1/collective/ingest")).json(&digest);
    if let Some(k) = hub_key {
        req = req.bearer_auth(k);
    }
    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    if status.is_success() {
        println!("contributed {n} bucket(s) to {hub_base}: {text}");
    } else {
        eprintln!("contribute failed: HTTP {} — {text}", status.as_u16());
        std::process::exit(1);
    }
    Ok(())
}

fn path_with_project(base: &str, project: &Option<String>) -> String {
    match project {
        Some(p) => format!("{base}?project={p}"),
        None => base.to_string(),
    }
}

/// Issue one request and print the response, then exit non-zero on HTTP error. On a TTY (and unless
/// `--json`) a successful response is shown as a rendered Markdown table for `kind`; piped or `--json`
/// output stays raw JSON so scripts keep parsing it.
fn call(cli: &Cli, method: Method, path: &str, body: Option<Value>, kind: &str) -> Result<()> {
    let client = reqwest::blocking::Client::new();
    let mut req = client.request(method, format!("{}{}", cli.base, path));
    if let Some(k) = &cli.key {
        req = req.bearer_auth(k);
    }
    if let Some(b) = body {
        req = req.json(&b);
    }

    let resp = req.send()?;
    let status = resp.status();
    let text = resp.text()?;
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => {
            let rendered = (!cli.json && status.is_success() && std::io::stdout().is_terminal())
                .then(|| lighttrack_render::render(kind, &v))
                .flatten();
            match rendered {
                Some(md) => println!("{md}"),
                None => println!("{}", serde_json::to_string_pretty(&v)?),
            }
        }
        Err(_) => println!("{text}"),
    }
    if !status.is_success() {
        eprintln!("HTTP {}", status.as_u16());
        std::process::exit(1);
    }
    Ok(())
}
