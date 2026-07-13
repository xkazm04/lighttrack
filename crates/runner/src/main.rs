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
mod calibrate_watch;
mod cli;
mod compare;
mod dataset;
mod gate;
mod http;
mod pairwise;
mod recurrence;
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
        } => score::score_recent(
            &cli, &http, &engine, rubric, project.as_deref(), *limit, *interval, cli.jobs,
        ),
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
            gate,
            pairwise,
        } => {
            let status = bench::run_benchmark(
                &cli, &http, &engine, benchmark, *samples, *gen_samples, *heal, *pairwise, cli.jobs,
            )?;
            if *gate {
                let code = gate::gate_exit_code(&status);
                if code != 0 {
                    eprintln!("gate: benchmark verdict '{status}' — failing build (exit {code})");
                    std::process::exit(code);
                }
                println!("gate: benchmark verdict '{status}' — ok");
            }
            Ok(())
        }
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
            recur_interval,
        } => serve::serve(&cli, &http, &engine, *once, *interval, *stale_secs, *recur_interval),
        Cmd::Calibrate {
            file,
            rubric,
            rubric_id,
            threshold,
            kappa_bar,
            samples,
            report,
            watch,
            once,
            interval,
            drift_threshold,
            project,
        } => {
            if *watch || *once {
                let params = calibrate_watch::WatchParams {
                    file,
                    rubric_text: rubric.as_deref(),
                    rubric_id: rubric_id.as_deref(),
                    project: project.as_deref(),
                    threshold: *threshold,
                    kappa_bar: *kappa_bar,
                    drift_threshold: *drift_threshold,
                    samples: *samples,
                    interval: *interval,
                    once: *once,
                    jobs: cli.jobs,
                };
                let code = calibrate_watch::watch(&cli, &http, &engine, &params)?;
                if code != 0 {
                    std::process::exit(code);
                }
                Ok(())
            } else {
                calibrate::calibrate(
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
                    cli.jobs,
                )
            }
        }
    }
}
