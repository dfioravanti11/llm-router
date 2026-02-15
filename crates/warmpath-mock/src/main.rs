use std::net::SocketAddr;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::net::TcpListener;
use warmpath_mock::{router, MockConfig, MockState};

/// Mock inference worker for GPU-free development.
#[derive(Debug, Parser)]
#[command(name = "warmpath-mock", version, about)]
struct Args {
    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8001")]
    bind: SocketAddr,

    /// Model name reported in responses.
    #[arg(long, default_value = "mock-model")]
    model: String,

    /// Delay before the first token of a streamed response, in milliseconds.
    #[arg(long, default_value_t = 20)]
    ttft_ms: u64,

    /// Delay between tokens, in milliseconds.
    #[arg(long, default_value_t = 5)]
    inter_token_ms: u64,

    /// Tokens to emit when the request does not set `max_tokens`.
    #[arg(long, default_value_t = 32)]
    default_max_tokens: usize,

    /// Requests served at once. Anything beyond this queues.
    #[arg(long, default_value_t = 256)]
    max_concurrency: usize,

    /// Blocks the simulated prefix cache holds. Zero disables it, which is the
    /// control condition for any cache-aware routing measurement.
    #[arg(long, default_value_t = 0)]
    cache_blocks: usize,

    /// Token ids per block. Must match the router's.
    #[arg(long, default_value_t = 16)]
    block_size: usize,

    /// Prefill cost per uncached prompt token, in microseconds.
    #[arg(long, default_value_t = 50)]
    prefill_per_token_us: u64,

    /// Directory holding the model's tokenizer.json and tokenizer_config.json.
    /// Must be the same model the router is configured with.
    #[arg(long)]
    model_dir: Option<std::path::PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    init_tracing();

    let state = MockState::new(MockConfig {
        model: args.model,
        time_to_first_token: Duration::from_millis(args.ttft_ms),
        inter_token_delay: Duration::from_millis(args.inter_token_ms),
        default_max_tokens: args.default_max_tokens,
        max_concurrency: args.max_concurrency,
        cache_blocks: args.cache_blocks,
        block_size: args.block_size,
        prefill_per_token: Duration::from_micros(args.prefill_per_token_us),
        model_directory: args.model_dir,
    });

    let listener = TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("failed to bind {}", args.bind))?;
    let local_addr = listener.local_addr()?;
    tracing::info!(%local_addr, "mock worker listening");

    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("mock worker failed")
}

fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warmpath_mock=info"));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutdown signal received");
}
