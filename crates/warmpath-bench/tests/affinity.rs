//! R0.3's exit criterion, as a test.
//!
//! Prefix-affinity routing has to beat round-robin on a workload with prefix
//! reuse, and the improvement has to show up in the worker's own cache
//! counters rather than only in the router's opinion of itself.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use warmpath::config::{
    AffinityConfig, Config, IndexConfig, Policy, RoutingConfig, ServerConfig, UpstreamConfig,
    WorkerConfig,
};
use warmpath_bench::record::{Mode, RunConfig};
use warmpath_bench::{run_once, RunReport};
use warmpath_mock::{MockConfig, MockState};

/// Blocks each worker can cache.
///
/// Sizing this against the workload is the whole experiment. A shared prefix of
/// 256 words is about 16 blocks, so a pool of ten prefixes is a working set of
/// roughly 160 blocks. At 64 blocks per worker no single worker can hold the
/// working set, but three of them together can. That is the regime where
/// a request goes actually decides whether it hits.
const CACHE_BLOCKS: usize = 64;
const BLOCK_SIZE: usize = 16;
/// Words in each shared prefix, about 16 blocks once rendered.
const PREFIX_WORDS: usize = 256;

struct Fleet {
    router: SocketAddr,
    workers: Vec<MockState>,
}

impl Fleet {
    /// Total prefix cache hit rate across every worker, in blocks.
    ///
    /// This is the worker's own counter, in the shape vLLM reports it. The
    /// router's predicted hit rate is a separate number, and comparing the two
    /// is what stops the router grading its own homework.
    async fn observed_hit_rate(&self) -> f64 {
        let mut queries = 0u64;
        let mut hits = 0u64;
        for worker in &self.workers {
            let stats = worker.cache_stats().await;
            queries += stats.prefix_cache_queries;
            hits += stats.prefix_cache_hits;
        }
        if queries == 0 {
            0.0
        } else {
            hits as f64 / queries as f64
        }
    }
}

async fn spawn_fleet(policy: Policy, worker_count: usize) -> Fleet {
    let mut workers = Vec::with_capacity(worker_count);
    let mut addresses = Vec::with_capacity(worker_count);

    for _ in 0..worker_count {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock worker should bind");
        addresses.push(listener.local_addr().expect("addr"));

        let state = MockState::new(MockConfig {
            time_to_first_token: Duration::from_millis(1),
            inter_token_delay: Duration::from_micros(200),
            max_concurrency: 32,
            cache_blocks: CACHE_BLOCKS,
            block_size: BLOCK_SIZE,
            // Generous enough that a cache miss is unmistakable in the latency,
            // which is the point of measuring against a mock at all.
            prefill_per_token: Duration::from_micros(120),
            ..MockConfig::default()
        });
        workers.push(state.clone());

        tokio::spawn(async move {
            let _ = axum::serve(listener, warmpath_mock::router(state)).await;
        });
    }

    let config = Config {
        server: ServerConfig::default(),
        upstream: UpstreamConfig::default(),
        routing: RoutingConfig {
            policy,
            affinity: AffinityConfig::default(),
        },
        index: IndexConfig {
            block_size: BLOCK_SIZE,
            // The router's model of the worker's capacity. It matches here
            // because the mock's capacity is known; against a real engine it
            // is a guess, and R0.5 measures what that guess costs.
            block_budget: CACHE_BLOCKS,
        },
        workers: addresses
            .iter()
            .enumerate()
            .map(|(index, addr)| WorkerConfig {
                name: format!("w{index}"),
                url: format!("http://{addr}"),
            })
            .collect(),
    };

    let app = warmpath::router(&config).expect("router should build");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("router should bind");
    let router = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    Fleet { router, workers }
}

/// A workload with real prefix reuse: a pool of long shared prefixes, each
/// request adding its own short question.
fn workload(target: SocketAddr, label: &str, prefix_pool: usize) -> RunConfig {
    RunConfig {
        target: format!("http://{target}"),
        endpoint: "/v1/chat/completions".to_string(),
        model: "mock-model".to_string(),
        mode: Mode::OpenLoop,
        rate_per_second: 60.0,
        concurrency: 4,
        duration_secs: 3.0,
        warmup_secs: 0.75,
        seed: 4242,
        label: label.to_string(),
        prompt_words: 24,
        shared_prefix_words: PREFIX_WORDS,
        prefix_pool,
        max_tokens: 4,
        stream: true,
        max_dispatch_lag_ms: 100.0,
    }
}

async fn measure(policy: Policy, label: &str, prefix_pool: usize) -> (RunReport, f64) {
    let fleet = spawn_fleet(policy, 3).await;
    let (report, _) = run_once(&workload(fleet.router, label, prefix_pool))
        .await
        .expect("run should finish");
    let hit_rate = fleet.observed_hit_rate().await;
    (report, hit_rate)
}

/// Prefixes that do not fit on one worker but do fit across the fleet.
const OVERSUBSCRIBED_POOL: usize = 10;

