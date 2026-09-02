//! `lt-runner` — the LightTrack scoring/benchmark worker. Runs locally / on the e2-micro (where
//! `claude` is authenticated and provider keys live), keeping the API free of model invocation.
//!
//! Subcommands: `score` / `score-text` (judge events or ad-hoc pairs), `bench` (run a benchmark:
//! compare / rubric / simple), `dataset build` (sample + anonymize), `serve` (job-queue worker).
//!
//! Since M7 the queue carries all five workloads, not just benchmark runs: `serve` declares which
//! kinds it can run (`--kinds`) and dispatches through `dispatch`, and the daemon subcommands take
//! `--via-queue` to route one cycle through the queue instead of running it in-process. Recurrence
//! is no longer a `--interval` flag or a sweep in this binary — it is a stored `Schedule` the API
//! sweeps, so it exists in deployments that never run a companion worker.
//!
//! Layout: `cli` (args), `http` (API client), `util` (helpers), `judge_spec` (the `--rubric` /
//! `--rubric-id` contract), `score`, `dataset`, `bench` (+`compare`, `rubric`), `serve`.

mod batch;
mod bench;
mod billing;
mod budget;
mod calibrate;
mod calibrate_batch;
mod calibrate_watch;
mod calibration_post;
mod cli;
mod compare;
mod dataset;
mod dispatch;
mod enqueue;
mod gate;
mod history;
mod http;
mod judge_spec;
mod labels;
mod pairwise;
mod provenance;
mod rubric;
mod runctl;
mod schedule;
mod score;
mod score_traces;
mod serve;
mod serve_api;
mod serve_job;
mod stats;
mod targets;
mod util;

use anyhow::Result;
use clap::Parser;

