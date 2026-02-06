//! Warmpath: a KV-cache-aware router for LLM inference fleets.
//!
//! R0.2 is a correct streaming proxy with baseline routing policies and
//! Prometheus metrics. There is no prompt builder and no block index yet;
//! those arrive with prefix affinity in R0.3.

pub mod config;
pub mod error;
pub mod metrics;
pub mod proxy;
pub mod worker;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use axum::routing::{get, post};
use axum::Router;

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
    let state = AppState {
        pool: Arc::new(WorkerPool::new(config, &metrics)?),
        metrics,
        request_ids: Arc::new(RequestIds::default()),
        max_request_bytes: config.server.max_request_bytes,
    };

    Ok(Router::new()
        .route("/health", get(proxy::health))
        .route("/metrics", get(proxy::metrics))
        .route("/v1/chat/completions", post(proxy::proxy))
        .route("/v1/completions", post(proxy::proxy))
        .with_state(state))
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
