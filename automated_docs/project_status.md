# Project Status

> Update this file whenever a milestone's exit criteria are met, scope changes, or the "what's next" changes. Milestones and exit criteria are defined in `project_spec.md` §3 (Release Roadmap) — this file tracks progress against them, it doesn't redefine them.

## Current phase: R0.3 complete, starting R0.4

Prefix-affinity routing works and is measured. The router builds a prompt
fingerprint, keeps an approximate block index with in-flight reservation, and
chooses a worker with one of four policies. The load signal is still the
router's own in-flight count rather than anything the worker reports.

## Milestones

Spec v3.0 cut the roadmap from ten releases to five. **R0.1 through R0.5 are
the commitment; everything else is Appendix A and closed until R0.5 ships.**
GPU is required only at R0.5.

| Release | Theme | Exit criterion | Status |
|---|---|---|---|
| R0.1 | Skeleton — correct proxy, does nothing clever | Client disconnect provably frees the worker slot; SSE bytes match upstream exactly | Done |
| R0.2 | Honest measurement — the harness, before any policy | Baseline p99 TTFT with CI from ≥3 runs, reproducible by one command; open-vs-closed-loop coordinated-omission demo | Done |
| R0.3 | The core idea — prefix-affinity routing | First affinity-vs-round-robin comparison with CIs on mocks; hash-correctness test passing | Partly done — see below |
| R0.4 | Load-aware and session-aware | A workload where pure affinity loses and balanced affinity wins, documented | Not started |
| R0.5 | Reality, and ship | A stranger can `docker compose up`, run `make bench`, and regenerate every published number | Not started |

Appendix A, closed until R0.5 ships: A1 reliability engineering, A2 precise
indexing via vLLM KV events, A3 agentic workloads, A4 a blog post, A5
disaggregation or sharded routers.

### R0.3 is not fully closed

The comparison, the index, the reservation, and the policies are done and
tested. What is missing is the part the spec calls the highest-risk item in the
project: the router currently tokenizes with a deterministic whitespace
tokenizer and renders with a chat template of its own design, so nothing has
been checked against a real model's tokenization or against vLLM's block hash
construction. A mismatch there fails silently, producing mediocre hit rates that
read as a weak result rather than a bug.

Part of that needs a GPU and belongs to R0.5. Part of it does not: the real
Qwen3-1.7B tokenizer and chat template can be exercised on a laptop, and vLLM's
hash construction can be implemented from its source. Closing the CPU-side part
is the next work item, ahead of R0.4.

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
  - `warmpath-core`: full-conversation chat template rendering, tokenization,
    and a chained block hash. The router and the mock worker both use it, and
    each fingerprints the request body independently.
  - Approximate block index: a flat map from block hash to worker bitset, which
    answers prefix queries exactly as a radix tree would because the hash chain
    already encodes the prefix. Leaf-first LRU eviction, and in-flight block
    reservation so a burst of identical prefixes is not scattered.
  - `prefix-affinity` and `prefix-affinity-balanced` policies, written as a
    pure function of index answer, load, and a rotation cursor.
  - The mock worker simulates a block-level prefix cache with prefill cost and
    exports vLLM-shaped hit counters.
  - Result in `RESULTS.md`: on an oversubscribed working set, the workers'
    reported hit rate goes from 35.6% to 89.1% and p50 TTFT from 38.3ms to
    8.5ms at equal throughput. The p99 intervals overlap at three runs, so the
    tail claim is explicitly not made.
  - First documented crossover: when the working set fits everywhere, affinity
    buys nothing.

## What's next

**R0.4 — Load-aware and session-aware.** Affinity currently weighs itself
against the router's own in-flight count, which is a proxy for a queue rather
than a reading of one.

1. Poll each worker's `/metrics` for running and waiting counts, KV cache
   utilization, and its own `prefix_cache_queries` / `prefix_cache_hits`. That
   last pair is what turns the predicted-versus-actual hit rate check into
   something the router does not control.
2. Feed KV headroom into the balanced score, replacing the relative-load
   stand-in.
3. Session affinity as a separate composable mechanism, so a multi-turn
   conversation sticks to its worker unless that worker fails or saturates.
4. `power-of-two` and `least-loaded` baselines, for a fair policy field.
5. A skewed workload that hotspots naive affinity. R0.3's comparison found the
   two affinity policies indistinguishable because nothing created a hotspot;
   building the workload where that stops being true is the milestone.
6. More repetitions. R0.3's p99 intervals overlapped at three runs, and R0.4
   asks a finer question than R0.3 did.

## Open risks to watch

From `project_spec.md` §2.2 — surface these if they start to materialize:
- Mock cache model might not be realistic enough to survive real vLLM (checked at the R0.5 gate; divergence is itself a publishable finding).
- **Tokenization/hashing mismatch with vLLM, the highest-risk item.** It fails silently. Currently open: see the R0.3 note above.
- Scope creep back into Appendix A. It is closed until R0.5 ships.
- GPU cost overrun (mitigated by bounding validation sessions to spot instances).
