# Architecture

> Living document. Update this file whenever a change touches how components are structured or how they talk to each other — new modules, changed request flow, new external dependency, changed data flow through the block index, etc. Keep it in sync with the actual code, not the aspirational spec: if code diverges from `project_spec.md`, this file should describe what's real, and note the divergence.

**Status as of this writing: R0.3.** The "What exists today" section below
describes real code. Everything after it is the target architecture from
`project_spec.md`, not yet built. Move sections up as they land.

## What exists today

A Cargo workspace with four crates.

### `crates/warmpath` — the router

| Module | Responsibility |
|---|---|
| `config` | TOML deserialization with `deny_unknown_fields`, plus validation (non-empty worker list, unique names, URL scheme). |
| `worker` | `Worker` (name + normalized base URL) and `WorkerPool` (workers, in-flight counts, the block index, one shared `reqwest::Client`, the policy, and a rotation cursor). `WorkerPool::pick` returns a `Choice` carrying the decision, the metric handles, and the block reservation. |
| `index` | The `BlockIndex` trait and its approximate backend. |
| `policy` | `choose`, a pure function of index answer, load, and cursor. |
| `prompt` | Reading the prompt out of an OpenAI request body. |
| `metrics` | Prometheus registry and per-worker metric handles. |
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
4. When the configured policy reads the index, the body is fingerprinted:
   the whole conversation rendered through the chat template, tokenized, and cut
   into a chained block hash per 16 tokens. A body the router cannot parse
   yields no fingerprint and routes on load, because no correctness invariant
   may depend on the index having an answer.
5. `WorkerPool::pick` asks the index how many leading blocks each worker holds,
   reads the in-flight counts, and calls `policy::choose`. The chosen worker
   gets a `Reservation` over the request's whole block chain.
6. Hop-by-hop headers, headers named in `Connection`, `host`, and
   `content-length` are dropped; the request id is added.
7. The upstream response's status and forwardable headers are copied back, and
   the body is streamed through as raw `Bytes`. On completion the reservation is
   confirmed; on anything else it is dropped, which releases it.

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

### Metrics

`Metrics` owns the registry; `Metrics::for_worker` resolves every handle one
worker's request path needs, once at startup, into a `WorkerMetrics` that the
`Choice` hands to the proxy. `prometheus-client` metrics share storage across
clones, so counters and gauges on the hot path are a plain atomic with no label
lookup. Histograms still take a short mutex per observation; whether that costs
anything is a question for R0.5, which measures the router's overhead rather
than assuming it.

One sharp edge worth remembering: `Family::get_or_create` returns a guard
holding a read lock on the label map, and takes a write lock when the label set
is new. Two live guards on the same family deadlock the calling thread, and
temporaries in a struct literal all live to the end of that statement — which
is exactly that shape. `for_worker` binds each lookup to its own `let`.

### The block index

A flat map from block hash to a worker bitset, plus per-worker leaf-first LRU
bookkeeping and a reservation table.

**Why not a radix tree.** The design notes call for a radix tree over
block-hash sequences. It is not needed: the hash chain already does the tree's
job, because block hash *i* is computed from its parent and therefore encodes
every token before it. Two prompts share hash *i* only if they agree on the
whole prefix, so a flat map answers prefix queries exactly as a tree would, in
one lookup per block. The tree buys knowing which blocks descend from which,
which reuse-aware eviction would want (Appendix A3); it can grow then,
measured against LRU rather than assumed better.

**Why leaf-first eviction.** Plain LRU over blocks evicts a chain's *oldest*
block, which is its *first* block, which strands every block behind it. The
worker keeps storing them and the index can never match them again, so the
modelled hit rate collapses while the modelled memory stays full. Engines evict
leaves first for the same reason. Under pressure a chain is eaten from the tail
and prefix matching degrades one block at a time.

