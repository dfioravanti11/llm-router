//! Mock inference worker.
//!
//! Speaks enough of the OpenAI API for the router to be developed and tested
//! without a GPU: streaming and non-streaming chat completions, bounded
//! concurrency with queueing, and counters the router's cancellation tests
//! assert on. Cache simulation, KV utilization, ZMQ event publishing, and
//! failure injection arrive with later releases.

pub mod cache;
pub mod chat;

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::Serialize;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use warmpath_core::PromptBuilder;

use crate::cache::{CacheStats, PrefixCache};

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
    /// Requests served at once. Anything beyond this waits.
    ///
    /// A worker that never queues cannot be overloaded, and an overloaded
    /// worker is the only place a closed-loop generator's blind spot shows up.
    pub max_concurrency: usize,
    /// Blocks the simulated prefix cache holds. Zero disables it, which makes
    /// every request pay full prefill and is the control condition for any
    /// cache-aware routing measurement.
    pub cache_blocks: usize,
    /// Token ids per block. Must match the router's, or the two are describing
    /// different things.
    pub block_size: usize,
    /// Prefill cost of one token whose block was not cached.
    ///
    /// This is the entire reason cache-aware routing shows up in a latency
    /// measurement: a cached prefix skips it.
    pub prefill_per_token: Duration,
}

impl Default for MockConfig {
    fn default() -> Self {
        Self {
            model: "mock-model".to_string(),
            time_to_first_token: Duration::from_millis(20),
            inter_token_delay: Duration::from_millis(5),
            default_max_tokens: 32,
            max_concurrency: 256,
            cache_blocks: 0,
            block_size: warmpath_core::DEFAULT_BLOCK_SIZE,
            prefill_per_token: Duration::from_micros(50),
        }
    }
}

/// Counters the router's tests read to prove a client disconnect frees the slot.
#[derive(Debug, Default, Serialize)]
pub struct Counters {
    /// Requests currently holding a slot, whether queued or being served.
    pub active: i64,
    /// Requests waiting for a serving slot.
    pub queued: i64,
    /// Serving slots available right now.
    pub available_slots: usize,
    /// Requests admitted since start.
    pub started: u64,
    /// Requests whose response was delivered in full.
    pub completed: u64,
    /// Requests whose client went away before the response finished.
    pub cancelled: u64,
    /// The simulated prefix cache, in the shape vLLM reports.
    pub cache: CacheStats,
}

#[derive(Debug)]
struct Inner {
    config: MockConfig,
    /// Bounds how many requests are served at once. A request holds a permit
    /// for the whole response, so the queue in front of it is real rather than
    /// simulated.
    slots: Arc<Semaphore>,
    cache: Mutex<PrefixCache>,
    prompts: PromptBuilder,
    active: AtomicI64,
    queued: AtomicI64,
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
        let slots = Arc::new(Semaphore::new(config.max_concurrency.max(1)));
        let cache = Mutex::new(PrefixCache::new(config.cache_blocks));
        let prompts = PromptBuilder::simple(config.block_size);
        Self {
            inner: Arc::new(Inner {
                config,
                cache,
                prompts,
                slots,
                active: AtomicI64::new(0),
                queued: AtomicI64::new(0),
                started: AtomicU64::new(0),
                completed: AtomicU64::new(0),
                cancelled: AtomicU64::new(0),
            }),
        }
    }

    pub fn config(&self) -> &MockConfig {
        &self.inner.config
    }

    /// Charge a request against the prefix cache, returning how many of its
    /// prompt tokens still have to be prefilled.
    ///
    /// The cache is consulted after the serving slot is taken, so a queued
    /// request's wait is not mistaken for prefill.
    pub async fn admit_prompt(&self, body: &serde_json::Value) -> Prefill {
        let Some(fingerprint) = crate::chat::fingerprint(&self.inner.prompts, body) else {
            return Prefill::default();
        };

        let cached_blocks = self.inner.cache.lock().await.admit(&fingerprint.blocks);
        let block_size = self.inner.config.block_size;

        Prefill {
            prompt_tokens: fingerprint.token_count,
            cached_tokens: cached_blocks * block_size,
            uncached_tokens: fingerprint
                .token_count
                .saturating_sub(cached_blocks * block_size),
        }
    }

    pub async fn cache_stats(&self) -> CacheStats {
        self.inner.cache.lock().await.stats()
    }

    pub fn counters(&self) -> Counters {
        Counters {
            active: self.inner.active.load(Ordering::Relaxed),
            queued: self.inner.queued.load(Ordering::Relaxed),
            available_slots: self.inner.slots.available_permits(),
            started: self.inner.started.load(Ordering::Relaxed),
            completed: self.inner.completed.load(Ordering::Relaxed),
            cancelled: self.inner.cancelled.load(Ordering::Relaxed),
            cache: CacheStats::default(),
        }
    }

    /// Take a slot, waiting for a serving permit if the worker is busy.
    ///
    /// The wait happens before any timing starts, so a queued request's time to
    /// first token includes the queueing, exactly as it would on a real worker.
    async fn admit(&self) -> Slot {
        self.inner.started.fetch_add(1, Ordering::Relaxed);
        self.inner.active.fetch_add(1, Ordering::Relaxed);
        self.inner.queued.fetch_add(1, Ordering::Relaxed);

        // The semaphore is never closed, so acquiring cannot fail.
        let permit = Arc::clone(&self.inner.slots)
            .acquire_owned()
            .await
            .expect("the slot semaphore is never closed");

        self.inner.queued.fetch_sub(1, Ordering::Relaxed);

        Slot {
            state: self.clone(),
            _permit: permit,
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
    /// Released with the slot, which is what lets a queued request start.
    _permit: OwnedSemaphorePermit,
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
    let mut counters = state.counters();
    counters.cache = state.cache_stats().await;
    Json(counters)
}

/// What a request costs, once the prefix cache has had its say.
#[derive(Debug, Clone, Copy, Default)]
pub struct Prefill {
    pub prompt_tokens: usize,
    /// Tokens whose blocks were already cached, so prefill skips them.
    pub cached_tokens: usize,
    pub uncached_tokens: usize,
}
