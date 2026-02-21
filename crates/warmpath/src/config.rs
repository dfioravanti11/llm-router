//! Router configuration.
//!
//! One TOML file holds everything. Fields that will be needed by later
//! releases are deliberately absent until the code that reads them exists.
//!
//! Every section falls back to its defaults field by field, so a config can
//! override one value without restating the section around it. Unknown keys
//! are still rejected, which is what catches a typo before it silently becomes
//! a default.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
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
    #[serde(default)]
    pub routing: RoutingConfig,
    #[serde(default)]
    pub index: IndexConfig,
    #[serde(default)]
    pub model: ModelConfig,
    #[serde(default)]
    pub health: HealthConfig,
    pub workers: Vec<WorkerConfig>,
}

/// Polling the workers, and deciding when one has stopped answering.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct HealthConfig {
    /// How often each worker's metrics endpoint is read. This is the router's
    /// only view of queue depth and KV pressure, so a long interval means
    /// routing on stale load.
    pub poll_interval_ms: u64,
    /// Path appended to a worker's base URL to reach its metrics.
    pub metrics_path: String,
    /// Consecutive failed polls before a worker stops receiving traffic. More
    /// than one, so a single dropped packet does not eject a healthy worker.
    pub unhealthy_after: u32,
    /// Consecutive successful polls before an ejected worker is used again.
    pub healthy_after: u32,
    /// Retry a request once on a different worker when the first never
    /// answered. Only safe before anything has streamed, which is why it
    /// applies to connection failures alone.
    pub retry_on_dispatch_failure: bool,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            poll_interval_ms: 500,
            metrics_path: "/metrics".to_string(),
            unhealthy_after: 3,
            healthy_after: 2,
            retry_on_dispatch_failure: true,
        }
    }
}

/// Where the router gets its tokenizer and chat template.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct ModelConfig {
    /// Directory holding `tokenizer.json` and `tokenizer_config.json`, as
    /// downloaded from the Hugging Face hub.
    ///
    /// Leave unset to use a deterministic development tokenizer. That is only
    /// valid against the mock worker: against a real engine the router would be
    /// cutting blocks at different boundaries than the worker, and the symptom
    /// is a mediocre hit rate rather than an error.
    ///
    /// When this *is* set and the files cannot be loaded, startup fails. A
    /// silent fall back to the development tokenizer is precisely the failure
    /// this setting exists to prevent.
    pub directory: Option<PathBuf>,
}

/// Which worker a request goes to.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Always the first configured worker. Useful when a run must pin all
    /// traffic to one worker.
    First,
    /// Even rotation over the configured workers, ignoring load and cache
    /// state. This is the baseline every later policy is measured against.
    #[default]
    RoundRobin,
    /// Fewest requests in flight at the worker wins. A cache-blind baseline
    /// that reacts to load, which round-robin does not.
    LeastLoaded,
    /// Two workers at random, the less loaded one wins. Cheaper than scanning
    /// the fleet and famously close to the same result.
    PowerOfTwo,
    /// Longest prefix match wins, with no regard for load. The naive form,
    /// kept honest so a skewed workload can show it hotspotting.
    PrefixAffinity,
    /// Prefix match, load, and fleet balance together.
    PrefixAffinityBalanced,
}

impl Policy {
    /// Stable name used as a metric label and in run manifests.
    pub fn as_str(self) -> &'static str {
        match self {
            Policy::First => "first",
            Policy::RoundRobin => "round-robin",
            Policy::LeastLoaded => "least-loaded",
            Policy::PowerOfTwo => "power-of-two",
            Policy::PrefixAffinity => "prefix-affinity",
            Policy::PrefixAffinityBalanced => "prefix-affinity-balanced",
        }
    }

    /// Whether this policy reads the block index.
    ///
    /// Prompt building costs a render, a tokenize, and a hash chain per
    /// request. A baseline run must not pay it, or the baseline is measuring
    /// the router's overhead as well as its routing.
    pub fn needs_prompt_fingerprint(self) -> bool {
        matches!(
            self,
            Policy::PrefixAffinity | Policy::PrefixAffinityBalanced
        )
    }
}

