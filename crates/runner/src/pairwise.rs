//! Pairwise comparison: round-robin A-vs-B preference judging across a benchmark's targets, printed
//! *alongside* the per-target scorecard table. Each unordered target pair is judged per case with the
//! order-debiased engine judge; results roll up into a win/loss/tie matrix and a win-rate ranking.
//! Non-goals: Elo / Bradley-Terry rating math, UI.

use anyhow::Result;
use serde_json::{json, Value};

use lighttrack_core::{BenchTarget, Benchmark, BenchmarkCase, ModelPriceRow, Rubric};
use lighttrack_engine::{generate, run_pairwise, EngineConfig, PairwiseWinner};

use crate::cli::Cli;
use crate::http::post;
use crate::util::{parallel_map, price_gen_cost};

/// A target's round-robin standing.
#[derive(Clone, Default)]
struct Standing {
    wins: u32,
    losses: u32,
    ties: u32,
}

impl Standing {
    fn games(&self) -> u32 {
        self.wins + self.losses + self.ties
    }
    /// Win rate with ties as half-wins; 0.5 when a target played no games.
    fn win_rate(&self) -> f64 {
        let g = self.games();
        if g == 0 {
            0.5
        } else {
            (self.wins as f64 + 0.5 * self.ties as f64) / g as f64
        }
    }
}

/// One generated candidate for a (target, case) cell.
struct GenCell {
    output: Option<String>,
    cost: f64,
    tokens: u64,
}

/// Judge criteria for the pairwise prompt: the structured rubric's dimensions, else the freeform text.
fn criteria_of(rubric: &Option<Rubric>, bench: &Benchmark) -> Option<String> {
    let text = match rubric {
        Some(r) => r
            .dimensions
            .iter()
            .map(|d| format!("{} ({})", d.key, d.description))
            .collect::<Vec<_>>()
            .join("; "),
        None => bench.rubric.clone(),
    };
    (!text.is_empty()).then_some(text)
}

fn label_of(t: &BenchTarget) -> String {
    t.label.clone().unwrap_or_else(|| format!("{}/{}", t.provider, t.model))
}

