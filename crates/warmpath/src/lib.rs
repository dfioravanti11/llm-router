//! Warmpath: a KV-cache-aware router for LLM inference fleets.
//!
//! A request arrives, its prompt is rendered and cut into hashed blocks, the
//! block index says which workers are likely to already hold that prefix, and
//! a policy weighs that against load. The response streams back untouched.

pub mod config;
pub mod error;
pub mod index;
pub mod metrics;
pub mod policy;
pub mod prompt;
pub mod proxy;
pub mod session;
pub mod state;
pub mod worker;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;
use warmpath_core::{ModelFiles, PromptBuilder};

pub use config::Config;
pub use metrics::Metrics;
pub use worker::WorkerPool;

/// Shared state handed to every request. Cheap to clone.
#[derive(Clone)]
pub struct AppState {
    pub pool: Arc<WorkerPool>,
    pub metrics: Arc<Metrics>,
    pub request_ids: Arc<RequestIds>,
    pub max_request_bytes: usize,
    /// `None` when the configured policy does not read the block index.
    /// Building a prompt costs a render, a tokenize, and a hash chain, and a
    /// baseline run must not pay for work its policy never uses.
    pub prompt_builder: Option<Arc<PromptBuilder>>,
}

/// Per-process request id source.
///
/// Ids restart at zero when the router restarts. That is deliberate: a run is
/// the unit of analysis for this project, and ids that are stable across
/// identical runs make two benchmark runs easier to diff.
#[derive(Debug, Default)]
pub struct RequestIds {
    next: AtomicU64,
}

impl RequestIds {
    pub fn next(&self) -> String {
        format!("req_{:012}", self.next.fetch_add(1, Ordering::Relaxed))
    }
}

/// Build the router's HTTP surface from a validated config.
pub fn router(config: &Config) -> anyhow::Result<Router> {
    let metrics = Arc::new(Metrics::new());
    let prompt_builder = if config.routing.policy.needs_prompt_fingerprint() {
        Some(Arc::new(build_prompt_builder(config)?))
    } else {
        None
    };

    let state = AppState {
        pool: Arc::new(WorkerPool::new(config, &metrics)?),
        metrics,
        request_ids: Arc::new(RequestIds::default()),
        max_request_bytes: config.server.max_request_bytes,
        prompt_builder,
    };

    state.pool.spawn_pollers();

    Ok(Router::new()
        .route("/health", get(proxy::health))
        .route("/metrics", get(proxy::metrics))
        .route("/debug/index", get(proxy::index_stats))
        .route("/debug/workers", get(proxy::worker_stats))
        .route("/v1/chat/completions", post(proxy::proxy))
        .route("/v1/completions", post(proxy::proxy))
        .with_state(state))
}

/// Build the prompt builder the configured policy needs.
///
/// A configured model directory that fails to load is a startup error, not a
/// fall back. Falling back would leave the router cutting blocks at different
/// boundaries than the worker, which produces a poor hit rate and no error,
/// and that silent failure is the highest-risk item in the project.
fn build_prompt_builder(config: &Config) -> anyhow::Result<PromptBuilder> {
    match &config.model.directory {
        Some(directory) => {
            let files = ModelFiles::new(directory.clone());
            let builder = files.load(config.index.block_size)?;
            tracing::info!(
                tokenizer = builder.tokenizer_name(),
                block_size = builder.block_size(),
                directory = %directory.display(),
                "loaded the model's tokenizer and chat template"
            );
            Ok(builder)
        }
        None => {
            tracing::warn!(
                "no [model] directory configured; using the development tokenizer. \
                 Block boundaries will not match a real engine's, so this is only \
                 valid against the mock worker."
            );
            Ok(PromptBuilder::simple(config.index.block_size))
        }
    }
}

/// Install the tracing subscriber. Safe to call more than once; later calls are
/// no-ops, which keeps integration tests from fighting over the global default.
pub fn init_tracing() {
    use tracing_subscriber::EnvFilter;

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("warmpath=info,warmpath_mock=info"));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true)
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_ids_are_sequential_and_padded() {
        let ids = RequestIds::default();
        assert_eq!(ids.next(), "req_000000000000");
        assert_eq!(ids.next(), "req_000000000001");
    }
}
