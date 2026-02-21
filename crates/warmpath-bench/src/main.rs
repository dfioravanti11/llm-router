use std::path::{Path, PathBuf};

use anyhow::Context;
use clap::{Parser, Subcommand};
use warmpath_bench::record::{Mode, RunConfig};
use warmpath_bench::report::{write_run, RunReport};
use warmpath_bench::{init_tracing, run_once, Campaign};

/// Open-loop load generator for OpenAI-compatible endpoints.
#[derive(Debug, Parser)]
#[command(name = "warmpath-bench", version, about)]
struct Args {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Run one or more repetitions of a configuration and aggregate them.
    Run(RunArgs),
    /// Aggregate run reports that already exist on disk.
    Aggregate(AggregateArgs),
}

#[derive(Debug, Parser)]
struct RunArgs {
    /// Base URL of the endpoint under test.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    target: String,

    #[arg(long, default_value = "/v1/chat/completions")]
    endpoint: String,

    #[arg(long, default_value = "mock-model")]
    model: String,

    #[arg(long, value_enum, default_value_t = Mode::OpenLoop)]
    mode: Mode,

    /// Arrivals per second. Open loop only.
    #[arg(long, default_value_t = 50.0)]
    rate: f64,

    /// Concurrent callers. Closed loop only.
    #[arg(long, default_value_t = 8)]
    concurrency: usize,

    /// Measurement duration in seconds, warmup included.
    #[arg(long, default_value_t = 30.0)]
    duration: f64,

    /// Leading seconds excluded from the reported summary.
    #[arg(long, default_value_t = 5.0)]
    warmup: f64,

    /// Base seed. Repetition `n` uses `seed + n`, so repetitions differ but the
    /// campaign as a whole reproduces.
    #[arg(long, default_value_t = 1)]
    seed: u64,

    /// Independent repetitions. The spec's floor for a published number is 3.
    #[arg(long, default_value_t = 3)]
    runs: usize,

    /// Words in the varying part of each prompt.
    #[arg(long, default_value_t = 128)]
    prompt_words: usize,

    /// Words in the shared prefix each request carries. Zero makes every
    /// prompt independent, which is the control condition for cache-aware
    /// routing.
    #[arg(long, default_value_t = 0)]
    shared_prefix_words: usize,

    /// Distinct shared prefixes in circulation.
    #[arg(long, default_value_t = 0)]
    prefix_pool: usize,

    /// Share of requests using the single hottest prefix, between 0 and 1.
    /// Zero spreads them evenly, which real traffic never does.
    #[arg(long, default_value_t = 0.0)]
    hot_prefix_share: f64,

    /// Turns per session. One means every request is independent.
    #[arg(long, default_value_t = 1)]
    session_turns: usize,

    /// Label recorded with the run, naming the configuration under test.
    #[arg(long, default_value = "")]
    label: String,

    #[arg(long, default_value_t = 64)]
    max_tokens: usize,

    /// Request a non-streaming response.
    #[arg(long)]
    no_stream: bool,

    /// A run whose p99 dispatch lag exceeds this is marked invalid.
    #[arg(long, default_value_t = 10.0)]
    max_dispatch_lag_ms: f64,

    /// Directory that run directories are written under.
    #[arg(long, default_value = "results")]
    out: PathBuf,

    /// Seconds to idle between repetitions, so one run's tail does not land in
    /// the next run's warmup.
    #[arg(long, default_value_t = 2.0)]
    settle: f64,
}

#[derive(Debug, Parser)]
struct AggregateArgs {
    /// Run directories, or `report.json` files, to aggregate.
    #[arg(required = true)]
    paths: Vec<PathBuf>,

    /// Where to write the campaign summary.
    #[arg(long)]
    out: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    match Args::parse().command {
        Command::Run(args) => run(args).await,
        Command::Aggregate(args) => aggregate(args),
    }
}