fn p50_ttft_ms(report: &RunReport) -> f64 {
    report
        .latency
        .ttft_from_intended
        .as_ref()
        .and_then(|summary| summary.value_at(50.0))
        .map(|value| value as f64 / 1_000.0)
        .expect("a p50 should exist")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn affinity_raises_the_workers_own_cache_hit_rate() {
    let (round_robin, rr_hits) =
        measure(Policy::RoundRobin, "round-robin", OVERSUBSCRIBED_POOL).await;
    let (affinity, affinity_hits) = measure(
        Policy::PrefixAffinity,
        "prefix-affinity",
        OVERSUBSCRIBED_POOL,
    )
    .await;

    assert!(round_robin.validity.valid, "{:?}", round_robin.validity);
    assert!(affinity.validity.valid, "{:?}", affinity.validity);

    // Round-robin shows every prefix to every worker, so each worker thrashes
    // a working set it cannot hold. Affinity partitions the prefixes across the
    // fleet, and each partition fits.
    assert!(
        affinity_hits > rr_hits + 0.1,
        "affinity hit rate {affinity_hits:.3} did not beat round-robin's {rr_hits:.3}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn affinity_lowers_time_to_first_token() {
    let (round_robin, _) = measure(Policy::RoundRobin, "round-robin", OVERSUBSCRIBED_POOL).await;
    let (affinity, _) = measure(
        Policy::PrefixAffinity,
        "prefix-affinity",
        OVERSUBSCRIBED_POOL,
    )
    .await;

    let baseline = p50_ttft_ms(&round_robin);
    let improved = p50_ttft_ms(&affinity);

    assert!(
        improved < baseline,
        "affinity p50 TTFT {improved:.1}ms did not beat round-robin's {baseline:.1}ms"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_balanced_policy_also_beats_the_baseline_on_an_even_workload() {
    let (_round_robin, rr_hits) =
        measure(Policy::RoundRobin, "round-robin", OVERSUBSCRIBED_POOL).await;
    let (balanced, balanced_hits) = measure(
        Policy::PrefixAffinityBalanced,
        "prefix-affinity-balanced",
        OVERSUBSCRIBED_POOL,
    )
    .await;

    assert!(balanced.validity.valid, "{:?}", balanced.validity);
    assert!(
        balanced_hits > rr_hits,
        "balanced hit rate {balanced_hits:.3} did not beat round-robin's {rr_hits:.3}"
    );
}

/// The control condition.
///
/// With nothing shared between prompts there is no locality to exploit, so
/// affinity has nothing to win and must not lose. A policy that looked better
/// here would be measuring something other than cache reuse.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn with_no_shared_prefix_the_policies_are_indistinguishable() {
    async fn run(policy: Policy) -> f64 {
        let fleet = spawn_fleet(policy, 3).await;
        let mut config = workload(fleet.router, "control", 0);
        config.shared_prefix_words = 0;

        let (report, _) = run_once(&config).await.expect("run should finish");
        assert!(report.validity.valid, "{:?}", report.validity);
        fleet.observed_hit_rate().await
    }

    let baseline = run(Policy::RoundRobin).await;
    let affinity = run(Policy::PrefixAffinity).await;

    assert!(
        baseline < 0.15 && affinity < 0.15,
        "independent prompts should barely hit: round-robin {baseline:.3}, affinity {affinity:.3}"
    );
}

/// The router must not be the only witness to its own success.
///
/// The router's index predicts which worker holds a prefix. The worker counts
/// what it actually served from cache. If routing is working, the workers'
/// counters have to agree that it is.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_workers_confirm_what_the_router_believes() {
    let fleet = spawn_fleet(Policy::PrefixAffinity, 3).await;
    let (report, _) = run_once(&workload(
        fleet.router,
        "prefix-affinity",
        OVERSUBSCRIBED_POOL,
    ))
    .await
    .expect("run should finish");
    assert!(report.validity.valid, "{:?}", report.validity);

    let observed = fleet.observed_hit_rate().await;
    assert!(
        observed > 0.4,
        "the workers reported a {observed:.3} hit rate while the router routed for affinity"
    );

    // Every worker should have served something. A policy that funnels all
    // traffic onto one worker would post a fine hit rate and be useless.
    let served: Vec<u64> = {
        let mut counts = Vec::new();
        for worker in &fleet.workers {
            counts.push(worker.counters().completed);
        }
        counts
    };
    assert!(
        served.iter().filter(|count| **count > 0).count() >= 2,
        "affinity collapsed onto too few workers: {served:?}"
    );
}

/// The crossover, documented as a test.
///
/// When the whole working set fits in every worker's cache, there is nothing
/// for cache-aware routing to arrange. Round-robin already hits on almost
/// everything, and affinity's only effect is to constrain where requests can
/// go. This is the first regime where the approach does not pay, and knowing
/// it is a result rather than a disappointment.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn affinity_gains_nothing_when_the_working_set_fits_everywhere() {
    // Three prefixes at about 16 blocks each is roughly 48 blocks, comfortably
    // inside a 64 block cache. Every worker can hold everything.
    let small_pool = 3;

    let (_, rr_hits) = measure(Policy::RoundRobin, "round-robin", small_pool).await;
    let (_, affinity_hits) = measure(Policy::PrefixAffinity, "prefix-affinity", small_pool).await;

    assert!(
        rr_hits > 0.8,
        "round-robin should already hit almost everything here, got {rr_hits:.3}"
    );
    assert!(
        (affinity_hits - rr_hits).abs() < 0.1,
        "expected no meaningful difference, got round-robin {rr_hits:.3} and affinity {affinity_hits:.3}"
    );
}