/// Run the round-robin pairwise phase and print the matrix + ranking. `jp`/`jm` are the judge
/// provider/model; `jobs` bounds concurrency across cells and games.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_pairwise_matrix(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    engine: &EngineConfig,
    bench: &Benchmark,
    cases: &[BenchmarkCase],
    targets: &[BenchTarget],
    rubric: &Option<Rubric>,
    prices: &[ModelPriceRow],
    jp: &str,
    jm: &str,
    jobs: usize,
) -> Result<()> {
    if targets.len() < 2 {
        println!("\nPAIRWISE: need ≥2 targets to compare; skipping.");
        return Ok(());
    }
    let labels: Vec<String> = targets.iter().map(label_of).collect();
    let criteria = criteria_of(rubric, bench);
    let (n_t, n_c) = (targets.len(), cases.len());
    println!("\nPAIRWISE (round-robin, order-debiased): {n_t} targets × {n_c} case(s), judge={jp}/{jm}");

    // Pre-flight cost gate. The full round-robin is `round_robin_games` games and each game is TWO
    // judge calls (both A/B orders, for debias), so the call count jumps super-linearly in targets:
    // 4→8 targets is ~4.7× the spend for a 2× target list, and nothing downstream warns before the
    // first call is paid for. Surface the number and refuse to start an oversized sweep.
    let max_possible = round_robin_games(n_t, n_c);
    let judge_calls = 2 * max_possible;
    let dollar_hint = match price_gen_cost(prices, jp, jm, Some(1500), Some(400)) {
        c if c > 0.0 => format!(" (~${:.2} at ~1.5k/0.4k tokens per call)", c * judge_calls as f64),
        _ => String::new(),
    };
    println!("  cost pre-flight: up to {max_possible} games ⇒ ~{judge_calls} judge calls{dollar_hint}");
    if max_possible > cli.max_games {
        println!(
            "  ABORT (pairwise): {max_possible} games exceeds --max-games {}. Re-run with \
             --max-games {max_possible} to proceed, or reduce targets/cases.",
            cli.max_games
        );
        return Ok(());
    }

    // 1. Generate one candidate per (target, case) cell, in parallel.
    let cells: Vec<GenCell> = parallel_map(n_t * n_c, jobs, |idx| {
        let (ti, ci) = (idx / n_c, idx % n_c);
        let t = &targets[ti];
        match generate(engine, &t.provider, &t.model, t.system_prompt.as_deref(), &cases[ci].input, None) {
            Ok(g) => GenCell {
                cost: g.cost_usd.unwrap_or_else(|| {
                    price_gen_cost(prices, &t.provider, &t.model, g.input_tokens, g.output_tokens)
                }),
                tokens: g.input_tokens.unwrap_or(0) + g.output_tokens.unwrap_or(0),
                output: Some(g.output),
            },
            Err(e) => {
                eprintln!("  gen error [{}, case {}]: {e}", labels[ti], ci + 1);
                GenCell { output: None, cost: 0.0, tokens: 0 }
            }
        }
    });
    let mut gen_cost = 0.0;
    let mut gen_tokens = 0u64;
    for c in &cells {
        gen_cost += c.cost;
        gen_tokens += c.tokens;
    }
    let output = |ti: usize, ci: usize| cells[ti * n_c + ci].output.as_deref();

    // 2. Enumerate games (case, i, j) for each unordered pair with both candidates present.
    let mut games: Vec<(usize, usize, usize)> = Vec::new();
    for ci in 0..n_c {
        for i in 0..n_t {
            for j in (i + 1)..n_t {
                if output(i, ci).is_some() && output(j, ci).is_some() {
                    games.push((ci, i, j));
                }
            }
        }
    }

    // 3. Judge every game with the order-debiased engine judge, in parallel.
    let outcomes: Vec<Result<lighttrack_engine::PairwiseOutcome>> = parallel_map(games.len(), jobs, |g| {
        let (ci, i, j) = games[g];
        run_pairwise(
            engine, jp, jm, &cases[ci].input, cases[ci].expected.as_deref(),
            output(i, ci).unwrap_or_default(), output(j, ci).unwrap_or_default(), criteria.as_deref(),
        )
        .map_err(anyhow::Error::from)
    });

    // 4. Fold outcomes (in game order — deterministic at any --jobs): accrue judge cost/tokens and
    // collect the per-game winners, then tally standings + head-to-head matrix. `parallel_map` is
    // eager, so every game has already been run and PAID FOR by the time we fold. A `?` here would
    // discard the whole matrix — every good verdict and the generation spend — on one transient
    // tail failure, and skip `post_run` so the cost never lands in the ledger. Instead, drop the
    // failed game (count it), keep the rest, and always reach `post_run`.
    let (mut judge_cost, mut judge_tokens) = (0.0_f64, 0u64);
    let mut played: Vec<(usize, usize, usize)> = Vec::with_capacity(games.len());
    let mut winners: Vec<(PairwiseWinner, bool)> = Vec::with_capacity(games.len());
    let mut judge_errors = 0u32;
    for (&g, outcome) in games.iter().zip(outcomes) {
        match outcome {
            Ok(o) => {
                judge_cost += o.cost_usd.unwrap_or_else(|| {
                    price_gen_cost(prices, jp, jm, Some(o.input_tokens), Some(o.output_tokens))
                });
                judge_tokens += o.tokens;
                played.push(g);
                winners.push((o.winner, o.position_bias));
            }
            Err(e) => {
                let (ci, i, j) = g;
                eprintln!("  judge error [case {}, {} vs {}]: {e}", ci + 1, labels[i], labels[j]);
                judge_errors += 1;
            }
        }
    }
    let (standings, beats, bias_count) = tally(n_t, &played, &winners);

    print_ranking(&labels, &standings);
    print_matrix(&labels, &beats);
    println!(
        "  games={}  judge_errors={judge_errors}  positional_ties(bias)={bias_count}  gen_cost=${gen_cost:.5}  judge_cost=${judge_cost:.5}  total=${:.5}",
        played.len(),
        gen_cost + judge_cost,
    );

    post_run(cli, http, bench, &labels, &standings, &beats, played.len(), judge_errors, bias_count, gen_cost, judge_cost, gen_tokens + judge_tokens)
}

