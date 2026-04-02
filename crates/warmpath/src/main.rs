use std::path::PathBuf;

use anyhow::Context;
use axum::serve::ListenerExt;
use clap::Parser;
use tokio::net::TcpListener;
use warmpath::{init_tracing, router, Config};

/// KV-cache-aware LLM inference router.
#[derive(Debug, Parser)]
#[command(name = "warmpath", version, about)]
struct Args {
    /// Path to the TOML config file.
    #[arg(long, short, default_value = "config/warmpath.toml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing();

    let config = Config::load(&args.config)?;
    let app = router(&config)?;

    let listener = TcpListener::bind(config.server.bind)
        .await
        .with_context(|| format!("failed to bind {}", config.server.bind))?;
    let local_addr = listener.local_addr()?;

    for worker in &config.workers {
        tracing::info!(name = %worker.name, url = %worker.url, "worker configured");
    }
    tracing::info!(%local_addr, "warmpath listening");

    // Nagle's algorithm holds a small write back, waiting to coalesce it with
    // the next one. The peer's delayed acknowledgement waits too. Together they
    // stall for the length of the delayed-ack timer, which on Linux is 40ms.
    //
    // A streaming proxy writes small things constantly: response headers, then
    // one SSE event per token. Every one of those is a candidate for the stall,
    // and the first one lands squarely in time to first token.
    //
    // Measured against real vLLM on 2026-04-06, this cost 40.2ms at the median
    // with an interval of 0.6ms. A constant that precise is a timer, not work.
    let listener = listener.tap_io(|stream| {
        if let Err(error) = stream.set_nodelay(true) {
            tracing::warn!(%error, "could not disable Nagle on an accepted connection");
        }
    });

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received; draining in-flight responses");
}
