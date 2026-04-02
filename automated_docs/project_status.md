# Project Status

> Update this file whenever a milestone's exit criteria are met, scope changes, or the "what's next" changes. Milestones and exit criteria are defined in `project_spec.md` §3 (Release Roadmap) — this file tracks progress against them, it doesn't redefine them.

## Current phase: R0.5, everything that does not need a GPU

The router builds a prompt fingerprint with the model's own tokenizer, keeps an
approximate block index with in-flight reservation, polls each worker for queue
depth and KV pressure, and chooses among six policies. Health checking, a single
retry, and session affinity are in.

R0.5 is the ship point. Everything that does not need a GPU is done. The GPU
half is now partly done: on 2026-04-06 the router ran against real vLLM on one
L4, the prefix cache hit rate reproduced the simulated result, and the
predicted-versus-actual gap was measured at +8.9 points. That closes the
credibility gate the milestone was built around.

What one GPU could not settle is latency. Two vLLM servers sharing one device
contend hard enough to swamp the effect, so the tail comparison and the router's
own overhead both remain unverified. They need two devices.

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

The `docker compose up` path has now been brought up and verified end to end:
streaming completions through the router, all four Prometheus targets up, and
Grafana serving the provisioned dashboard against live router metrics. What is
left of the exit criterion is the GPU half.

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
5. Flamegraph the router. Its added latency is now measured by subtraction in
   `RESULTS.md`: under about 0.3ms to proxy, and 1.2ms at the median to build
   the fingerprint, roughly two thirds of which is tokenizing. The spec's
   requirement is stated at p99, and p99 came back unresolvable, because the
   noise floor of one laptop running the generator, the router and the worker
   is larger than the quantity. Redo it with the generator on its own machine.
6. Negative-results pass: at least one documented losing regime. Three are
   already in `RESULTS.md`, so this is mostly a matter of confirming they
   survive real workers.
7. Re-measure router overhead now that the 40ms Nagle stall is fixed. Every
   latency figure in `RESULTS.md` predates the fix, and all of them were taken
   over loopback on a laptop where the stall never fired, so they are probably
   unaffected. Probably is not measured.
8. The tail latency comparison, on two separate GPUs. One device cannot show a
   worker saturating while another idles, and that mechanism is what the skewed
   traffic result rests on.
9. The sub-millisecond p99 overhead figure, on machines that are not shared. It
   has now failed to resolve three times for the same reason.

## Open risks to watch

From `project_spec.md` §2.2 — surface these if they start to materialize:
- Mock cache model might not be realistic enough to survive real vLLM (checked at the R0.5 gate; divergence is itself a publishable finding).
- **Tokenization mismatch with vLLM, the highest-risk item.** It fails silently. Largely closed: the router uses the model's own tokenizer and chat template, and refuses to start rather than falling back. The remaining half needs vLLM, at R0.5.
- Scope creep back into Appendix A. It is closed until R0.5 ships.
- GPU cost overrun (mitigated by bounding validation sessions to spot instances).
