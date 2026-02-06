//! Open-loop load generator and statistics harness.
//!
//! Usable against any OpenAI-compatible endpoint, not just this project's
//! router. Three properties are the reason it exists rather than an
//! off-the-shelf generator:
//!
//! - Arrivals follow a schedule computed before the run starts, so a slow
//!   response cannot delay a later arrival and hide the queueing it caused.
//! - Every latency is recorded against both the intended dispatch time and the
//!   actual one, so the coordinated-omission gap comes out of any run.
//! - A run whose generator fell behind its own schedule is marked invalid.

pub mod aggregate;
pub mod record;
pub mod report;
pub mod runner;
pub mod schedule;
pub mod stats;
pub mod workload;

pub use aggregate::Campaign;
pub use record::{Mode, RequestRecord, RunConfig};
pub use report::{RunReport, Validity};

use std::time::SystemTime;

/// Run once and build its report.
pub async fn run_once(config: &RunConfig) -> anyhow::Result<(RunReport, Vec<RequestRecord>)> {
    let started_at = SystemTime::now();
    let outcome = runner::run(config).await?;
    let report = RunReport::build(config, &outcome.records, outcome.wall_clock, started_at);
    Ok((report, outcome.records))
}

/// Install the tracing subscriber. Safe to call more than once.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warmpath_bench=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .try_init();
}