/// Number of games in a full round-robin: one per unordered target pair, per case. `O(n_t² · n_c)`.
fn round_robin_games(n_t: usize, n_c: usize) -> usize {
    n_c * n_t * n_t.saturating_sub(1) / 2
}

/// Roll per-game winners (aligned with `games`) into standings + a head-to-head matrix + a count of
/// games decided a tie by position bias. Pure, so the aggregation is unit-tested without live calls.
fn tally(
    n_t: usize,
    games: &[(usize, usize, usize)],
    winners: &[(PairwiseWinner, bool)],
) -> (Vec<Standing>, Vec<Vec<u32>>, u32) {
    let mut standings = vec![Standing::default(); n_t];
    let mut beats = vec![vec![0u32; n_t]; n_t]; // beats[i][j] = times i beat j
    let mut bias_count = 0u32;
    for (&(_, i, j), &(winner, bias)) in games.iter().zip(winners) {
        if bias {
            bias_count += 1;
        }
        match winner {
            PairwiseWinner::A => {
                standings[i].wins += 1;
                standings[j].losses += 1;
                beats[i][j] += 1;
            }
            PairwiseWinner::B => {
                standings[j].wins += 1;
                standings[i].losses += 1;
                beats[j][i] += 1;
            }
            PairwiseWinner::Tie => {
                standings[i].ties += 1;
                standings[j].ties += 1;
            }
        }
    }
    (standings, beats, bias_count)
}

/// Ranking by win-rate (ties = half-wins), descending, stable on original target order.
fn print_ranking(labels: &[String], standings: &[Standing]) {
    let mut order: Vec<usize> = (0..labels.len()).collect();
    order.sort_by(|&a, &b| {
        standings[b].win_rate().total_cmp(&standings[a].win_rate()).then(a.cmp(&b))
    });
    println!("  win-rate ranking:");
    for (rank, &i) in order.iter().enumerate() {
        let s = &standings[i];
        println!(
            "    {}. {:<20} win_rate={:.3}  W-L-T={}-{}-{}",
            rank + 1, trunc(&labels[i], 20), s.win_rate(), s.wins, s.losses, s.ties
        );
    }
}

/// Head-to-head matrix: cell (row, col) = times the row target beat the column target.
fn print_matrix(labels: &[String], beats: &[Vec<u32>]) {
    let n = labels.len();
    println!("  head-to-head (row beats col):");
    print!("    {:<14}", "");
    for j in 0..n {
        print!("{:>5}", format!("T{}", j + 1));
    }
    println!();
    for (i, row) in beats.iter().enumerate() {
        print!("    T{:<2} {:<10}", i + 1, trunc(&labels[i], 10));
        for (j, wins) in row.iter().enumerate() {
            if i == j {
                print!("{:>5}", "-");
            } else {
                print!("{:>5}", wins);
            }
        }
        println!();
    }
}

fn trunc(s: &str, n: usize) -> String {
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n - 1).collect::<String>())
    } else {
        s.to_string()
    }
}

