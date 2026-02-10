//! R0.2 exit criteria, as tests.
//!
//! The harness has to be trustworthy before any number it produces is worth
//! reading, so these run the real generator against a real worker rather than
//! asserting on synthetic records.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use warmpath::config::{
    AffinityConfig, Config, IndexConfig, Policy, RoutingConfig, ServerConfig, UpstreamConfig,
    WorkerConfig,
};
use warmpath_bench::record::{Mode, RunConfig};
use warmpath_bench::report::write_run;
use warmpath_bench::{run_once, Campaign};
use warmpath_mock::{MockConfig, MockState};

async fn spawn_mock(ttft: Duration, inter_token: Duration) -> SocketAddr {
    spawn_mock_with(ttft, inter_token, 256).await
}

async fn spawn_mock_with(
    ttft: Duration,
    inter_token: Duration,
    max_concurrency: usize,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock worker should bind");
    let addr = listener.local_addr().expect("mock should have an addr");
    let state = MockState::new(MockConfig {
        time_to_first_token: ttft,
        inter_token_delay: inter_token,
        max_concurrency,
        ..MockConfig::default()
    });

    tokio::spawn(async move {
        let _ = axum::serve(listener, warmpath_mock::router(state)).await;
    });

    addr
}

async fn spawn_router(workers: &[SocketAddr]) -> SocketAddr {
    let config = Config {
        server: ServerConfig::default(),
        upstream: UpstreamConfig::default(),
        routing: RoutingConfig {
            policy: Policy::RoundRobin,
            affinity: AffinityConfig::default(),
        },
        index: IndexConfig::default(),
        workers: workers
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
    let addr = listener.local_addr().expect("router should have an addr");

    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    addr
}

fn config(target: SocketAddr) -> RunConfig {
    RunConfig {
        target: format!("http://{target}"),
        endpoint: "/v1/chat/completions".to_string(),
        model: "mock-model".to_string(),
        mode: Mode::OpenLoop,
        rate_per_second: 50.0,
        concurrency: 4,
        duration_secs: 2.0,
        warmup_secs: 0.5,
        seed: 11,
        label: String::new(),
        prompt_words: 32,
        shared_prefix_words: 0,
        prefix_pool: 0,
        max_tokens: 4,
        stream: true,
        max_dispatch_lag_ms: 50.0,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_open_loop_run_through_the_router_is_valid_and_complete() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let (report, records) = run_once(&config(router)).await.expect("run should finish");

    assert!(
        report.validity.valid,
        "run was invalid: {:?}",
        report.validity.reasons
    );
    assert!(
        report.counts.measured > 40,
        "expected roughly 75 measured requests, got {}",
        report.counts.measured
    );
    assert_eq!(
        report.counts.failed,
        0,
        "no request should have failed: {:?}",
        records.iter().find_map(|record| record.error.clone())
    );
    assert!(report.counts.warmup > 0, "the warmup window caught nothing");
    assert!(report.latency.ttft_from_intended.is_some());
    assert!(report.latency.e2e_from_intended.is_some());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn intended_time_latency_is_never_below_dispatch_time_latency() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let (_, records) = run_once(&config(router)).await.expect("run should finish");

    // This is the invariant the whole open-loop design rests on. A request
    // cannot respond before it was due, so measuring from the intended time is
    // always the larger number. If this ever inverts, the schedule and the
    // clock have come apart.
    for record in &records {
        if let (Some(intended), Some(dispatch)) = (record.ttft_us, record.ttft_from_dispatch_us) {
            assert!(
                intended >= dispatch,
                "request {} reported {intended}us from intended but {dispatch}us from dispatch",
                record.index
            );
        }
        if let (Some(intended), Some(dispatch)) = (record.e2e_us, record.e2e_from_dispatch_us) {
            assert!(intended >= dispatch, "request {} inverted", record.index);
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_unloaded_run_shows_no_coordinated_omission_gap() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let (report, _) = run_once(&config(router)).await.expect("run should finish");

    // Nothing is queueing, so the two views of the same requests should agree.
    // The gap is a symptom of overload, not an artefact of the measurement.
    //
    // Compared in absolute terms rather than as a ratio: against a worker that
    // answers in a couple of milliseconds, a fraction of a millisecond of
    // scheduling jitter is a large ratio and a small problem. The gap is
    // dispatch lag, so bounding it in milliseconds is the statement that
    // actually matters.
    for gap in &report.omission_gap_ttft {
        let difference_us = gap.from_intended_us.saturating_sub(gap.from_dispatch_us);
        assert!(
            difference_us < 10_000,
            "p{} was {:.1}ms late against dispatch on an unloaded run",
            gap.percentile,
            difference_us as f64 / 1_000.0
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_run_that_misses_its_own_lag_budget_is_marked_invalid() {
    let worker = spawn_mock(Duration::from_millis(1), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    // A zero-millisecond budget is unmeetable: dispatch always takes some time
    // after the deadline. This checks the whole path from measurement to
    // verdict, without depending on how fast the machine running it happens to
    // be.
    let mut config = config(router);
    config.max_dispatch_lag_ms = 0.0;

    let (report, _) = run_once(&config).await.expect("run should finish");

    assert!(!report.validity.valid);
    assert!(
        report.validity.reasons[0].contains("fell behind schedule"),
        "{:?}",
        report.validity.reasons
    );
    assert!(report.validity.p99_dispatch_lag_us > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_closed_loop_run_records_requests_from_every_caller() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let mut config = config(router);
    config.mode = Mode::ClosedLoop;
    config.concurrency = 4;
    config.duration_secs = 1.0;
    config.warmup_secs = 0.2;

    let (report, records) = run_once(&config).await.expect("run should finish");

    assert!(report.counts.measured > 0);
    assert_eq!(report.counts.failed, 0);
    // A closed-loop caller is never late, because its request is due exactly
    // when it becomes free to send it. That is the blind spot, stated as an
    // assertion.
    for record in &records {
        assert_eq!(
            record.ttft_us, record.ttft_from_dispatch_us,
            "closed loop cannot distinguish the two clocks"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn round_robin_spreads_a_run_across_every_worker() {
    let first = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let second = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[first, second]).await;

    let (report, _) = run_once(&config(router)).await.expect("run should finish");
    assert!(report.validity.valid, "{:?}", report.validity.reasons);

    let metrics = reqwest::get(format!("http://{router}/metrics"))
        .await
        .expect("metrics request should succeed")
        .text()
        .await
        .expect("metrics body should decode");

    for worker in ["w0", "w1"] {
        let needle = format!(
            r#"warmpath_routing_decisions_total{{worker="{worker}",policy="round-robin"}}"#
        );
        let line = metrics
            .lines()
            .find(|line| line.starts_with(&needle))
            .unwrap_or_else(|| panic!("no routing decisions for {worker} in:\n{metrics}"));
        let count: u64 = line
            .rsplit(' ')
            .next()
            .and_then(|value| value.parse().ok())
            .expect("decision count should parse");
        assert!(count > 0, "{worker} served nothing");
    }

    assert!(
        metrics.contains("warmpath_in_flight_requests 0"),
        "in-flight should be back to zero after the run:\n{metrics}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn three_runs_aggregate_into_a_publishable_campaign() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let mut reports = Vec::new();
    for repetition in 0..3 {
        let mut config = config(router);
        config.seed = 100 + repetition;
        config.duration_secs = 1.0;
        config.warmup_secs = 0.2;

        let (report, _) = run_once(&config).await.expect("run should finish");
        assert!(report.validity.valid, "{:?}", report.validity.reasons);
        reports.push(report);
    }

    let campaign = Campaign::from_reports(&reports).expect("should aggregate");

    assert_eq!(campaign.valid_runs, 3);
    campaign
        .publishable()
        .expect("three valid runs from one commit should be publishable");

    let p99 = campaign
        .metrics
        .get("ttft_from_intended_p99_us")
        .expect("p99 should be aggregated");
    assert_eq!(p99.runs, 3);
    assert!(
        p99.ci95_half_width.is_some(),
        "three runs should yield an interval"
    );
}

/// The R0.2 headline result, as a test.
///
/// The same saturated worker, measured two ways. Open loop keeps arriving on
/// schedule and sees the queue build. Closed loop throttles itself the moment
/// the worker slows down, so it never offers the load that produced the queue
/// and reports a much friendlier tail. Both runs are internally valid; the
/// closed-loop one is answering a different question than the one it appears to
/// answer.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closed_loop_under_reports_the_tail_that_open_loop_measures() {
    // Two serving slots, ~30ms per response. Capacity is about 66 requests per
    // second.
    let worker = spawn_mock_with(Duration::from_millis(10), Duration::from_millis(5), 2).await;
    let router = spawn_router(&[worker]).await;

    let mut open = config(router);
    open.max_tokens = 4;
    open.rate_per_second = 150.0;
    open.duration_secs = 2.0;
    open.warmup_secs = 0.3;
    // The generator itself keeps up fine here; the queue is at the worker.
    open.max_dispatch_lag_ms = 100.0;

    let mut closed = open.clone();
    closed.mode = Mode::ClosedLoop;
    closed.concurrency = 2;

    let (open_report, _) = run_once(&open).await.expect("open-loop run should finish");
    let (closed_report, _) = run_once(&closed)
        .await
        .expect("closed-loop run should finish");

    let open_p99 = open_report
        .latency
        .ttft_from_intended
        .as_ref()
        .and_then(|summary| summary.value_at(99.0))
        .expect("open loop should report a p99");
    let closed_p99 = closed_report
        .latency
        .ttft_from_intended
        .as_ref()
        .and_then(|summary| summary.value_at(99.0))
        .expect("closed loop should report a p99");

    assert!(
        open_p99 > closed_p99 * 2,
        "expected closed loop to under-report the tail: open p99 {:.1}ms vs closed p99 {:.1}ms",
        open_p99 as f64 / 1_000.0,
        closed_p99 as f64 / 1_000.0
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_saturated_worker_makes_requests_queue() {
    let worker = spawn_mock_with(Duration::from_millis(10), Duration::from_millis(5), 2).await;
    let router = spawn_router(&[worker]).await;

    let mut config = config(router);
    config.max_tokens = 4;
    config.rate_per_second = 150.0;
    config.duration_secs = 1.5;
    config.warmup_secs = 0.2;
    config.max_dispatch_lag_ms = 100.0;

    let (report, _) = run_once(&config).await.expect("run should finish");

    let ttft = report
        .latency
        .ttft_from_intended
        .as_ref()
        .expect("summary should exist");
    let p50 = ttft.value_at(50.0).expect("p50");
    let p99 = ttft.value_at(99.0).expect("p99");

    // Unqueued service time is about 30ms. A saturated worker has to push the
    // tail well past that, or the concurrency limit is not doing anything.
    assert!(
        p99 > 60_000,
        "p99 was {:.1}ms; the worker does not appear to be queueing",
        p99 as f64 / 1_000.0
    );
    assert!(p99 > p50, "the tail should be worse than the median");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_written_run_can_be_read_back() {
    let worker = spawn_mock(Duration::from_millis(2), Duration::from_millis(0)).await;
    let router = spawn_router(&[worker]).await;

    let mut config = config(router);
    config.duration_secs = 1.0;
    config.warmup_secs = 0.2;

    let (report, records) = run_once(&config).await.expect("run should finish");
    let directory = tempfile::tempdir().expect("temp dir should be created");
    let run_directory = write_run(directory.path(), &report, &records).expect("run should write");

    let written: warmpath_bench::RunReport = serde_json::from_str(
        &std::fs::read_to_string(run_directory.join("report.json")).expect("report should exist"),
    )
    .expect("report should parse");
    assert_eq!(written.run_id, report.run_id);
    assert_eq!(written.counts.measured, report.counts.measured);

    let jsonl =
        std::fs::read_to_string(run_directory.join("records.jsonl")).expect("records should exist");
    assert_eq!(jsonl.lines().count(), records.len());

    let csv =
        std::fs::read_to_string(run_directory.join("percentiles.csv")).expect("csv should exist");
    assert!(csv.starts_with("metric,percentile,value_us\n"));
    assert!(csv.contains("ttft_from_intended,99,"));
}
