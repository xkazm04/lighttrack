//! `lt` — LightTrack operator CLI. A thin HTTP client over the API.
//!
//! Global options (also read from env):
//!   --base  LIGHTTRACK_URL  (default http://127.0.0.1:8787)
//!   --key   LIGHTTRACK_KEY  (admin key for management, or a project key for scoped reads)
//!
//! Examples:
//!   lt projects create --name billing-demo --id billing-demo
//!   lt keys create --project <id> --name app-key
//!   lt limits set --project <id> --metric cost_usd --window day --threshold 5 --action alert
//!   lt limits status --project <id>
//!   lt rubrics create --project <id> --file rubric.json
//!   lt costs --project <id>
//!   lt events --project <id> --limit 20
//!
//! Layout: `cli` (args), `http` (API client + output), then one module per domain — `projects`
//! (projects + keys), `limits`, `rubrics`, `usage` (costs / events / traces / margin), `collective`.

mod cli;
mod collective;
mod http;
mod limits;
mod projects;
mod rubrics;
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
        Cmd::Rubrics { action } => rubrics::run(&cli, action),
        Cmd::Costs { project } => usage::costs(&cli, project),
        Cmd::Events { project, limit } => usage::events(&cli, project, *limit),
        Cmd::Traces { project, limit } => usage::traces(&cli, project, *limit),
        Cmd::Trace { id } => usage::trace(&cli, id),
        Cmd::Margin {
            by,
            project,
            since,
            until,
        } => usage::margin(&cli, by, project, since, until),
        Cmd::Collective { action } => collective::run(&cli, action),
    }
}
