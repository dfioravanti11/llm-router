# Project Status

> Update this file whenever a milestone's exit criteria are met, scope changes, or the "what's next" changes. Milestones and exit criteria are defined in `project_spec.md` §3 (Release Roadmap) — this file tracks progress against them, it doesn't redefine them.

## Current phase: R0.1 complete, starting R0.2

The router is a correct streaming proxy over a mock worker. No prompt builder,
no block index, no routing policy yet.

## Milestones

Each release is a theme + exit criterion + demo artifact, not a date. GPU is required only at R0.5 and R1.0 (bounded validation sessions).

| Release | Theme | Exit criterion | Status |
|---|---|---|---|
| R0.1 | Skeleton — correct proxy, does nothing clever | Client disconnect provably frees the worker slot; SSE bytes match upstream exactly | Done |
| R0.2 | Honest measurement — the harness, before any policy | Baseline p99 TTFT with CI from ≥3 runs, reproducible by one command; open-vs-closed-loop coordinated-omission demo | In progress |
| R0.3 | The core idea — prefix-affinity routing | First affinity-vs-round-robin comparison chart with CIs on mock workers; hash-chain correctness test passing | Not started |
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
- No routing, index, or benchmark code yet.

## What's next

**R0.2 — Honest measurement.** The harness ships before any policy, because a
baseline captured by a different harness is not a baseline.

1. `warmpath-bench` as a standalone binary against any OpenAI-compatible
   endpoint: open-loop Poisson arrival schedule computed up front, latency
   recorded as `t_response − t_intended`, generator-side saturation detection.
2. HdrHistogram recording, warmup exclusion, per-request JSONL, run manifests
   carrying config, seed, and git SHA.
3. Prometheus metrics in the router and a Grafana dashboard as JSON.
4. Round-robin baseline captured with confidence intervals across ≥3 runs.
5. The open-loop vs. deliberately closed-loop comparison on one workload,
   showing the coordinated-omission gap.

Per the spec's build philosophy: plumbing can be agent-assisted aggressively;
the benchmark statistics get written and understood line by line.

## Open risks to watch

From `project_spec.md` §2.2 — surface these if they start to materialize:
- Mock cache model might not be realistic enough to survive real vLLM (checked at R0.5/R1.0 gates).
- Tokenization/hashing mismatch with vLLM could silently break matching (guarded by an integration test before any routing result is trusted).
- Rust learning curve could stall early releases (mitigated by keeping R0.1 deliberately boring).
- GPU cost overrun (mitigated by bounding validation sessions to spot instances).