/// Record the pairwise phase as a benchmark run (mode `pairwise`) so its cost lands in run totals.
#[allow(clippy::too_many_arguments)]
fn post_run(
    cli: &Cli,
    http: &reqwest::blocking::Client,
    bench: &Benchmark,
    labels: &[String],
    standings: &[Standing],
    beats: &[Vec<u32>],
    n_games: usize,
    judge_errors: u32,
    bias_count: u32,
    gen_cost: f64,
    judge_cost: f64,
    total_tokens: u64,
) -> Result<()> {
    let ranking: Vec<Value> = labels
        .iter()
        .enumerate()
        .map(|(i, l)| {
            json!({
                "target": l, "win_rate": standings[i].win_rate(),
                "wins": standings[i].wins, "losses": standings[i].losses, "ties": standings[i].ties,
            })
        })
        .collect();
    let report = json!({
        "mode": "pairwise", "targets": labels, "ranking": ranking,
        "beats_matrix": beats, "n_games": n_games, "judge_errors": judge_errors,
        "positional_bias_ties": bias_count,
        "gen_cost_usd": gen_cost, "judge_cost_usd": judge_cost,
    });
    // A pairwise run ranks targets against each other — there is no baseline to regress against, so
    // its honest status in the unified vocabulary is `no_baseline` (never gates a build).
    let run = json!({
        "benchmark_id": bench.id, "n_cases": n_games as u32, "cost_usd": gen_cost + judge_cost,
        "status": "no_baseline", "finished_at": crate::util::now_ts(),
        "total_tokens": total_tokens, "report": report,
    });
    post(cli, http, "/v1/benchmark-runs", &run)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    #[test]
    fn tally_builds_matrix_and_standings() {
        // 3 targets, one case → 3 games: (0,1), (0,2), (1,2).
        let games = vec![(0, 0, 1), (0, 0, 2), (0, 1, 2)];
        // 0 beats 1 (A); 2 beats 0 (B); 1 ties 2. Middle game is a position-bias tie.
        let winners = vec![
            (PairwiseWinner::A, false),
            (PairwiseWinner::B, true),
            (PairwiseWinner::Tie, false),
        ];
        let (standings, beats, bias) = tally(3, &games, &winners);
        assert_eq!(bias, 1, "one game was a positional tie");
        // Target 0: beat 1, lost to 2 → 1W-1L-0T.
        assert_eq!((standings[0].wins, standings[0].losses, standings[0].ties), (1, 1, 0));
        // Target 1: lost to 0, tied 2 → 0W-1L-1T.
        assert_eq!((standings[1].wins, standings[1].losses, standings[1].ties), (0, 1, 1));
        // Target 2: beat 0, tied 1 → 1W-0L-1T.
        assert_eq!((standings[2].wins, standings[2].losses, standings[2].ties), (1, 0, 1));
        // Head-to-head: 0 beat 1 once; 2 beat 0 once.
        assert_eq!(beats[0][1], 1);
        assert_eq!(beats[2][0], 1);
        assert_eq!(beats[1][0], 0);
    }

    #[test]
    fn round_robin_games_is_quadratic_in_targets() {
        // 2 targets × N cases = N games (the common "A vs B" shape).
        assert_eq!(round_robin_games(2, 100), 100);
        // The report's danger case: 8 targets × 100 cases = 2800 games ⇒ 5600 judge calls.
        assert_eq!(round_robin_games(8, 100), 2800);
        // 4→8 targets is a ~4.7× jump for a 2× target list (600 → 2800 games at 100 cases).
        assert_eq!(round_robin_games(4, 100), 600);
        // Degenerate inputs don't panic (saturating_sub guards n_t = 0).
        assert_eq!(round_robin_games(0, 100), 0);
        assert_eq!(round_robin_games(1, 100), 0);
    }

    #[test]
    fn win_rate_counts_ties_as_half() {
        let s = Standing { wins: 2, losses: 1, ties: 1 };
        // (2 + 0.5) / 4 = 0.625
        assert!(approx(s.win_rate(), 0.625));
        // No games → neutral 0.5.
        assert!(approx(Standing::default().win_rate(), 0.5));
    }

    #[test]
    fn ranking_order_is_by_win_rate_desc_stable() {
        // Two targets tied on win-rate keep their original index order.
        let standings = vec![
            Standing { wins: 1, losses: 1, ties: 0 }, // 0.5
            Standing { wins: 2, losses: 0, ties: 0 }, // 1.0
            Standing { wins: 1, losses: 1, ties: 0 }, // 0.5
        ];
        let mut order: Vec<usize> = (0..3).collect();
        order.sort_by(|&a, &b| {
            standings[b].win_rate().total_cmp(&standings[a].win_rate()).then(a.cmp(&b))
        });
        assert_eq!(order, vec![1, 0, 2], "highest win-rate first, ties keep index order");
    }
}
