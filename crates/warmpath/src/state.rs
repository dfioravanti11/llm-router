//! What each worker is currently doing, and whether it is answering at all.
//!
//! Until now the router judged load by counting its own in-flight requests.
//! That is a proxy for a queue, not a reading of one: it cannot see work the
//! engine has admitted but not started, and it cannot see memory pressure at
//! all. A worker holding a request's whole prefix but with no KV headroom left
//! should lose to one with room, and in-flight counts cannot express that.
//!
//! So each worker's `/metrics` is polled. The names parsed here are vLLM's, so
//! the same code path works against the mock and against a real engine.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use serde::Serialize;

use crate::config::HealthConfig;

/// One worker's last known state.
///
/// Stored as atomics rather than behind a lock: the poller writes each field
/// once a second and the request path reads them constantly, so there is
/// nothing to gain from making readers wait. The fields can be momentarily
/// inconsistent with each other, which does not matter, because every one of
/// them is already a slightly stale approximation of a number that is moving.
#[derive(Debug)]
pub struct WorkerState {
    running: AtomicU32,
    waiting: AtomicU32,
    /// KV utilization in per-mille, so it fits an integer atomic.
    kv_per_mille: AtomicU32,
    healthy: AtomicBool,
    consecutive_failures: AtomicU32,
    consecutive_successes: AtomicU32,
    /// Prefix cache counters as the worker reports them, for the
    /// predicted-versus-actual comparison.
    cache_queries: AtomicU64,
    cache_hits: AtomicU64,
    polls_failed: AtomicU64,
}

impl Default for WorkerState {
    fn default() -> Self {
        Self {
            running: AtomicU32::new(0),
            waiting: AtomicU32::new(0),
            kv_per_mille: AtomicU32::new(0),
            // A worker is assumed healthy until it fails, so a router that
            // starts before its workers do still serves traffic to them.
            healthy: AtomicBool::new(true),
            consecutive_failures: AtomicU32::new(0),
            consecutive_successes: AtomicU32::new(0),
            cache_queries: AtomicU64::new(0),
            cache_hits: AtomicU64::new(0),
            polls_failed: AtomicU64::new(0),
        }
    }
}

impl WorkerState {
    /// Requests the worker has admitted but not finished.
    pub fn queue_depth(&self) -> usize {
        (self.running.load(Ordering::Relaxed) + self.waiting.load(Ordering::Relaxed)) as usize
    }

    pub fn waiting(&self) -> usize {
        self.waiting.load(Ordering::Relaxed) as usize
    }

