# Project Status

> Update this file whenever a milestone's exit criteria are met, scope changes, or the "what's next" changes. Milestones and exit criteria are defined in `project_spec.md` §3 (Release Roadmap) — this file tracks progress against them, it doesn't redefine them.

## Current phase: R0.2 complete, starting R0.3

The router is a correct streaming proxy with baseline routing policies and
Prometheus metrics, and the measurement harness is in place. No prompt builder
and no block index yet.

## Milestones

Each release is a theme + exit criterion + demo artifact, not a date. GPU is required only at R0.5 and R1.0 (bounded validation sessions).

| Release | Theme | Exit criterion | Status |
|---|---|---|---|
| R0.1 | Skeleton — correct proxy, does nothing clever | Client disconnect provably frees the worker slot; SSE bytes match upstream exactly | Done |
| R0.2 | Honest measurement — the harness, before any policy | Baseline p99 TTFT with CI from ≥3 runs, reproducible by one command; open-vs-closed-loop coordinated-omission demo | Done |
| R0.3 | The core idea — prefix-affinity routing | First affinity-vs-round-robin comparison chart with CIs on mock workers; hash-chain correctness test passing | In progress |
| R0.4 | Load-aware and session-aware | A workload where pure affinity loses and balanced affinity wins, documented | Not started |
| R0.5 | First contact with reality (real vLLM) | Real-hardware chart matching/diverging from mock result; predicted-vs-actual hit-rate discrepancy quantified | Not started |
| R0.6 | Reliability | Chaos test: zero dropped/corrupted responses across repeated worker kills; hedging improves p99.9 measurably | Not started |
| R0.7 | Precise indexing (ZMQ events) | Quantified answer to how much accuracy the approximate index gives up, and when it matters | Not started |
| R0.8 | Agentic workloads | LRU vs. reuse-aware retention comparison on real CacheWise traces, crossover documented | Not started |
| R0.9 | Self-scrutiny | Router's own added overhead measured and published; ≥2 documented failure regimes | Not started |
| R1.0 | Public release | A stranger can `docker compose up`, run `make bench`, and regenerate every published number | Not started |
| R1.0+ | Blog post | Published post covering coordinated omission, approximate-vs-precise, negative results, router overhead | Not started |

## What's been accomplished

- Product and engineering spec written (`project_spec.md` v2.0), including prior-art positioning, non-goals, success criteria, and full roadmap.
- **R0.1 shipped.** Cargo workspace with two crates, CI, and a `make check`
  gate (fmt, clippy with warnings denied, tests).
  - `warmpath`: TOML config with validation, worker pool, streaming proxy for
    `/v1/chat/completions` and `/v1/completions`, health endpoint, structured
    logging, request ids that span ingress to worker.
  - `warmpath-mock`: OpenAI-compatible mock worker with deterministic output,
    configurable TTFT and inter-token delay, and slot counters.
  - Both R0.1 exit criteria are covered by tests: SSE bytes proxied are
    byte-identical to the worker's own output, and a client that hangs up
    mid-stream frees the worker slot.
- **R0.2 shipped.** The measurement harness, before the policy it will judge.
  - `warmpath-bench`: open-loop Poisson schedule computed before the run
    starts, intended-time latency accounting, warmup exclusion, HdrHistogram
    summaries, per-request JSONL, run reports carrying config and seed and git
    SHA, and median plus 95% confidence intervals across runs.
  - Every request timed against both the intended and the actual dispatch time,
    so the coordinated-omission gap comes out of any run.
  - A run whose generator fell behind its own schedule is marked invalid and
    excluded from its campaign, with the reason recorded.
  - Router: Prometheus metrics at `/metrics`, plus `round-robin` and `first`
    policies under a `[routing]` config section.
  - Mock worker: bounded concurrency with real queueing.
  - First measured finding in `RESULTS.md`, reproducible with `make co-demo`.

## What's next

**R0.3 — Prefix-affinity routing.** The core idea, now that there is a harness
able to judge it.

1. Prompt builder: render the full conversation through the model's chat
   template, tokenize with the HF `tokenizers` crate, compute a
   vLLM-compatible block hash chain at 16-token granularity. Building from only
   the latest message is a known bug class in this space and the reason the
   full render is non-negotiable.
2. Approximate block index: radix tree over block-hash sequences with
   per-node worker sets, LRU eviction against a per-worker block budget, behind
   the trait the event-driven backend will also implement at R0.7.
3. In-flight block reservation, so two requests with the same long prefix
   arriving milliseconds apart do not both score as misses.
4. `prefix-affinity` and `prefix-affinity-balanced` policies with cache and
   balance thresholds.
5. Workload shapes with real prefix sharing in `warmpath-bench`, which R0.2
   deliberately left out.
6. A hash-chain correctness test, and the first affinity-versus-round-robin
   comparison with confidence intervals.

Per the spec's build philosophy: the block index and the routing policy get
written and understood line by line.

## Open risks to watch

From `project_spec.md` §2.2 — surface these if they start to materialize:
- Mock cache model might not be realistic enough to survive real vLLM (checked at R0.5/R1.0 gates).
- Tokenization/hashing mismatch with vLLM could silently break matching (guarded by an integration test before any routing result is trusted).
- Rust learning curve could stall early releases (mitigated by keeping R0.1 deliberately boring).
- GPU cost overrun (mitigated by bounding validation sessions to spot instances).
