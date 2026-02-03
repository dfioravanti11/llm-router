use std::path::PathBuf;

use anyhow::Context;
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server failed")
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received; draining in-flight responses");
}
