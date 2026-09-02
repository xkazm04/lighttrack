//! `lt` — LightTrack operator CLI. A thin HTTP client over the API.
//!
//! Global options (also read from env):
//!   --base  LIGHTTRACK_URL  (default http://127.0.0.1:8787)
//!   --key   LIGHTTRACK_KEY  (admin key for management, or a project key for scoped reads)
//!
//! Examples:
//!   lt projects create --name billing-demo --id billing-demo
//!   lt keys create --project <id> --name app-key --scope ingest --expires 2027-01-01T00:00:00Z
//!   lt keys list --project <id>   |   lt keys rotate --project <id> <key-id> --grace-secs 3600
//!   lt limits set --project <id> --metric cost_usd --window day --threshold 5 --action alert
//!   lt limits status --project <id>
//!   lt alerts list --open --since 7d   |   lt alerts ack <alert-id> --by oncall
//!   lt rubrics create --project <id> --file rubric.json
//!   lt labels add --subject event:<id> --value 0.9 --labeler me   |   lt labels list
//!   lt judges trust anthropic/claude-haiku-4-5 --project <id> --rubric-id <id>
//!   lt schedules create --project <id> --type bench_run --every 6h --payload '{"benchmark_id":"b1"}'
//!   lt schedules list   |   lt schedules set <id> --disabled   |   lt jobs list --status running
//!   lt relay devices add --name studio-laptop --capability 'xprice/*'   (key shown ONCE)
//!   lt relay devices list   |   lt relay devices revoke <device-id>
//!   lt prompts list --project <id>   |   lt prompts quality --project <id> --rubric-id <r>
//!   lt costs --project <id>
//!   lt prices unpriced --project <id>   |   lt prices history openai gpt-5.5
//!   lt events --project <id> --limit 20
//!
//! Layout: `cli` (args), `http` (API client + output), then one module per domain — `projects`
//! (projects + keys), `limits`, `alerts`, `rubrics`, `schedules` (recurring work + the job queue),
//! `usage` (costs / events / traces / margin), `collective`, `relay` (the device fleet).

mod alerts;
mod cli;
mod collective;
mod http;
mod labels;
mod limits;
mod prices;
mod projects;
mod prompts;
mod relay;
mod rubrics;
mod schedules;
mod usage;

use anyhow::Result;
use clap::Parser;

use cli::{Cli, Cmd};

fn main() -> Result<()> {
    let cli = Cli::parse();
    match &cli.cmd {
        Cmd::Projects { action } => projects::run(&cli, action),
        Cmd::Keys { action } => projects::run_keys(&cli, action),
        Cmd::Limits { action } => limits::run(&cli, action),
        Cmd::Alerts { action } => alerts::run(&cli, action),
        Cmd::Rubrics { action } => rubrics::run(&cli, action),
        Cmd::Labels { action } => labels::run(&cli, action),
        Cmd::Judges { action } => labels::run_judges(&cli, action),
        Cmd::Costs { project } => usage::costs(&cli, project),
        Cmd::Prices { action } => prices::run(&cli, action),
        Cmd::Prompts { action } => prompts::run(&cli, action),
        Cmd::Events { project, limit } => usage::events(&cli, project, *limit),
        Cmd::Traces { project, limit } => usage::traces(&cli, project, *limit),
        Cmd::Trace { id } => usage::trace(&cli, id),
        Cmd::Margin {
            by,
            project,
            since,
            until,
        } => usage::margin(&cli, by, project, since, until),
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
