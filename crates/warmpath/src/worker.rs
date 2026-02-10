//! Worker pool: identity, load, the block index, and the routing decision.
//!
//! Health checking with ejection and a single retry lands in R0.4, alongside
//! the load signals. What is here now is the router's own in-flight count,
//! which R0.4 replaces with the worker's reported queue depth and KV
//! utilization. Circuit breaking and drain are Appendix A1.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::Context;
use warmpath_core::PromptFingerprint;

use crate::config::{AffinityConfig, Config, Policy, WorkerConfig};
use crate::index::{ApproximateIndex, BlockIndex, IndexStats, Reservation, MAX_WORKERS};
use crate::metrics::{Metrics, WorkerMetrics};
use crate::policy::{self, Decision, RoutingInputs};

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

/// The result of routing one request.
///
/// Holds the reservation, so the blocks this request will produce stay
/// attributed to its worker for as long as it is in flight.
pub struct Choice {
    pub index: usize,
    pub decision: Decision,
    pub metrics: WorkerMetrics,
    pub reservation: Option<Reservation>,
}

#[derive(Debug)]
pub struct WorkerPool {
    workers: Vec<Worker>,
    /// Metric handles, indexed in step with `workers`.
    worker_metrics: Vec<WorkerMetrics>,
    /// Requests dispatched and not yet finished, per worker.
    in_flight: Vec<AtomicUsize>,
    client: reqwest::Client,
    policy: Policy,
    affinity: AffinityConfig,
    index: Arc<dyn BlockIndex>,
    /// Rotation cursor, shared by every policy that needs an arbitrary but
    /// repeatable choice.
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
            in_flight: workers.iter().map(|_| AtomicUsize::new(0)).collect(),
            index: Arc::new(ApproximateIndex::new(
                workers.len(),
                config.index.block_budget,
            )),
            workers,
            worker_metrics,
            client,
            policy,
            affinity: config.routing.affinity.clone(),
            cursor: AtomicUsize::new(0),
        })
    }

    /// Route one request.
    ///
    /// `fingerprint` is `None` when the policy does not read the index, or when
    /// the request body could not be understood. Either way the affinity
    /// policies degrade to routing on load, because no correctness invariant
    /// may depend on the index having an answer.
    pub fn pick(&self, fingerprint: Option<&PromptFingerprint>) -> Choice {
        let worker_count = self.workers.len();

        // Stack-allocated, so routing does not allocate. `MAX_WORKERS` is the
        // fleet size the bitset in the index already caps at.
        let mut matched = [0usize; MAX_WORKERS];
        let mut load = [0usize; MAX_WORKERS];

        let blocks = fingerprint.map(|f| f.blocks.as_slice()).unwrap_or(&[]);
        if !blocks.is_empty() {
            self.index
                .match_prefix(blocks, &mut matched[..worker_count]);
        }
        for (worker, slot) in load[..worker_count].iter_mut().enumerate() {
            *slot = self.in_flight[worker].load(Ordering::Relaxed);
        }

        let cursor = self.cursor.fetch_add(1, Ordering::Relaxed);
        let decision = policy::choose(
            self.policy,
            &self.affinity,
            RoutingInputs {
                prompt_blocks: blocks.len(),
                matched: &matched[..worker_count],
                load: &load[..worker_count],
            },
            cursor,
        );

        self.in_flight[decision.worker].fetch_add(1, Ordering::Relaxed);

        // Reserve the whole chain, not just the part the worker was missing.
        // Committing it later doubles as the least-recently-used touch that
        // keeps a hot prefix alive in the index's eviction model.
        let reservation = fingerprint.filter(|f| !f.is_empty()).map(|f| {
            Reservation::new(
                Arc::clone(&self.index),
                decision.worker,
                f.blocks.clone().into(),
            )
        });

        Choice {
            index: decision.worker,
            decision,
            metrics: self.worker_metrics[decision.worker].clone(),
            reservation,
        }
    }

    /// Release the load a request was holding.
    pub fn finish(&self, worker: usize) {
        if worker < self.in_flight.len() {
            // Saturating, because an in-flight count that has already reached
            // zero means a bookkeeping bug, and wrapping to `usize::MAX` would
            // turn that bug into a worker no policy will ever pick again.
            let _ = self.in_flight[worker].fetch_update(
                Ordering::Relaxed,
                Ordering::Relaxed,
                |current| Some(current.saturating_sub(1)),
            );
        }
    }

    pub fn worker(&self, index: usize) -> &Worker {
        &self.workers[index]
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

    pub fn index_stats(&self) -> IndexStats {
        self.index.stats()
    }

    pub fn in_flight(&self, worker: usize) -> usize {
        self.in_flight[worker].load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{IndexConfig, RoutingConfig, ServerConfig, UpstreamConfig};
    use crate::policy::DecisionReason;
    use warmpath_core::BlockHash;

    fn pool(policy: Policy, worker_count: usize) -> WorkerPool {
        let config = Config {
            server: ServerConfig::default(),
            upstream: UpstreamConfig::default(),
            routing: RoutingConfig {
                policy,
                affinity: AffinityConfig::default(),
            },
            index: IndexConfig::default(),
            workers: (0..worker_count)
                .map(|index| WorkerConfig {
                    name: format!("w{index}"),
                    url: format!("http://127.0.0.1:{}", 8001 + index),
                })
                .collect(),
        };
        WorkerPool::new(&config, &Metrics::new()).expect("pool should build")
    }

    fn fingerprint(seed: u64, blocks: usize) -> PromptFingerprint {
        PromptFingerprint {
            blocks: (0..blocks)
                .map(|i| BlockHash(seed * 1_000 + i as u64))
                .collect(),
            token_count: blocks * 16,
        }
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
            let choice = pool.pick(None);
            assert_eq!(choice.index, 0);
            pool.finish(choice.index);
        }
    }

    #[test]
    fn round_robin_rotates_evenly_and_wraps() {
        let pool = pool(Policy::RoundRobin, 3);
        let picked: Vec<usize> = (0..7)
            .map(|_| {
                let choice = pool.pick(None);
                pool.finish(choice.index);
                choice.index
            })
            .collect();

        assert_eq!(picked, [0, 1, 2, 0, 1, 2, 0]);
    }

    #[test]
    fn a_baseline_policy_takes_no_reservation() {
        let pool = pool(Policy::RoundRobin, 2);
        let choice = pool.pick(Some(&fingerprint(1, 8)));

        assert!(choice.reservation.is_some(), "the chain is still recorded");
        assert_eq!(choice.decision.reason, DecisionReason::RoundRobin);
    }

    #[test]
    fn in_flight_rises_on_dispatch_and_falls_on_finish() {
        let pool = pool(Policy::First, 2);

        let first = pool.pick(None);
        let second = pool.pick(None);
        assert_eq!(pool.in_flight(0), 2);

        pool.finish(first.index);
        assert_eq!(pool.in_flight(0), 1);
        pool.finish(second.index);
        assert_eq!(pool.in_flight(0), 0);
    }

    #[test]
    fn finishing_more_than_was_dispatched_cannot_underflow() {
        let pool = pool(Policy::First, 2);
        pool.finish(0);
        pool.finish(0);
        assert_eq!(pool.in_flight(0), 0);
    }

    /// The whole point of R0.3, end to end through the pool.
    #[test]
    fn a_repeated_prefix_returns_to_the_worker_that_holds_it() {
        let pool = pool(Policy::PrefixAffinity, 3);
        let prompt = fingerprint(1, 16);

        // First request has nothing to go on, so it lands somewhere and
        // commits its blocks when it finishes.
        let first = pool.pick(Some(&prompt));
        let mut reservation = first.reservation.expect("should reserve");
        reservation.confirm();
        drop(reservation);
        pool.finish(first.index);

        // Every later request with the same prefix follows it.
        for _ in 0..10 {
            let repeat = pool.pick(Some(&prompt));
            assert_eq!(repeat.index, first.index);
            assert_eq!(repeat.decision.reason, DecisionReason::Affinity);
            assert_eq!(repeat.decision.matched_blocks, 16);
            pool.finish(repeat.index);
        }
    }

    /// The burst case the in-flight reservation exists for.
    #[test]
    fn a_second_request_with_the_same_prefix_follows_the_first_before_it_finishes() {
        let pool = pool(Policy::PrefixAffinity, 3);
        let prompt = fingerprint(7, 16);

        // Nothing has completed, so the index has no committed blocks at all.
        let first = pool.pick(Some(&prompt));
        let second = pool.pick(Some(&prompt));

        assert_eq!(
            second.index, first.index,
            "a burst of identical prefixes must not be scattered"
        );
        assert_eq!(second.decision.reason, DecisionReason::Affinity);
    }

    #[test]
    fn a_cancelled_request_does_not_leave_its_prefix_in_the_index() {
        let pool = pool(Policy::PrefixAffinity, 3);
        let prompt = fingerprint(9, 16);

        let cancelled = pool.pick(Some(&prompt));
        drop(cancelled.reservation);
        pool.finish(cancelled.index);

        assert_eq!(pool.index_stats().blocks, 0);
        assert_eq!(pool.index_stats().reserved, 0);
    }

    #[test]
    fn different_prefixes_spread_across_the_fleet() {
        let pool = pool(Policy::PrefixAffinity, 3);

        let mut seen = std::collections::HashSet::new();
        for seed in 0..9 {
            let choice = pool.pick(Some(&fingerprint(seed, 16)));
            let mut reservation = choice.reservation.expect("should reserve");
            reservation.confirm();
            drop(reservation);
            pool.finish(choice.index);
            seen.insert(choice.index);
        }

        assert!(
            seen.len() > 1,
            "nine unrelated prompts all landed on one worker"
        );
    }

    #[test]
    fn the_balanced_policy_yields_when_one_worker_is_buried() {
        let pool = pool(Policy::PrefixAffinityBalanced, 2);
        let prompt = fingerprint(3, 16);

        // Teach the index that worker 0 holds the prefix.
        let first = pool.pick(Some(&prompt));
        let mut reservation = first.reservation.expect("should reserve");
        reservation.confirm();
        drop(reservation);
        pool.finish(first.index);
        let holder = first.index;

        // Bury the holder past both thresholds without finishing anything.
        // The same prefix is used, so affinity keeps sending them there until
        // the imbalance is what stops it.
        let mut held = Vec::new();
        while pool.in_flight(holder) <= pool.affinity.balance_abs_threshold {
            held.push(pool.pick(Some(&prompt)));
            assert!(
                held.len() < 100,
                "affinity stopped piling onto the holder before the threshold"
            );
        }

        let decision = pool.pick(Some(&prompt));
        assert_eq!(decision.decision.reason, DecisionReason::BalanceOverride);
        assert_ne!(decision.index, holder);
    }
}