fn default_session_capacity() -> usize {
    100_000
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct RoutingConfig {
    #[serde(default)]
    pub policy: Policy,
    #[serde(default)]
    pub affinity: AffinityConfig,
    /// Sessions remembered for affinity. Session ids come from clients, so
    /// this is bounded on purpose: without a limit a client could grow the map
    /// without end. Zero disables session affinity.
    #[serde(default = "default_session_capacity")]
    pub session_capacity: usize,
}

impl Default for RoutingConfig {
    fn default() -> Self {
        Self {
            policy: Policy::default(),
            affinity: AffinityConfig::default(),
            session_capacity: default_session_capacity(),
        }
    }
}

/// Thresholds for the two prefix-affinity policies.
///
/// Every field falls back to its default, so a config can override one
/// threshold without restating the rest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AffinityConfig {
    /// Smallest fraction of a prompt's blocks worth routing on. Below this a
    /// match is treated as no match, so a trivial overlap cannot drag a
    /// request onto a worker for nothing.
    pub cache_threshold: f64,
    /// In-flight requests the busiest worker must exceed before the fleet can
    /// be called imbalanced. Stops one-against-zero from looking like a crisis.
    pub balance_abs_threshold: usize,
    /// How many times the least loaded worker's depth the busiest may reach
    /// before affinity yields.
    pub balance_rel_threshold: f64,
    /// How much of the balanced score comes from headroom rather than cache
    /// locality. Zero makes the balanced policy pick the longest match; one
    /// makes it ignore the cache.
    pub load_weight: f64,
}

impl Default for AffinityConfig {
    fn default() -> Self {
        Self {
            cache_threshold: 0.2,
            balance_abs_threshold: 8,
            balance_rel_threshold: 2.0,
            load_weight: 0.3,
        }
    }
}

/// The block index and the prompt building that feeds it.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct IndexConfig {
    /// Token ids per block. Must match the worker's, or nothing lines up.
    pub block_size: usize,
    /// Committed blocks one worker may hold before eviction starts. This is a
    /// model of the engine's capacity, not a reading of it.
    pub block_budget: usize,
}

impl Default for IndexConfig {
    fn default() -> Self {
        Self {
            block_size: warmpath_core::DEFAULT_BLOCK_SIZE,
            block_budget: 65_536,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields, default)]
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
#[serde(deny_unknown_fields, default)]
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

        if self.workers.len() > crate::index::MAX_WORKERS {
            bail!(
                "at most {} workers are supported, got {}",
                crate::index::MAX_WORKERS,
                self.workers.len()
            );
        }

        if self.index.block_size == 0 {
            bail!("index.block_size must be positive");
        }
        if self.index.block_budget == 0 {
            bail!("index.block_budget must be positive");
        }

        let affinity = &self.routing.affinity;
        if !(0.0..=1.0).contains(&affinity.cache_threshold) {
            bail!(
                "routing.affinity.cache_threshold must be between 0 and 1, got {}",
                affinity.cache_threshold
            );
        }
        if !(0.0..=1.0).contains(&affinity.load_weight) {
            bail!(
                "routing.affinity.load_weight must be between 0 and 1, got {}",
                affinity.load_weight
            );
        }
        if self.health.unhealthy_after == 0 || self.health.healthy_after == 0 {
            bail!("health.unhealthy_after and health.healthy_after must be positive");
        }
        if !self.health.metrics_path.starts_with('/') {
            bail!(
                "health.metrics_path must start with /, got `{}`",
                self.health.metrics_path
            );
        }