use cli::{BillingCmd, Cli, Cmd, DatasetCmd};
use lighttrack_core::JobKind;
use lighttrack_engine::EngineConfig;
use serde_json::{json, Map, Value};

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
            rubric_id,
            project,
            prompt_tag,
            limit,
            interval,
            via_queue,
        } => {
            if *via_queue {
                let mut p = Map::new();
                enqueue::judge_fields(&mut p, rubric.as_deref(), rubric_id.as_deref())?;
                p.insert("limit".into(), json!(limit));
                if let Some(pr) = project {
                    p.insert("project".into(), json!(pr));
                }
                if let Some(t) = prompt_tag {
                    p.insert("prompt_tag".into(), json!(t));
                }
                return enqueue::run_via_queue(
                    &cli,
                    &http,
                    &engine,
                    JobKind::ScoreEvents,
                    Value::Object(p),
                );
            }
            // Resolved once, before any judging: a bad rubric id fails immediately instead of on
            // every tick of an `--interval` loop.
            let judge =
                judge_spec::Judge::resolve(&cli, &http, rubric.as_deref(), rubric_id.as_deref())?;
            score::score_recent(
                &cli,
                &http,
                &engine,
                &judge,
                project.as_deref(),
                prompt_tag.as_deref(),
                *limit,
                *interval,
                cli.jobs,
            )
        }
        Cmd::ScoreText {
            rubric,
            rubric_id,
            input,
            output,
            project,
        } => {
            let judge =
                judge_spec::Judge::resolve(&cli, &http, rubric.as_deref(), rubric_id.as_deref())?;
            score::score_text(&cli, &http, &engine, &judge, input, output, project)
        }
        Cmd::ScoreTraces {
            project,
            rubric,
            rubric_id,
            sample_every,
            errors_always,
            settle_secs,
            limit,
            judge,
            interval,
            once,
            via_queue,
        } => {
            if *via_queue {
                let mut p = Map::new();
                enqueue::judge_fields(&mut p, rubric.as_deref(), rubric_id.as_deref())?;
                p.insert("project".into(), json!(project));
                p.insert("sample_every".into(), json!(sample_every));
                p.insert("errors_always".into(), json!(errors_always));
                p.insert("settle_secs".into(), json!(settle_secs));
                p.insert("limit".into(), json!(limit));
                if let Some(j) = judge {
                    p.insert("judge_model".into(), json!(j));
                }
                return enqueue::run_via_queue(
                    &cli,
                    &http,
                    &engine,
                    JobKind::ScoreTraces,
                    Value::Object(p),
                );
            }
            // Per-command judge override, else the global --model. Built without cloning EngineConfig.
            let eng = EngineConfig {
                claude_bin: engine.claude_bin.clone(),
                model: judge.clone().unwrap_or_else(|| engine.model.clone()),
                bare: engine.bare,
            };
            let params = score_traces::Params {
                project,
                rubric_text: rubric.as_deref(),
                rubric_id: rubric_id.as_deref(),
                sample_every: *sample_every,
                errors_always: *errors_always,
                settle_secs: *settle_secs,
                limit: *limit,
                interval: *interval,
                once: *once,
                jobs: cli.jobs,
            };
            score_traces::run(&cli, &http, &eng, &params)
        }
        Cmd::Bench {
            benchmark,
            samples,
            gen_samples,
            batch,
            heal,
            gate,
            pairwise,
        } => {
            let status = bench::run_benchmark(
                &cli,
                &http,
                &engine,
                benchmark,
                *samples,
                *gen_samples,
                *batch,
                *heal,
                *pairwise,
                cli.jobs,
                None,
                &runctl::RunControl::inert(),
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
        Cmd::Labels { action } => match action {
            cli::LabelsCmd::Import {
                file,
                project,
                labeler,
            } => labels::import(&cli, &http, file, project.as_deref(), labeler.as_deref()),
        },
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
            via_queue,
        } => {
            if *via_queue {
                return enqueue::run_via_queue(
                    &cli,
                    &http,
                    &engine,
                    JobKind::DatasetSample,
                    json!({ "project": project, "n": n, "name_prefix": name_prefix,
                            "llm_scrub": llm_scrub }),
                );
            }
            schedule::schedule(
                &cli,
                &http,
                &engine,
                project,
                *interval,
                *once,
                *n,
                name_prefix,
                *llm_scrub,
            )
        }
        Cmd::Serve {
            once,
            interval,
            stale_secs,
            lease_renew_secs,
            kinds,
            providers,
        } => {
            // An unknown kind is refused here rather than silently narrowing the worker to the ones
            // spelled correctly — which would look, from the outside, exactly like an empty queue.
            for k in kinds {
                if JobKind::from_wire(k).is_none() {
                    anyhow::bail!(
                        "unknown --kinds value '{k}': expected {}",
                        JobKind::vocabulary()
                    );
                }
            }
            let params = serve::ServeParams {
                once: *once,
                interval: *interval,
                stale_secs: *stale_secs,
                lease_renew_secs: *lease_renew_secs,
                kinds: kinds.clone(),
                providers: if providers.is_empty() {
                    serve::providers_from_env()
                } else {
                    providers.clone()
                },
            };
            serve::serve(&cli, &http, &engine, &params)
        }
        Cmd::Calibrate {
            file,
            dataset,
            rubric,
            rubric_id,
            threshold,
            kappa_bar,
            samples,
            compare_batch,
            report,
            watch,
            once,
            interval,
            drift_threshold,
            project,
            via_queue,
        } => {
            if *via_queue {
                let mut p = Map::new();
                enqueue::judge_fields(&mut p, rubric.as_deref(), rubric_id.as_deref())?;
                if let Some(f) = file {
                    p.insert("file".into(), json!(f));
                }
                if let Some(d) = dataset {
                    p.insert("dataset_id".into(), json!(d));
                }
                p.insert("threshold".into(), json!(threshold));
                p.insert("kappa_bar".into(), json!(kappa_bar));
                p.insert("drift_threshold".into(), json!(drift_threshold));
                p.insert("samples".into(), json!(samples));
                if let Some(pr) = project {
                    p.insert("project".into(), json!(pr));
                }
                return enqueue::run_via_queue(
                    &cli,
                    &http,
                    &engine,
                    JobKind::Calibrate,
                    Value::Object(p),
                );
            }
            let set = calibrate::load_set(&cli, &http, file.as_deref(), dataset.as_deref())?;
            if *watch || *once {
                let params = calibrate_watch::WatchParams {
                    set: &set,
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
            } else if let Some(batch) = *compare_batch {
                let rid = rubric_id.as_deref().ok_or_else(|| {
                    anyhow::anyhow!("--compare-batch needs --rubric-id: batching is only implemented for structured rubrics")
                })?;
                let rubric = calibrate::resolve_rubric(&cli, &http, Some(rid))?
                    .ok_or_else(|| anyhow::anyhow!("rubric {rid} not found"))?;
                let items = set.items.clone();
                let prices: Vec<lighttrack_core::ModelPriceRow> =
                    http::get(&cli, &http, "/v1/prices").unwrap_or_default();
                let (jp, jm) = lighttrack_engine::parse_judge_spec(&cli.model);
                calibrate_batch::compare(
                    &engine, &jp, &jm, &rubric, &items, batch, *samples, cli.jobs, *threshold,
                    &prices,
                )?;
                Ok(())
            } else {
                calibrate::calibrate(
                    &cli,
                    &http,
                    &engine,
                    &set,
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