async fn run(args: RunArgs) -> anyhow::Result<()> {
    anyhow::ensure!(args.runs > 0, "--runs must be at least 1");

    let mut reports = Vec::with_capacity(args.runs);

    for repetition in 0..args.runs {
        let config = RunConfig {
            target: args.target.clone(),
            endpoint: args.endpoint.clone(),
            model: args.model.clone(),
            mode: args.mode,
            rate_per_second: args.rate,
            concurrency: args.concurrency,
            duration_secs: args.duration,
            warmup_secs: args.warmup,
            seed: args.seed + repetition as u64,
            label: args.label.clone(),
            prompt_words: args.prompt_words,
            shared_prefix_words: args.shared_prefix_words,
            prefix_pool: args.prefix_pool,
            hot_prefix_share: args.hot_prefix_share,
            session_turns: args.session_turns,
            max_tokens: args.max_tokens,
            stream: !args.no_stream,
            max_dispatch_lag_ms: args.max_dispatch_lag_ms,
        };

        tracing::info!(
            repetition = repetition + 1,
            of = args.runs,
            seed = config.seed,
            "starting run"
        );

        let (report, records) = run_once(&config).await?;
        let directory = write_run(&args.out, &report, &records)?;

        println!("{}", report.headline());
        if !report.validity.valid {
            for reason in &report.validity.reasons {
                println!("  invalid: {reason}");
            }
        }
        tracing::info!(directory = %directory.display(), "run written");

        reports.push(report);

        if repetition + 1 < args.runs && args.settle > 0.0 {
            tokio::time::sleep(std::time::Duration::from_secs_f64(args.settle)).await;
        }
    }

    report_campaign(&reports, Some(&args.out.join("campaign.json")))
}

fn aggregate(args: AggregateArgs) -> anyhow::Result<()> {
    let mut reports = Vec::with_capacity(args.paths.len());
    for path in &args.paths {
        reports.push(read_report(path)?);
    }
    report_campaign(&reports, args.out.as_deref())
}

fn read_report(path: &Path) -> anyhow::Result<RunReport> {
    let file = if path.is_dir() {
        path.join("report.json")
    } else {
        path.to_path_buf()
    };

    let text = std::fs::read_to_string(&file)
        .with_context(|| format!("failed to read {}", file.display()))?;
    serde_json::from_str(&text).with_context(|| format!("failed to parse {}", file.display()))
}

fn report_campaign(reports: &[RunReport], out: Option<&Path>) -> anyhow::Result<()> {
    let campaign = Campaign::from_reports(reports)?;

    println!();
    println!("{} run(s), {} valid", campaign.runs, campaign.valid_runs);
    for excluded in &campaign.excluded {
        println!(
            "  excluded {}: {}",
            excluded.run_id,
            excluded.reasons.join("; ")
        );
    }
    println!();
    print!("{}", campaign.table());

    match campaign.publishable() {
        Ok(()) => println!("\nthis campaign meets the bar for publishing"),
        Err(reason) => println!("\nnot publishable: {reason}"),
    }

    print_omission_gap(reports);

    if let Some(path) = out {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_vec_pretty(&campaign)?)?;
        println!("campaign written to {}", path.display());
    }

    Ok(())
}

/// Print the coordinated-omission gap from the first valid run.
///
/// The gap is a property of one run rather than of the campaign: it compares
/// two views of the same requests, so averaging it across runs would blur the
/// thing being shown.
fn print_omission_gap(reports: &[RunReport]) {
    let Some(report) = reports
        .iter()
        .find(|report| report.validity.valid && !report.omission_gap_ttft.is_empty())
    else {
        return;
    };

    println!();
    println!(
        "coordinated-omission gap on time to first token ({})",
        report.run_id
    );
    println!(
        "{:>10} {:>16} {:>16} {:>8}",
        "percentile", "from intended", "from dispatch", "ratio"
    );
    for gap in &report.omission_gap_ttft {
        println!(
            "{:>10} {:>13.1}ms {:>13.1}ms {:>8.2}",
            gap.percentile,
            gap.from_intended_us as f64 / 1_000.0,
            gap.from_dispatch_us as f64 / 1_000.0,
            gap.ratio
        );
    }
}
