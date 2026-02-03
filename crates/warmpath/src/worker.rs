//! Worker pool.
//!
//! R0.1 tracks only identity and a shared HTTP client. Health state, queue
//! depth, KV pressure, and circuit-breaker state land in R0.4 and R0.6.

use anyhow::Context;

use crate::config::{Config, WorkerConfig};

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

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Worker>,
    client: reqwest::Client,
}

impl WorkerPool {
    pub fn new(config: &Config) -> anyhow::Result<Self> {
        config.validate()?;

        let client = reqwest::Client::builder()
            .connect_timeout(config.upstream.connect_timeout())
            .read_timeout(config.upstream.read_timeout())
            // No compression features are enabled on the reqwest dependency, so
            // response bodies reach us exactly as the worker wrote them. That is
            // what makes byte-identical passthrough possible.
            .build()
            .context("failed to build the upstream HTTP client")?;

        Ok(Self {
            workers: config.workers.iter().map(Worker::new).collect(),
            client,
        })
    }

    /// Choose the worker for a request.
    ///
    /// R0.1 has no policy engine, so the first configured worker always wins.
    /// The method exists so the seam is in place for R0.3 and so the proxy path
    /// never grows a hardcoded index. `WorkerPool::new` rejects an empty worker
    /// list, so the slice is never empty here.
    pub fn pick(&self) -> &Worker {
        &self.workers[0]
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn workers(&self) -> &[Worker] {
        &self.workers
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
