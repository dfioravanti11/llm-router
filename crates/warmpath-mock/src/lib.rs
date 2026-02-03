//! Mock inference worker.
//!
//! Speaks enough of the OpenAI API for the router to be developed and tested
//! without a GPU. R0.1 covers streaming and non-streaming chat completions plus
//! the counters the router's cancellation tests assert on. Cache simulation,
//! queueing, and failure injection arrive with later releases.

pub mod chat;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;

/// Timing and identity knobs for one mock worker.
#[derive(Debug, Clone)]
pub struct MockConfig {
    /// Model name echoed back in responses.
    pub model: String,
    /// Delay before the first token of a streamed response.
    pub time_to_first_token: Duration,
    /// Delay between subsequent tokens.
    pub inter_token_delay: Duration,
    /// Token count used when the request does not set `max_tokens`.
    pub default_max_tokens: usize,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            model: "mock-model".to_string(),
            time_to_first_token: Duration::from_millis(20),
            inter_token_delay: Duration::from_millis(5),
            default_max_tokens: 32,
        }
    }
}

/// Counters the router's tests read to prove a client disconnect frees the slot.
#[derive(Debug, Default, Serialize)]
pub struct Counters {
    /// Requests currently holding a slot.
    pub active: i64,
    /// Requests admitted since start.
    pub started: u64,
    /// Requests whose response was delivered in full.
    pub completed: u64,
    /// Requests whose client went away before the response finished.
    pub cancelled: u64,
}

#[derive(Debug)]
struct Inner {
    config: MockConfig,
    active: AtomicI64,
    started: AtomicU64,
    completed: AtomicU64,
    cancelled: AtomicU64,
}

/// Shared worker state. Cheap to clone.
#[derive(Debug, Clone)]
pub struct MockState {
    inner: Arc<Inner>,
}

impl MockState {
    pub fn new(config: MockConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                config,
                active: AtomicI64::new(0),
                started: AtomicU64::new(0),
                completed: AtomicU64::new(0),
                cancelled: AtomicU64::new(0),
            }),
        }
    }

    pub fn config(&self) -> &MockConfig {
        &self.inner.config
    }

    pub fn counters(&self) -> Counters {
        Counters {
            active: self.inner.active.load(Ordering::Relaxed),
            started: self.inner.started.load(Ordering::Relaxed),
            completed: self.inner.completed.load(Ordering::Relaxed),
            cancelled: self.inner.cancelled.load(Ordering::Relaxed),
        }
    }

    fn admit(&self) -> Slot {
        self.inner.started.fetch_add(1, Ordering::Relaxed);
        self.inner.active.fetch_add(1, Ordering::Relaxed);
        Slot {
            state: self.clone(),
            released: false,
        }
    }
}

/// Holds a worker slot for the lifetime of one response.
///
/// The slot is released on drop. A response body that is dropped before it
/// finished — which is what happens when the client disconnects mid-stream —
/// therefore counts as cancelled and frees the slot without any explicit
/// cancellation signal.
struct Slot {
    state: MockState,
    released: bool,
}

impl Slot {
    fn complete(&mut self) {
        if !self.released {
            self.released = true;
            self.state.inner.active.fetch_sub(1, Ordering::Relaxed);
            self.state.inner.completed.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        if !self.released {
            self.released = true;
            self.state.inner.active.fetch_sub(1, Ordering::Relaxed);
            self.state.inner.cancelled.fetch_add(1, Ordering::Relaxed);
            tracing::info!("response dropped before completion; slot released");
        }
    }
}

/// Build the worker's HTTP surface.
pub fn router(state: MockState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/debug/stats", get(stats))
        .route("/v1/chat/completions", post(chat::chat_completions))
        .route("/v1/completions", post(chat::completions))
        .with_state(state)
}

async fn health() -> &'static str {
    "ok"
}

async fn stats(State(state): State<MockState>) -> Json<Counters> {
    Json(state.counters())
}
