//! `lt` — LightTrack operator CLI. A thin HTTP client over the API.
//!
//! Global options (also read from env):
//!   --base  LIGHTTRACK_URL  (default http://127.0.0.1:8787)
//!   --key   LIGHTTRACK_KEY  (admin key for management, or a project key for scoped reads)
//!
//! Examples:
//!   lt health   |   lt capabilities   |   lt storage status   |   lt ingest status
//!   lt projects create --name billing-demo --id billing-demo
//!   lt projects update <id> --disable   |   lt projects redaction <id> --since 30d
//!   lt keys create --project <id> --name app-key --scope ingest --expires 2027-01-01T00:00:00Z
//!   lt keys list --project <id>   |   lt keys rotate --project <id> <key-id> --grace-secs 3600
//!   lt limits set --project <id> --metric cost_usd --window day --threshold 5 --action alert
//!   lt limits status --project <id>   |   lt limits usage --project <id> --by customer
//!   lt margin-policies list --project <id>
//!   lt alerts list --open --since 7d   |   lt alerts ack <alert-id> --by oncall
//!   lt alerts channels set --project <id> --type webhook --target <url> --signed  (secret ONCE)
//!   lt rubrics create --project <id> --file rubric.json
//!   lt labels add --subject event:<id> --value 0.9 --labeler me   |   lt labels list
//!   lt judges trust anthropic/claude-haiku-4-5 --project <id> --rubric-id <id>
//!   lt schedules create --project <id> --type bench_run --every 6h --payload '{"benchmark_id":"b1"}'
//!   lt schedules list   |   lt schedules set <id> --disabled   |   lt jobs list --status running
//!   lt relay devices add --name studio-laptop --capability 'xprice/*'   (key shown ONCE)
//!   lt relay devices list   |   lt relay devices revoke <device-id>
//!   lt relay tasks list --status queued   |   lt relay actions --limit 5000
//!   lt prompts list --project <id>   |   lt prompts quality --project <id> --rubric-id <r>
//!   lt costs --project <id>   |   lt costs prompts --project <id>
//!   lt rollup --by customer,day   |   lt forecast --project <id> --horizon 30
//!   lt margin --by product   |   lt margin trend --days 7   |   lt margin customer <id>
//!   lt prices unpriced --project <id>   |   lt prices history openai gpt-5.5
//!   lt events --project <id> --limit 20
//!
//! Layout: `cli` (args, one module per domain), `http` (API client + output), `query` (query-string
//! building), then one module per domain — `platform` (the deployment's own doors), `projects`
//! (projects and keys), `limits`, `alerts` with `alert_channels`, `rubrics`, `schedules` (recurring
//! work and the job queue), `usage` (costs, events, traces, rollup, forecast), `margin` (the profit
//! reports), `revenue` (recognized revenue and the margin policies over it), `collective`, and
//! `relay` (the device fleet and its queue).

mod alert_channels;
mod alerts;
mod cli;
mod collective;
mod contract;
mod datasets;
mod http;
mod labels;
mod limits;
mod margin;
mod platform;
mod prices;
mod projects;
mod prompts;
mod query;
mod relay;
mod revenue;
mod rubrics;
mod schedules;
mod usage;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Health => platform::health(&cli),
        Cmd::Openapi => platform::openapi(&cli),
        Cmd::Capabilities => platform::capabilities(&cli),
        Cmd::Ingest { action } => platform::ingest(&cli, action),
        Cmd::Storage { action } => platform::storage(&cli, action),
        Cmd::Projects { action } => projects::run(&cli, action),
        Cmd::Keys { action } => projects::run_keys(&cli, action),
        Cmd::Limits { action } => limits::run(&cli, action),
        Cmd::MarginPolicies { action } => revenue::run_policies(&cli, action),
        Cmd::Alerts { action } => alerts::run(&cli, action),
        Cmd::Rubrics { action } => rubrics::run(&cli, action),
        Cmd::Datasets { action } => datasets::run(&cli, action),
        Cmd::Labels { action } => labels::run(&cli, action),
        Cmd::Judges { action } => labels::run_judges(&cli, action),
        Cmd::Costs { args, action } => usage::costs(&cli, &args.project, action),
        Cmd::Rollup {
            project,
            by,
            since,
            until,
            time,
            filter,
        } => usage::rollup(&cli, project, by, since, until, time, filter),
        Cmd::Forecast {
            project,
            by,
            horizon,
            lookback,
        } => usage::forecast(&cli, project, by, *horizon, *lookback),
        Cmd::Prices { action } => prices::run(&cli, action),
        Cmd::Prompts { action } => prompts::run(&cli, action),
        Cmd::Events {
            project,
            limit,
            cursor,
        } => usage::events(&cli, project, *limit, cursor),
        Cmd::Traces {
            project,
            limit,
            cursor,
        } => usage::traces(&cli, project, *limit, cursor),
        Cmd::Trace { id } => usage::trace(&cli, id),
        Cmd::Margin { args, action } => margin::run(&cli, args, action),
        Cmd::Revenue { action } => revenue::run(&cli, action),
        Cmd::Schedules { action } => schedules::run(&cli, action),
        Cmd::Jobs { action } => schedules::run_jobs(&cli, action),
        Cmd::Reprice {
            currency,
            project,
            rate,
            apply,
        } => usage::reprice(&cli, currency, project, rate, *apply),
        Cmd::Collective { action } => collective::run(&cli, action),
        Cmd::Relay { action } => relay::run(&cli, action),
    }
}
