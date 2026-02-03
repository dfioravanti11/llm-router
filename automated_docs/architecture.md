# Architecture

> Living document. Update this file whenever a change touches how components are structured or how they talk to each other — new modules, changed request flow, new external dependency, changed data flow through the block index, etc. Keep it in sync with the actual code, not the aspirational spec: if code diverges from `project_spec.md`, this file should describe what's real, and note the divergence.

**Status as of this writing: R0.1.** The "What exists today" section below
describes real code. Everything after it is the target architecture from
`project_spec.md`, not yet built. Move sections up as they land.

## What exists today

A Cargo workspace with two crates.

### `crates/warmpath` — the router

| Module | Responsibility |
|---|---|
| `config` | TOML deserialization with `deny_unknown_fields`, plus validation (non-empty worker list, unique names, URL scheme). |
| `worker` | `Worker` (name + normalized base URL) and `WorkerPool` (workers + one shared `reqwest::Client`). `WorkerPool::pick` is the policy seam; R0.1 always returns the first worker. |
| `proxy` | The forwarding path, plus the health handler. |
| `error` | `ProxyError` mapped to OpenAI-shaped error bodies. |
| `lib` | `AppState`, `RequestIds`, `router()`, `init_tracing()`. |

Request path as built:

1. `proxy::proxy` splits the request into parts and body.
2. A request id is taken from an inbound `x-request-id` when it is present and
   well-formed, otherwise generated from a per-process counter.
3. The declared `content-length` is checked against `server.max_request_bytes`
   before anything is buffered; the body is then buffered, because R0.3 needs
   the full prompt.
4. `WorkerPool::pick` selects a worker. Hop-by-hop headers, headers named in
   `Connection`, `host`, and `content-length` are dropped; the request id is
   added.
5. The upstream response's status and forwardable headers are copied back, and
   the body is streamed through as raw `Bytes`.

Two properties fall out of the streaming design rather than being bolted on:

- **Byte-identical output.** Nothing parses SSE, re-chunks, or decompresses.
  The `reqwest` dependency enables no compression features, so bodies arrive
  exactly as the worker wrote them.
- **Cancellation and backpressure.** The upstream response and a `StreamGuard`
  both live inside the response body's generator. When the client disconnects,
  axum drops the body, which drops the generator, which closes the upstream
  connection and logs the cancellation. Because the upstream stream is polled
  inline by the client's response body, bytes are pulled off the worker socket
  only when the client is ready for them.

### `crates/warmpath-mock` — the mock worker

Serves `/v1/chat/completions`, `/v1/completions`, `/health`, and
`/debug/stats`. Output is deterministic (fixed completion id, `created: 0`), so
two runs of the same request produce byte-identical responses — which is what
makes the router's byte-identity test an equality assertion rather than a field
comparison. A `Slot` guard held by the response body reports `active`,
`started`, `completed`, and `cancelled`, so a dropped response is observable
from outside the process.

Not built yet: cache simulation, queueing, KV utilization, ZMQ event
publishing, failure injection.

### Not built yet, anywhere

Prompt builder, block index, policy engine, metrics, tracing export,
`warmpath-bench`, Docker Compose.

## Target system overview

Warmpath is an OpenAI-compatible HTTP proxy in front of N LLM inference workers (vLLM, or a mock worker in development). It routes each request to the worker most likely to already hold that request's prompt prefix in its KV cache.

```
                          ┌──────────────────────────────────────────────────┐
   OpenAI-compatible      │                  WARMPATH ROUTER                 │
   client ───────────────►│                                                  │
                          │  ┌────────────────┐                              │
                          │  │  HTTP ingress  │  axum/hyper                  │
                          │  │  /v1/chat/…    │  SSE, backpressure,          │
                          │  └───────┬────────┘  cancellation                │
                          │          │                                       │
                          │  ┌───────▼────────┐                              │
                          │  │ Prompt builder │  chat template →             │
                          │  │                │  tokenize → block hashes     │
                          │  └───────┬────────┘                              │
                          │          │                                       │
                          │  ┌───────▼────────┐      ┌──────────────────┐    │
                          │  │  Policy engine │◄─────┤   Block Index    │    │
                          │  │ rr | p2c | LL  │      │  hash → {worker} │    │
                          │  │ prefix-affinity│      │                  │    │
                          │  └───────┬────────┘      │  approximate     │    │
                          │          │               │      OR          │    │
                          │  ┌───────▼────────┐      │  event-driven    │    │
                          │  │ Worker pool    │      └────▲────────▲────┘    │
                          │  │ health, drain, │           │        │         │
                          │  │ circuit break, │   inferred│        │ ZMQ     │
                          │  │ hedging, load  │   updates │        │ events  │
                          │  │ + KV pressure  │───────────┘        │         │
                          │  └───────┬────────┘                    │         │
                          │          │      ┌──────────────────┐   │         │
                          │          │      │ Metrics / traces │   │         │
                          └──────────┼──────┴──────────────────┴───┼─────────┘
                                     │                             │
                   ┌─────────────────┼──────────────┐              │
                   ▼                 ▼              ▼              │
             ┌──────────┐      ┌──────────┐   ┌──────────┐         │
             │ Worker 1 │      │ Worker 2 │   │ Worker N │         │
             │ mock →   │      │ mock →   │   │ mock →   │─────────┘
             │ vLLM     │      │ vLLM     │   │ vLLM     │  BlockStored /
             │ /metrics │      │ /metrics │   │ /metrics │  BlockRemoved
             └──────────┘      └──────────┘   └──────────┘

      ┌────────────────────────────────────────────────────────────┐
      │  warmpath-bench (separate binary)                          │
      │  open-loop Poisson arrivals · intended-dispatch latency    │
      │  multi-turn sessions · agentic trace replay · HdrHistogram │
      │  warmup exclusion · N-run CIs · CDF output                 │
      └────────────────────────────────────────────────────────────┘
```