    /// Fraction of KV cache in use, between zero and one.
    pub fn kv_utilization(&self) -> f64 {
        self.kv_per_mille.load(Ordering::Relaxed) as f64 / 1_000.0
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    pub fn snapshot(&self) -> WorkerSnapshot {
        WorkerSnapshot {
            running: self.running.load(Ordering::Relaxed) as usize,
            waiting: self.waiting.load(Ordering::Relaxed) as usize,
            kv_utilization: self.kv_utilization(),
            healthy: self.is_healthy(),
            cache_queries: self.cache_queries.load(Ordering::Relaxed),
            cache_hits: self.cache_hits.load(Ordering::Relaxed),
            polls_failed: self.polls_failed.load(Ordering::Relaxed),
        }
    }

    /// Record a successful poll.
    fn observe(&self, sample: WorkerSample, config: &HealthConfig) {
        self.running.store(sample.running, Ordering::Relaxed);
        self.waiting.store(sample.waiting, Ordering::Relaxed);
        self.kv_per_mille.store(
            (sample.kv_utilization.clamp(0.0, 1.0) * 1_000.0) as u32,
            Ordering::Relaxed,
        );
        self.cache_queries
            .store(sample.cache_queries, Ordering::Relaxed);
        self.cache_hits.store(sample.cache_hits, Ordering::Relaxed);

        self.consecutive_failures.store(0, Ordering::Relaxed);
        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;

        if !self.is_healthy() && successes >= config.healthy_after {
            self.healthy.store(true, Ordering::Relaxed);
            tracing::info!("worker re-admitted after {successes} successful checks");
        }
    }

    /// Record a failed poll.
    fn observe_failure(&self, config: &HealthConfig) {
        self.polls_failed.fetch_add(1, Ordering::Relaxed);
        self.consecutive_successes.store(0, Ordering::Relaxed);
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;

        if self.is_healthy() && failures >= config.unhealthy_after {
            self.healthy.store(false, Ordering::Relaxed);
            tracing::warn!("worker ejected after {failures} failed checks");
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct WorkerSnapshot {
    pub running: usize,
    pub waiting: usize,
    pub kv_utilization: f64,
    pub healthy: bool,
    pub cache_queries: u64,
    pub cache_hits: u64,
    pub polls_failed: u64,
}

impl WorkerSnapshot {
    /// The worker's own prefix cache hit rate, in blocks.
    ///
    /// This is the number the router does not control, and the one that keeps
    /// it from grading its own homework.
    pub fn hit_rate(&self) -> f64 {
        if self.cache_queries == 0 {
            0.0
        } else {
            self.cache_hits as f64 / self.cache_queries as f64
        }
    }
}

/// One reading of a worker's metrics endpoint.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct WorkerSample {
    pub running: u32,
    pub waiting: u32,
    pub kv_utilization: f64,
    pub cache_queries: u64,
    pub cache_hits: u64,
}

/// Read the handful of metrics the router cares about out of a Prometheus
/// exposition.
///
/// Deliberately not a full Prometheus parser. The router needs five numbers
/// with known names and no labels, and a hand-written scan for those is easier
/// to reason about than a dependency. A metric that is absent leaves its field
/// at zero, because a worker that does not report queue depth should look idle
/// rather than break routing.
pub fn parse_metrics(body: &str) -> WorkerSample {
    let mut sample = WorkerSample::default();

    for line in body.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

        // `name value`, or `name{labels} value`. Labels are skipped rather
        // than parsed: none of the metrics read here carry any.
        let Some((name, value)) = line.rsplit_once(char::is_whitespace) else {
            continue;
        };
        let name = name.split('{').next().unwrap_or(name).trim();
        let Ok(value) = value.trim().parse::<f64>() else {
            continue;
        };

        match name {
            "vllm:num_requests_running" => sample.running = value.max(0.0) as u32,
            "vllm:num_requests_waiting" => sample.waiting = value.max(0.0) as u32,
            "vllm:gpu_cache_usage_perc" => sample.kv_utilization = value,
            "vllm:prefix_cache_queries_total" => sample.cache_queries = value.max(0.0) as u64,
            "vllm:prefix_cache_hits_total" => sample.cache_hits = value.max(0.0) as u64,
            _ => {}
        }
    }

    sample
}

/// Poll every worker's metrics endpoint until the process ends.
///
/// One task for the whole fleet rather than one per worker: the fleet is a
/// handful of replicas, and a single loop is easier to reason about than N
/// tasks racing the same interval.
pub async fn poll_workers(
    client: reqwest::Client,
    endpoints: Vec<String>,
    states: Vec<Arc<WorkerState>>,
    config: HealthConfig,
) {
    let interval = Duration::from_millis(config.poll_interval_ms.max(50));

    // The loop walks the fleet one worker at a time, so without a bound of its
    // own a single slow worker delays every other worker's reading. The client
    // is shared with the proxy, where a read timeout is measured in tens of
    // seconds because a long generation is healthy. A worker that accepts the
    // connection and then says nothing would therefore freeze the router's view
    // of the whole fleet for that long, and routing would run on load figures
    // from a minute ago.
    //
    // One poll interval is the bound, because a reading that arrives later than
    // the next poll was due is worth nothing anyway. A worker that cannot answer
    // in that time counts as a failed poll, and it still takes several in a row
    // to eject it.
    let timeout = interval;

    loop {
        for (endpoint, state) in endpoints.iter().zip(states.iter()) {
            match fetch(&client, endpoint, timeout).await {
                Ok(sample) => state.observe(sample, &config),
                Err(error) => {
                    tracing::debug!(endpoint, error = %error, "worker metrics poll failed");
                    state.observe_failure(&config);
                }
            }
        }
        tokio::time::sleep(interval).await;
    }
}

async fn fetch(
    client: &reqwest::Client,
    endpoint: &str,
    timeout: Duration,
) -> anyhow::Result<WorkerSample> {
    let response = client.get(endpoint).timeout(timeout).send().await?;
    anyhow::ensure!(
        response.status().is_success(),
        "metrics endpoint returned {}",
        response.status()
    );
    Ok(parse_metrics(&response.text().await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    const VLLM_LIKE: &str = "\
# HELP vllm:num_requests_running Requests currently being served
# TYPE vllm:num_requests_running gauge
vllm:num_requests_running 6
# HELP vllm:num_requests_waiting Requests waiting
# TYPE vllm:num_requests_waiting gauge
vllm:num_requests_waiting 4
vllm:gpu_cache_usage_perc 0.75
vllm:prefix_cache_queries_total 1200
vllm:prefix_cache_hits_total 900
some_other_metric 42
";

    fn health() -> HealthConfig {
        HealthConfig {
            poll_interval_ms: 100,
            unhealthy_after: 2,
            healthy_after: 2,
            ..HealthConfig::default()
        }
    }

    /// The poll loop walks the fleet one worker at a time, and it shares the
    /// proxy's HTTP client, whose read timeout is measured in tens of seconds
    /// because a long generation is healthy. A worker that accepts the
    /// connection and then says nothing would freeze every other worker's
    /// reading for that long, and the router would route on load figures from a
    /// minute ago.
    #[tokio::test]
    async fn a_stalled_worker_does_not_freeze_the_rest_of_the_fleet() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        // Accepts connections and never answers. Holding the streams matters,
        // since dropping them would close the socket and end the stall.
        let stalled = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let stalled_addr = stalled.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = stalled.accept().await {
                held.push(stream);
            }
        });

        let answering = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let answering_addr = answering.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = answering.accept().await {
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    let _ = stream.read(&mut request).await;
                    let body = "vllm:num_requests_running 6\nvllm:num_requests_waiting 4\n";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: text/plain\r\ncontent-length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });

        // A read timeout like the proxy's, which is the thing that makes the
        // stall dangerous in the first place.
        let client = reqwest::Client::builder()
            .read_timeout(Duration::from_secs(60))
            .build()
            .unwrap();

        let states = vec![
            Arc::new(WorkerState::default()),
            Arc::new(WorkerState::default()),
        ];

        // The stalled worker is first in the list, so the second one is only
        // ever reached if the stall is bounded.
        tokio::spawn(poll_workers(
            client,
            vec![
                format!("http://{stalled_addr}/metrics"),
                format!("http://{answering_addr}/metrics"),
            ],
            states.clone(),
            health(),
        ));

        tokio::time::sleep(Duration::from_secs(2)).await;

        assert_eq!(
            states[1].queue_depth(),
            10,
            "the answering worker was never read, so the stall blocked the loop"
        );
    }

