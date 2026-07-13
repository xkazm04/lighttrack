//! `lt-runner` — the LightTrack scoring/benchmark worker. Runs locally / on the e2-micro (where
//! `claude` is authenticated and provider keys live), keeping the API free of model invocation.
//!
//! Subcommands: `score` / `score-text` (judge events or ad-hoc pairs), `bench` (run a benchmark:
//! compare / rubric / simple), `dataset build` (sample + anonymize), `serve` (job-queue worker).
//!
//! Layout: `cli` (args), `http` (API client), `util` (helpers), `score`, `dataset`, `bench`
//! (+`compare`, `rubric`), `serve`.

mod bench;
mod billing;
mod calibrate;
mod cli;
mod compare;
mod dataset;
mod http;
mod rubric;
mod schedule;
mod score;
mod serve;
mod stats;
mod util;

use anyhow::Result;
use clap::Parser;

use cli::{BillingCmd, Cli, Cmd, DatasetCmd};
use lighttrack_engine::EngineConfig;

fn main() -> Result<()> {
    let _ = dotenvy::dotenv(); // load .env (GEMINI_API_KEY, OPENAI_API_KEY, LIGHTTRACK_*) if present
    let cli = Cli::parse();
    let engine = EngineConfig {
        claude_bin: lighttrack_engine::resolve_claude_bin(&cli.claude_bin),
        model: cli.model.clone(),
        bare: cli.bare,
    };
    let http = http::client()?;

    match &cli.cmd {
        Cmd::Score {
            rubric,
            project,
            limit,
            interval,
        } => score::score_recent(&cli, &http, &engine, rubric, project.as_deref(), *limit, *interval),
        Cmd::ScoreText {
            rubric,
            input,
            output,
            project,
        } => score::score_text(&cli, &http, &engine, rubric, input, output, project),
        Cmd::Bench {
            benchmark,
            samples,
            gen_samples,
            heal,
        } => bench::run_benchmark(&cli, &http, &engine, benchmark, *samples, *gen_samples, *heal),
        Cmd::Dataset { action } => match action {
            DatasetCmd::Build {
                project,
                name,
                n,
                llm_scrub,
            } => dataset::build_dataset(&cli, &http, &engine, project, name, *n, *llm_scrub),
        },
        Cmd::Billing { action } => match action {
            BillingCmd::Sync {
                provider,
                project,
                days,
            } => billing::sync(&cli, &http, provider, project, *days),
        },
        Cmd::Schedule {
            project,
            interval,
            once,
            n,
            name_prefix,
            llm_scrub,
        } => schedule::schedule(
            &cli, &http, &engine, project, *interval, *once, *n, name_prefix, *llm_scrub,
        ),
        Cmd::Serve {
            once,
            interval,
            stale_secs,
        } => serve::serve(&cli, &http, &engine, *once, *interval, *stale_secs),
        Cmd::Calibrate {
            file,
            rubric,
            rubric_id,
            threshold,
            kappa_bar,
            samples,
            report,
        } => calibrate::calibrate(
            &cli,
            &http,
            &engine,
            file,
            rubric.as_deref(),
            rubric_id.as_deref(),
            *threshold,
            *kappa_bar,
            *samples,
            report.as_deref(),
        ),
    }
}