## Components

### HTTP ingress
axum/hyper server exposing `/v1/chat/completions` and `/v1/completions`, streaming and non-streaming. Byte-faithful SSE passthrough; backpressure applied end to end; client disconnect cancels the upstream request and frees the worker slot within one token interval.

### Prompt builder
Applies the target model's chat template to the full conversation (not just the latest message — see prior-art bug in `CLAUDE.md`), tokenizes with the HF `tokenizers` crate, and computes a vLLM-compatible block hash chain (parent hash + block tokens, 16-token blocks by default). Per-session tokenizer caching avoids re-tokenizing history on every turn.

### Block index
Hash → {worker} mapping, one trait with two backends:
- **Approximate**: radix tree over block-hash sequences, updated from the router's own dispatches, LRU eviction with a per-worker block budget. No dependency on the worker.
- **Event-driven**: ZMQ SUB per worker, msgpack-decoded `BlockStored`/`BlockRemoved` events from vLLM. Has a replay endpoint for recovering after a subscriber gap.

Both backends support **in-flight block reservation**: blocks a dispatched-but-incomplete request will produce are provisionally attributed to its worker, so concurrent identical-prefix requests route together instead of all scoring as misses.

### Policy engine
Pluggable, config-switchable routing policies: `round-robin`, `random`, `least-loaded`, `power-of-two`, `prefix-affinity`, `prefix-affinity-balanced`. The balanced variant scores workers on match ratio + inverse queue depth + KV headroom, and falls back to shortest-queue routing (`reason: balance-override`) when the fleet is imbalanced past configured thresholds.

### Worker pool
Tracks per-worker health (active checks, ejection/re-admission), circuit breaking on error rate/latency, graceful drain for rolling restarts, hedged requests (fire a duplicate at the p95 threshold, cancel the loser), and scale-out/scale-in with index rebuild on join. Polls each worker's `/metrics` for queue depth, KV utilization, and vLLM's own `prefix_cache_queries`/`prefix_cache_hits` (ground truth for validating the router's predicted hit rate).

### Metrics / traces
Prometheus `/metrics` endpoint (predicted + actual hit rate, TTFT/TPOT/E2E histograms, per-worker queue depth and KV utilization, index size, routing decisions by reason, hedge fire/win rates). OpenTelemetry traces spanning router → worker. Grafana dashboard shipped as JSON.

### Mock worker
Standalone binary matching vLLM's request/response shape, for GPU-free development. Simulates block-level prefix cache with configurable capacity/eviction, TTFT as a function of uncached prefill tokens, bounded concurrency with queueing, optional ZMQ event publishing, and failure injection (latency spikes, errors, mid-stream aborts, hard kill).

### warmpath-bench
Separate binary, usable standalone against any OpenAI-compatible endpoint. Open-loop Poisson-arrival load generator with intended-dispatch-time latency accounting, self-monitoring for generator-side saturation, multi-turn session modeling, trace replay (CacheWise agentic traces, ShareGPT-style, synthetic), HdrHistogram output, run manifests (config/seed/git SHA).

## Data flow: one request

1. Request arrives → ID assigned → trace span opened.
2. Prompt builder renders full conversation → tokenizes → computes block hash chain.
3. Policy engine scores workers using the block index + worker pool's load/pressure signals; picks a worker and a reason (`affinity` / `balance-override` / `fallback` / `hedge`).
4. In-flight reservation attributes this request's future blocks to the chosen worker.
5. Request forwarded; SSE streamed back with backpressure; hedge timer armed if enabled.
6. On completion: reservation confirmed (approximate) or reconciled against ZMQ events (event-driven); metrics recorded; session→worker affinity updated.
7. On cancellation/failure: reservation released, upstream cancelled, circuit breaker updated.

## External dependencies

| Dependency | Role |
|---|---|
| vLLM workers | Serve inference; publish `/metrics` and (optionally) ZMQ KV events |
| Prometheus | Scrapes router `/metrics` |
| Grafana | Dashboards over Prometheus |
| Hugging Face Hub | Model/tokenizer download (`HF_TOKEN`, `MODEL_ID`) |
| Docker Compose | Local orchestration of router + N mock workers + Prometheus + Grafana |

## Configuration

Single TOML file: worker endpoints and event sockets, model/tokenizer, block size, policy selection, `cache_threshold`, `balance_abs_threshold`, `balance_rel_threshold`, index backend and capacity, hedging thresholds, health-check and circuit-breaker parameters, metrics/tracing endpoints. Reloadable so baseline and treatment runs share one binary.