    #[test]
    fn the_metrics_the_router_needs_are_read() {
        let sample = parse_metrics(VLLM_LIKE);

        assert_eq!(sample.running, 6);
        assert_eq!(sample.waiting, 4);
        assert!((sample.kv_utilization - 0.75).abs() < 1e-9);
        assert_eq!(sample.cache_queries, 1200);
        assert_eq!(sample.cache_hits, 900);
    }

    #[test]
    fn comments_and_unknown_metrics_are_skipped() {
        assert_eq!(
            parse_metrics("# just a comment\nnothing_useful 1\n"),
            WorkerSample::default()
        );
    }

    #[test]
    fn labelled_metrics_are_read() {
        let sample = parse_metrics("vllm:num_requests_running{model=\"qwen\"} 3\n");
        assert_eq!(sample.running, 3);
    }

    #[test]
    fn a_malformed_body_yields_zeros_rather_than_failing() {
        assert_eq!(parse_metrics(""), WorkerSample::default());
        assert_eq!(parse_metrics("garbage\n\n{}\n"), WorkerSample::default());
        assert_eq!(
            parse_metrics("vllm:num_requests_running not_a_number\n"),
            WorkerSample::default()
        );
    }

    #[test]
    fn scientific_notation_parses() {
        let sample = parse_metrics("vllm:prefix_cache_queries_total 1.2e3\n");
        assert_eq!(sample.cache_queries, 1200);
    }

    #[test]
    fn queue_depth_is_running_plus_waiting() {
        let state = WorkerState::default();
        state.observe(parse_metrics(VLLM_LIKE), &health());

        assert_eq!(state.queue_depth(), 10);
        assert_eq!(state.waiting(), 4);
        assert!((state.kv_utilization() - 0.75).abs() < 1e-3);
    }

    #[test]
    fn a_worker_starts_healthy() {
        assert!(WorkerState::default().is_healthy());
    }

    #[test]
    fn a_worker_is_ejected_only_after_repeated_failures() {
        let state = WorkerState::default();
        let config = health();

        state.observe_failure(&config);
        assert!(state.is_healthy(), "one failure should not eject");

        state.observe_failure(&config);
        assert!(!state.is_healthy(), "two failures should eject");
    }

    #[test]
    fn a_single_success_does_not_re_admit_an_ejected_worker() {
        let state = WorkerState::default();
        let config = health();

        state.observe_failure(&config);
        state.observe_failure(&config);
        assert!(!state.is_healthy());

        state.observe(WorkerSample::default(), &config);
        assert!(!state.is_healthy(), "one success should not re-admit");

        state.observe(WorkerSample::default(), &config);
        assert!(state.is_healthy(), "two successes should re-admit");
    }

    #[test]
    fn a_success_resets_the_failure_run() {
        let state = WorkerState::default();
        let config = health();

        state.observe_failure(&config);
        state.observe(WorkerSample::default(), &config);
        state.observe_failure(&config);

        assert!(
            state.is_healthy(),
            "failures either side of a success are not consecutive"
        );
    }

    #[test]
    fn the_snapshot_reports_the_workers_own_hit_rate() {
        let state = WorkerState::default();
        state.observe(parse_metrics(VLLM_LIKE), &health());

        let snapshot = state.snapshot();
        assert!((snapshot.hit_rate() - 0.75).abs() < 1e-9);
        assert!(snapshot.healthy);
    }

    #[test]
    fn a_worker_that_has_never_answered_reports_no_hit_rate() {
        assert_eq!(WorkerState::default().snapshot().hit_rate(), 0.0);
    }
}
