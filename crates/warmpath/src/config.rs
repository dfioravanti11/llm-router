//! Router configuration.
//!
//! One TOML file holds everything. Fields that will be needed by later
//! releases (policy selection, index backend, thresholds) are deliberately
//! absent until the code that reads them exists.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context};
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub server: ServerConfig,
    #[serde(default)]
    pub upstream: UpstreamConfig,
    pub workers: Vec<WorkerConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerConfig {
    /// Address the router listens on.
    pub bind: SocketAddr,
    /// Largest request body the router will buffer. Bodies are buffered
    /// because R0.3 needs the full prompt to compute block hashes.
    pub max_request_bytes: usize,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            bind: "0.0.0.0:8080".parse().expect("valid default bind address"),
            max_request_bytes: 4 * 1024 * 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpstreamConfig {
    /// Timeout for establishing a connection to a worker.
    pub connect_timeout_ms: u64,
    /// Maximum gap between bytes on a streamed response. This is a read
    /// timeout rather than a total-request timeout: a long generation is
    /// healthy, a stalled one is not.
    pub read_timeout_ms: u64,
}

impl Default for UpstreamConfig {
    fn default() -> Self {
        Self {
            connect_timeout_ms: 2_000,
            read_timeout_ms: 60_000,
        }
    }
}

impl UpstreamConfig {
    pub fn connect_timeout(&self) -> Duration {
        Duration::from_millis(self.connect_timeout_ms)
    }

    pub fn read_timeout(&self) -> Duration {
        Duration::from_millis(self.read_timeout_ms)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerConfig {
    /// Label used in logs and metrics. Must be unique.
    pub name: String,
    /// Worker base URL, for example `http://127.0.0.1:8001`.
    pub url: String,
}

impl Config {
    /// Read and validate a config file.
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read config file {}", path.display()))?;
        let config: Config = toml::from_str(&text)
            .with_context(|| format!("failed to parse config file {}", path.display()))?;
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        if self.workers.is_empty() {
            bail!("config must define at least one worker");
        }

        let mut seen = Vec::with_capacity(self.workers.len());
        for worker in &self.workers {
            if worker.name.is_empty() {
                bail!("worker names must not be empty");
            }
            if seen.contains(&worker.name.as_str()) {
                bail!("duplicate worker name `{}`", worker.name);
            }
            seen.push(worker.name.as_str());

            if !worker.url.starts_with("http://") && !worker.url.starts_with("https://") {
                bail!(
                    "worker `{}` url must start with http:// or https://, got `{}`",
                    worker.name,
                    worker.url
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from(text: &str) -> anyhow::Result<Config> {
        let config: Config = toml::from_str(text)?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn defaults_apply_when_sections_are_omitted() {
        let config = config_from(
            r#"
            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect("config should parse");

        assert_eq!(config.server.bind.port(), 8080);
        assert_eq!(config.upstream.connect_timeout_ms, 2_000);
        assert_eq!(config.workers.len(), 1);
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = config_from(
            r#"
            [server]
            bind = "0.0.0.0:8080"
            max_request_bytes = 1024
            typo_here = true

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect_err("unknown key should fail");

        assert!(err.to_string().contains("typo_here"), "got: {err}");
    }

    #[test]
    fn duplicate_worker_names_are_rejected() {
        let err = config_from(
            r#"
            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8002"
            "#,
        )
        .expect_err("duplicate name should fail");

        assert!(
            err.to_string().contains("duplicate worker name"),
            "got: {err}"
        );
    }

    #[test]
    fn worker_url_scheme_is_checked() {
        let err = config_from(
            r#"
            [[workers]]
            name = "w0"
            url = "127.0.0.1:8001"
            "#,
        )
        .expect_err("missing scheme should fail");

        assert!(err.to_string().contains("http://"), "got: {err}");
    }
}
