//! R0.1 exit criteria, as tests.
//!
//! Two things have to hold before anything clever gets built on top: SSE bytes
//! reaching the client are exactly the bytes the worker wrote, and a client
//! that hangs up frees the worker slot.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_util::StreamExt;
use serde_json::json;
use tokio::net::TcpListener;
use warmpath::config::{
    Config, HealthConfig, IndexConfig, ModelConfig, RoutingConfig, ServerConfig, UpstreamConfig,
    WorkerConfig,
};
use warmpath_mock::{MockConfig, MockState};

/// Start a mock worker on an ephemeral port.
async fn spawn_mock(config: MockConfig) -> (SocketAddr, MockState) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("mock worker should bind");
    let addr = listener
        .local_addr()
        .expect("mock worker should have an addr");
    let state = MockState::new(config);

    let serve_state = state.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, warmpath_mock::router(serve_state)).await;
    });

    (addr, state)
}

/// Start a router on an ephemeral port, pointed at one worker.
async fn spawn_router(worker: SocketAddr) -> SocketAddr {
    let config = Config {
        server: ServerConfig::default(),
        upstream: UpstreamConfig::default(),
        routing: RoutingConfig::default(),
        index: IndexConfig::default(),
        model: ModelConfig::default(),
        health: HealthConfig::default(),
        workers: vec![WorkerConfig {
            name: "w0".to_string(),
            url: format!("http://{worker}"),
        }],
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

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .expect("test client should build")
}

fn streaming_request(max_tokens: usize) -> serde_json::Value {
    json!({
        "model": "mock-model",
        "stream": true,
        "max_tokens": max_tokens,
        "messages": [{ "role": "user", "content": "warm the cache" }],
    })
}

/// Collect an entire response body into one buffer.
async fn collect_body(response: reqwest::Response) -> Bytes {
    let mut stream = response.bytes_stream();
    let mut collected = Vec::new();
    while let Some(chunk) = stream.next().await {
        collected.extend_from_slice(&chunk.expect("stream chunk should arrive"));
    }
    Bytes::from(collected)
}

#[tokio::test]
async fn streamed_sse_bytes_match_the_worker_exactly() {
    let (worker, _state) = spawn_mock(MockConfig {
        time_to_first_token: Duration::from_millis(1),
        inter_token_delay: Duration::from_millis(1),
        ..MockConfig::default()
    })
    .await;
    let router = spawn_router(worker).await;
    let client = client();
    let body = streaming_request(24);

    let direct = client
        .post(format!("http://{worker}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("direct request should succeed");
    assert_eq!(direct.status(), reqwest::StatusCode::OK);
    assert_eq!(
        direct
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    let direct_bytes = collect_body(direct).await;

    let proxied = client
        .post(format!("http://{router}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("proxied request should succeed");
    assert_eq!(proxied.status(), reqwest::StatusCode::OK);
    assert_eq!(
        proxied
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("text/event-stream")
    );
    assert_eq!(
        proxied
            .headers()
            .get("x-warmpath-worker")
            .and_then(|value| value.to_str().ok()),
        Some("w0")
    );
    let proxied_bytes = collect_body(proxied).await;

    assert_eq!(
        direct_bytes, proxied_bytes,
        "proxied SSE body differs from the worker's own output"
    );
    assert!(
        proxied_bytes.ends_with(b"data: [DONE]\n\n"),
        "stream should end with the DONE sentinel"
    );
}

#[tokio::test]
async fn non_streaming_bytes_match_the_worker_exactly() {
    let (worker, _state) = spawn_mock(MockConfig {
        time_to_first_token: Duration::from_millis(1),
        inter_token_delay: Duration::from_millis(0),
        ..MockConfig::default()
    })
    .await;
    let router = spawn_router(worker).await;
    let client = client();
    let body = json!({
        "model": "mock-model",
        "stream": false,
        "max_tokens": 8,
        "messages": [{ "role": "user", "content": "warm the cache" }],
    });

    let direct = client
        .post(format!("http://{worker}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("direct request should succeed");
    let direct_bytes = collect_body(direct).await;

    let proxied = client
        .post(format!("http://{router}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("proxied request should succeed");
    let proxied_bytes = collect_body(proxied).await;

    assert_eq!(direct_bytes, proxied_bytes);
}

#[tokio::test]
async fn legacy_completions_endpoint_is_forwarded() {
    let (worker, _state) = spawn_mock(MockConfig {
        time_to_first_token: Duration::from_millis(1),
        inter_token_delay: Duration::from_millis(1),
        ..MockConfig::default()
    })
    .await;
    let router = spawn_router(worker).await;
    let client = client();
    let body = json!({ "model": "mock-model", "stream": true, "max_tokens": 4, "prompt": "hi" });

    let direct = client
        .post(format!("http://{worker}/v1/completions"))
        .json(&body)
        .send()
        .await
        .expect("direct request should succeed");
    let direct_bytes = collect_body(direct).await;

    let proxied = client
        .post(format!("http://{router}/v1/completions"))
        .json(&body)
        .send()
        .await
        .expect("proxied request should succeed");
    let proxied_bytes = collect_body(proxied).await;

    assert_eq!(direct_bytes, proxied_bytes);
    assert!(direct_bytes.contains(&b'{'), "body should carry SSE frames");
}

#[tokio::test]
async fn client_disconnect_frees_the_worker_slot() {
    // Slow enough that the stream is still open when the client hangs up, and
    // long enough that the worker has plenty of writes left to notice.
    let (worker, state) = spawn_mock(MockConfig {
        time_to_first_token: Duration::from_millis(1),
        inter_token_delay: Duration::from_millis(10),
        ..MockConfig::default()
    })
    .await;
    let router = spawn_router(worker).await;
    let client = client();

    let response = client
        .post(format!("http://{router}/v1/chat/completions"))
        // At 10ms per token this stream runs for ~50s, so it cannot reach its
        // natural end inside the 10s deadline below. Reaching `active == 0`
        // therefore proves the disconnect freed the slot.
        .json(&streaming_request(5_000))
        .send()
        .await
        .expect("request should succeed");

    let mut stream = response.bytes_stream();
    let first = stream
        .next()
        .await
        .expect("a first chunk should arrive")
        .expect("first chunk should not be an error");
    assert!(first.starts_with(b"data: "), "expected an SSE frame");
    assert_eq!(
        state.counters().active,
        1,
        "worker should be holding one slot while streaming"
    );

    // Hang up mid-stream.
    drop(stream);

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let counters = state.counters();
        if counters.active == 0 {
            assert_eq!(
                counters.cancelled, 1,
                "the freed slot should be recorded as cancelled, not completed"
            );
            assert_eq!(
                counters.completed, 0,
                "the response never finished, so nothing should be counted complete"
            );
            return;
        }
        assert!(
            Instant::now() < deadline,
            "worker slot was still held 10s after the client disconnected: {counters:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn health_endpoint_answers_without_a_worker_round_trip() {
    let (worker, _state) = spawn_mock(MockConfig::default()).await;
    let router = spawn_router(worker).await;

    let response = client()
        .get(format!("http://{router}/health"))
        .send()
        .await
        .expect("health request should succeed");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.expect("body should decode"), "ok");
}

#[tokio::test]
async fn oversized_requests_are_rejected_before_dispatch() {
    let (worker, state) = spawn_mock(MockConfig::default()).await;

    let config = Config {
        server: ServerConfig {
            max_request_bytes: 512,
            ..ServerConfig::default()
        },
        upstream: UpstreamConfig::default(),
        routing: RoutingConfig::default(),
        index: IndexConfig::default(),
        model: ModelConfig::default(),
        health: HealthConfig::default(),
        workers: vec![WorkerConfig {
            name: "w0".to_string(),
            url: format!("http://{worker}"),
        }],
    };
    let app = warmpath::router(&config).expect("router should build");
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("router should bind");
    let router = listener.local_addr().expect("router should have an addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let response = client()
        .post(format!("http://{router}/v1/chat/completions"))
        .json(&json!({
            "model": "mock-model",
            "messages": [{ "role": "user", "content": "x".repeat(4096) }],
        }))
        .send()
        .await
        .expect("request should get a response");

    assert_eq!(response.status(), reqwest::StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        state.counters().started,
        0,
        "an oversized request should never reach the worker"
    );
}

#[tokio::test]
async fn an_inbound_request_id_is_carried_through() {
    let (worker, _state) = spawn_mock(MockConfig {
        time_to_first_token: Duration::from_millis(1),
        inter_token_delay: Duration::from_millis(0),
        ..MockConfig::default()
    })
    .await;
    let router = spawn_router(worker).await;

    let response = client()
        .post(format!("http://{router}/v1/chat/completions"))
        .header("x-request-id", "trace-abc-123")
        .json(&json!({ "model": "mock-model", "max_tokens": 2, "messages": [] }))
        .send()
        .await
        .expect("request should succeed");

    assert_eq!(
        response
            .headers()
            .get("x-warmpath-request-id")
            .and_then(|value| value.to_str().ok()),
        Some("trace-abc-123")
    );
}
