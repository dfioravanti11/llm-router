//! Worker pool and routing policy.
//!
//! R0.2 tracks identity, a shared HTTP client, and the two baseline policies.
//! Health state, queue depth, KV pressure, and circuit-breaker state land in
//! R0.4 and R0.6.

use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::Context;

use crate::config::{Config, Policy, WorkerConfig};
use crate::metrics::{Metrics, WorkerMetrics};

#[derive(Debug, Clone)]
pub struct Worker {
    pub name: String,
    /// Base URL with any trailing slash removed, so joining a path is a
    /// concatenation rather than a URL-resolution question.
    base_url: String,
}

impl Worker {
    fn new(config: &WorkerConfig) -> Self {
        Self {
            name: config.name.clone(),
            base_url: config.url.trim_end_matches('/').to_string(),
        }
    }

    /// Absolute URL for a path-and-query taken verbatim from the client request.
    pub fn endpoint(&self, path_and_query: &str) -> String {
        format!("{}{}", self.base_url, path_and_query)
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }
}

/// The outcome of one routing decision: which worker, and the metric handles
/// already resolved for it.
pub struct Choice<'a> {
    pub worker: &'a Worker,
    pub metrics: &'a WorkerMetrics,
}

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Worker>,
    /// Metric handles, indexed in step with `workers`.
    worker_metrics: Vec<WorkerMetrics>,
    client: reqwest::Client,
    policy: Policy,
    /// Rotation cursor for `round-robin`.
    cursor: AtomicUsize,
}

impl WorkerPool {
    pub fn new(config: &Config, metrics: &Metrics) -> anyhow::Result<Self> {
        config.validate()?;

        let client = reqwest::Client::builder()
            .connect_timeout(config.upstream.connect_timeout())
            .read_timeout(config.upstream.read_timeout())
            // No compression features are enabled on the reqwest dependency, so
            // response bodies reach us exactly as the worker wrote them. That is
            // what makes byte-identical passthrough possible.
            .build()
            .context("failed to build the upstream HTTP client")?;

        let policy = config.routing.policy;
        let workers: Vec<Worker> = config.workers.iter().map(Worker::new).collect();
        let worker_metrics = workers
            .iter()
            .map(|worker| metrics.for_worker(&worker.name, policy.as_str()))
            .collect();

        Ok(Self {
            workers,
            worker_metrics,
            client,
            policy,
            cursor: AtomicUsize::new(0),
        })
    }

    /// Choose the worker for a request.
    ///
    /// Neither policy here looks at cache state or load, which is the point:
    /// they are the baselines R0.3 has to beat. `WorkerPool::new` rejects an
    /// empty worker list, so the index is always in range.
    pub fn pick(&self) -> Choice<'_> {
        let index = match self.policy {
            Policy::First => 0,
            // Relaxed ordering is enough: the counter only has to advance, and
            // no other memory is published through it.
            Policy::RoundRobin => self.cursor.fetch_add(1, Ordering::Relaxed) % self.workers.len(),
        };

        Choice {
            worker: &self.workers[index],
            metrics: &self.worker_metrics[index],
        }
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }

    pub fn policy(&self) -> Policy {
        self.policy
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RoutingConfig, ServerConfig, UpstreamConfig};

    fn pool(policy: Policy, worker_count: usize) -> WorkerPool {
        let config = Config {
            server: ServerConfig::default(),
            upstream: UpstreamConfig::default(),
            routing: RoutingConfig { policy },
            workers: (0..worker_count)
                .map(|index| WorkerConfig {
                    name: format!("w{index}"),
                    url: format!("http://127.0.0.1:{}", 8001 + index),
                })
                .collect(),
        };
        WorkerPool::new(&config, &Metrics::new()).expect("pool should build")
    }

    #[test]
    fn endpoint_joins_without_doubling_slashes() {
        let worker = Worker::new(&WorkerConfig {
            name: "w0".to_string(),
            url: "http://127.0.0.1:8001/".to_string(),
        });

        assert_eq!(
            worker.endpoint("/v1/chat/completions"),
            "http://127.0.0.1:8001/v1/chat/completions"
        );
    }

    #[test]
    fn endpoint_preserves_the_query_string() {
        let worker = Worker::new(&WorkerConfig {
            name: "w0".to_string(),
            url: "http://127.0.0.1:8001".to_string(),
        });

        assert_eq!(
            worker.endpoint("/v1/completions?trace=1"),
            "http://127.0.0.1:8001/v1/completions?trace=1"
        );
    }

    #[test]
    fn first_policy_pins_every_request_to_one_worker() {
        let pool = pool(Policy::First, 3);
        for _ in 0..10 {
            assert_eq!(pool.pick().worker.name, "w0");
        }
    }

    #[test]
    fn round_robin_rotates_evenly_and_wraps() {
        let pool = pool(Policy::RoundRobin, 3);
        let picked: Vec<String> = (0..7).map(|_| pool.pick().worker.name.clone()).collect();

        assert_eq!(picked, ["w0", "w1", "w2", "w0", "w1", "w2", "w0"]);
    }

    #[test]
    fn round_robin_over_one_worker_is_that_worker() {
        let pool = pool(Policy::RoundRobin, 1);
        for _ in 0..5 {
            assert_eq!(pool.pick().worker.name, "w0");
        }
    }
}