        if affinity.balance_rel_threshold < 1.0 {
            bail!(
                "routing.affinity.balance_rel_threshold must be at least 1, got {}",
                affinity.balance_rel_threshold
            );
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
    fn a_section_can_override_one_field_without_restating_the_rest() {
        let config = config_from(
            r#"
            [server]
            bind = "127.0.0.1:9999"

            [index]
            block_budget = 128

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect("a partial section should parse");

        assert_eq!(config.server.bind.port(), 9999);
        assert_eq!(
            config.server.max_request_bytes,
            ServerConfig::default().max_request_bytes
        );
        assert_eq!(config.index.block_budget, 128);
        assert_eq!(config.index.block_size, 16);
        assert_eq!(config.upstream.connect_timeout_ms, 2_000);
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
    fn affinity_policies_are_selectable_and_thresholds_default() {
        let config = config_from(
            r#"
            [routing]
            policy = "prefix-affinity-balanced"

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect("config should parse");

        assert_eq!(config.routing.policy, Policy::PrefixAffinityBalanced);
        assert!(config.routing.policy.needs_prompt_fingerprint());
        assert_eq!(config.routing.affinity.cache_threshold, 0.2);
        assert_eq!(config.index.block_size, 16);
    }

    #[test]
    fn baseline_policies_do_not_ask_for_a_prompt_fingerprint() {
        for policy in [
            Policy::RoundRobin,
            Policy::First,
            Policy::LeastLoaded,
            Policy::PowerOfTwo,
        ] {
            assert!(
                !policy.needs_prompt_fingerprint(),
                "{policy:?} should not pay for prompt building"
            );
        }
        assert!(Policy::PrefixAffinity.needs_prompt_fingerprint());
        assert!(Policy::PrefixAffinityBalanced.needs_prompt_fingerprint());
    }

    #[test]
    fn every_policy_has_a_distinct_stable_name() {
        let names: Vec<&str> = [
            Policy::First,
            Policy::RoundRobin,
            Policy::LeastLoaded,
            Policy::PowerOfTwo,
            Policy::PrefixAffinity,
            Policy::PrefixAffinityBalanced,
        ]
        .iter()
        .map(|policy| policy.as_str())
        .collect();

        let unique: std::collections::HashSet<_> = names.iter().collect();
        assert_eq!(unique.len(), names.len(), "{names:?}");
    }

    #[test]
    fn health_settings_are_validated() {
        let err = config_from(
            r#"
            [health]
            unhealthy_after = 0

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect_err("zero should be rejected");
        assert!(err.to_string().contains("unhealthy_after"), "got: {err}");

        let err = config_from(
            r#"
            [health]
            metrics_path = "metrics"

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect_err("a relative path should be rejected");
        assert!(err.to_string().contains("metrics_path"), "got: {err}");
    }

    #[test]
    fn out_of_range_thresholds_are_rejected() {
        for (key, value) in [
            ("cache_threshold", "1.5"),
            ("load_weight", "-0.1"),
            ("balance_rel_threshold", "0.5"),
        ] {
            let mut affinity = AffinityConfig::default();
            match key {
                "cache_threshold" => affinity.cache_threshold = value.parse().unwrap(),
                "load_weight" => affinity.load_weight = value.parse().unwrap(),
                _ => affinity.balance_rel_threshold = value.parse().unwrap(),
            }
            let err = config_from(&format!(
                r#"
                [routing.affinity]
                cache_threshold = {}
                balance_abs_threshold = {}
                balance_rel_threshold = {}
                load_weight = {}

                [[workers]]
                name = "w0"
                url = "http://127.0.0.1:8001"
                "#,
                affinity.cache_threshold,
                affinity.balance_abs_threshold,
                affinity.balance_rel_threshold,
                affinity.load_weight
            ))
            .expect_err("out of range value should fail");

            assert!(err.to_string().contains(key), "got: {err}");
        }
    }

    #[test]
    fn a_zero_block_size_is_rejected() {
        let err = config_from(
            r#"
            [index]
            block_size = 0

            [[workers]]
            name = "w0"
            url = "http://127.0.0.1:8001"
            "#,
        )
        .expect_err("zero block size should fail");

        assert!(err.to_string().contains("block_size"), "got: {err}");
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
