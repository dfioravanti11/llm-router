# Project Status

> Update this file whenever a milestone's exit criteria are met, scope changes, or the "what's next" changes. Milestones and exit criteria are defined in `project_spec.md` §3 (Release Roadmap) — this file tracks progress against them, it doesn't redefine them.

## Current phase: R0.4 complete, starting R0.5

The router builds a prompt fingerprint with the model's own tokenizer, keeps an
approximate block index with in-flight reservation, polls each worker for queue
depth and KV pressure, and chooses among six policies. Health checking, a single
retry, and session affinity are in.

R0.5 is the ship point and the only milestone that needs a GPU.

## Milestones

Spec v3.0 cut the roadmap from ten releases to five. **R0.1 through R0.5 are
the commitment; everything else is Appendix A and closed until R0.5 ships.**
GPU is required only at R0.5.

| Release | Theme | Exit criterion | Status |
|---|---|---|---|
| R0.1 | Skeleton — correct proxy, does nothing clever | Client disconnect provably frees the worker slot; SSE bytes match upstream exactly | Done |
| R0.2 | Honest measurement — the harness, before any policy | Baseline p99 TTFT with CI from ≥3 runs, reproducible by one command; open-vs-closed-loop coordinated-omission demo | Done |
| R0.3 | The core idea — prefix-affinity routing | First affinity-vs-round-robin comparison with CIs on mocks; hash-correctness test passing | Done, except the half of the hash check that needs vLLM |
| R0.4 | Load-aware and session-aware | A workload where pure affinity loses and balanced affinity wins, documented | Done |
| R0.5 | Reality, and ship | A stranger can `docker compose up`, run `make bench`, and regenerate every published number | Next |

Appendix A, closed until R0.5 ships: A1 reliability engineering, A2 precise
indexing via vLLM KV events, A3 agentic workloads, A4 a blog post, A5
disaggregation or sharded routers.

### What is still open

Comparing the router's predicted hit rate against vLLM's own
`prefix_cache_queries` and `prefix_cache_hits` on real hardware. Until then the
mock worker's agreement with the router is not evidence, since both are models
written by the same person from the same idea. That comparison is an R0.5 exit
criterion.

Docker is not installed on this machine, so the `docker compose up` path is
unverified. That is a real gap for R0.5, whose exit criterion is a stranger
reproducing every number from a clean checkout.

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
  Open-loop Poisson schedule, intended-time latency, warmup exclusion, run
  reports carrying config and seed and git SHA, invalid-run detection, and
  medians with 95% intervals across runs. Router gained Prometheus metrics and
  a round-robin baseline; the mock gained bounded concurrency with queueing.
- **R0.3 shipped.** Prefix-affinity routing, measured against the baseline.
  Full-conversation chat template rendering, tokenization, and a chained block
  hash in `warmpath-core`, shared by router and mock so each fingerprints
  independently. An approximate block index: a flat map from block hash to
  worker bitset, which answers prefix queries exactly as a radix tree would
  because the hash chain already encodes the prefix, with leaf-first eviction
  and in-flight block reservation. Then the model's own tokenizer and chat
  template, which closed the part of the highest-risk item that does not need
  hardware.
- **R0.4 shipped.** Load-aware and session-aware routing.
  - The mock worker exposes vLLM-named metrics; the router polls them for queue
    depth, KV utilization, and the worker's own prefix cache counters.
  - The balanced score weighs match ratio against whichever of queue headroom
    and memory headroom is tighter.
  - Health checking with ejection and re-admission, and one retry elsewhere
    when a worker never answered.
  - `least-loaded` and `power-of-two` baselines, so the field is fair.
  - Session affinity via an `x-session-id` header, bounded and composable.
  - A skew knob in the load generator, and a policy matrix across workload
    shapes.
  - Result: on skewed traffic naive affinity posts the best hit rate in the
    field and a median 64 times worse than round-robin, with throughput
    dropping below the offered rate, because 80% of requests land on one
    worker. Hit rate is not the objective.
  - Second result, less flattering and worth keeping: round-robin wins the
    skewed workload outright. Heavy skew puts the hot prefix in every worker's
    cache, so rotation gets a 77.5% hit rate for free and cache-aware routing
    has little to win and a hotspot to lose.
  - Third result: `least-loaded` posts a p99 nearly three times round-robin's
    on identical cache behaviour, because a queue depth polled every 100ms is
    stale enough to herd. `power-of-two` sits between them, as its reputation
    says it should.

## What's next

**R0.5 — Reality, and ship.** The only milestone that needs a GPU, and the
point at which the project is finished.

1. Two vLLM instances on rented L4s running Qwen3-1.7B.
2. Validate the router's predicted hit rate against
   `vllm:prefix_cache_queries` and `vllm:prefix_cache_hits`. This is the check
   that stops the router grading its own homework, and the reason the mock's
   agreement with it does not count.
3. Fix whatever the mock got wrong, and publish the divergence either way.
4. Re-run the R0.3 and R0.4 comparisons on real hardware.
5. Flamegraph the router and publish its own added latency, flattering or not.
6. Negative-results pass: at least one documented losing regime. Two are
   already in `RESULTS.md`, so this is mostly a matter of confirming they
   survive real workers.
7. Docker Compose, so a stranger can bring the stack up. Untested so far,
   because Docker is not installed on this machine.
8. `docs/DESIGN.md` with alternatives considered and what would change at a
   hundred workers. Apache-2.0, README with the headline chart, repo hygiene.

## Open risks to watch

From `project_spec.md` §2.2 — surface these if they start to materialize:
- Mock cache model might not be realistic enough to survive real vLLM (checked at the R0.5 gate; divergence is itself a publishable finding).
- **Tokenization mismatch with vLLM, the highest-risk item.** It fails silently. Largely closed: the router uses the model's own tokenizer and chat template, and refuses to start rather than falling back. The remaining half needs vLLM, at R0.5.
- Scope creep back into Appendix A. It is closed until R0.5 ships.
- GPU cost overrun (mitigated by bounding validation sessions to spot instances).