**In-flight reservation.** Two requests with the same long prefix can arrive a
millisecond apart, before either has finished and taught the index anything.
Without reservation both score as misses and get spread across the fleet, which
is exactly the scattering the router exists to prevent. `Reservation` releases
on drop, mirroring the response body's `StreamGuard` deliberately: the same
shape means the same failure mode cannot appear in one and not the other.

Matching costs O(prompt blocks + workers), not their product. A worker's answer
is written once, at the block where it stopped matching, since the alive set
only shrinks.

### `crates/warmpath-core` — prompt handling

Chat template rendering (a documented simple form, and Jinja for the templates
models ship), tokenization behind a trait, and the chained block hash. The
router and the mock worker both depend on it and each fingerprints the request
body independently, so a hashing bug shows up as a collapsed hit rate rather
than as two copies of the same mistake agreeing.

The hashes are internally consistent, not byte-compatible with vLLM's. Routing
only needs requests comparable with each other. Compatibility matters for
checking predicted hit rate against vLLM's own counters, which is R0.5.

### `crates/warmpath-bench` — the load generator

A separate binary, usable against any OpenAI-compatible endpoint.

| Module | Responsibility |
|---|---|
| `schedule` | Seeded xorshift generator and the Poisson arrival schedule. |
| `workload` | Request bodies, built before the run starts so nothing but the send happens on the dispatch path. |
| `runner` | The dispatch loop, open and closed. |
| `record` | `RunConfig` and the per-request record. |
| `report` | Validity verdict, latency summaries, and on-disk output. |
| `stats` | HdrHistogram summaries, medians, and Student's t confidence intervals. |
| `aggregate` | Several runs into one publishable campaign. |

Three properties are why it exists rather than an off-the-shelf generator:

- **Open loop by construction.** The schedule is a `Vec<Duration>` computed
  before the first request goes out, and the dispatch loop sleeps to absolute
  deadlines. A deadline already in the past returns immediately, so generator
  slowness becomes measured lag rather than a quietly stretched schedule.
- **Both clocks, every request.** Latency is recorded from the intended
  dispatch time and from the actual one. The first is the honest number; the
  second is what a generator without a schedule would report. Keeping both
  means the coordinated-omission gap falls out of any run.
- **Runs can be invalid.** A run whose p99 dispatch lag exceeds its budget, or
  whose error rate exceeds one percent, is marked invalid, kept on disk with
  its reasons, and excluded from the campaign statistics.

Across runs, each run contributes one observation per metric — its own p99, for
instance — and those get a median and a t-based 95% interval. The interval
describes the spread of run-level percentiles, which is the question a reader
is actually asking. Applying a normal approximation to individual latencies
would be wrong: latency distributions are heavy-tailed and their percentiles
are not sample means.

### `crates/warmpath-mock` — the mock worker

Serves `/v1/chat/completions`, `/v1/completions`, `/health`, and
`/debug/stats`. Concurrency is bounded by a semaphore whose permit is held for
the whole response, so requests beyond the limit queue for real. A worker that
never queues cannot be overloaded, and an overloaded worker is the only place a
closed-loop generator's blind spot shows up. Output is deterministic (fixed completion id, `created: 0`), so
two runs of the same request produce byte-identical responses — which is what
makes the router's byte-identity test an equality assertion rather than a field
comparison. A `Slot` guard held by the response body reports `active`,
`started`, `completed`, and `cancelled`, so a dropped response is observable
from outside the process.

It also simulates a block-level prefix cache: a request whose prefix is already
held skips prefill for those blocks, so time to first token drops. That is what
makes cache-aware routing observable without a GPU. The cache is a separate
implementation from the router's index on purpose, since a prediction and the
thing it predicts being the same function proves nothing.

Not built yet: KV utilization, ZMQ event publishing, failure injection.

### Not built yet, anywhere

Worker state polling, session affinity, the event-driven index, OpenTelemetry
export, Docker Compose.

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
