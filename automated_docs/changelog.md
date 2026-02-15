# Changelog

Notable changes to this project, newest first. Update this alongside any commit that changes behavior, architecture, or scope — not for pure prose/typo fixes.

Format per entry: `## YYYY-MM-DD — short title`, then a few bullets of what changed and why (link to the relevant roadmap release from `project_status.md` when applicable).

## 2026-02-15 — Real tokenizer and chat template

- `warmpath-core` gains `HuggingFaceTokenizer` and `ModelFiles`, which load a
  model's `tokenizer.json` and the chat template out of its
  `tokenizer_config.json`. `scripts/fetch-model.sh` downloads both for
  Qwen3-1.7B, which is ungated so no token is needed.
- Both the router and the mock worker take a model directory. A configured
  directory that fails to load is a startup error rather than a fall back to the
  development tokenizer, because that fall back is precisely the silent failure
  this addresses.
- Hugging Face chat templates are rendered by Python Jinja and call Python
  string methods. Qwen3's uses `startswith`, `split`, and `strip`, none of which
  minijinja has, so `minijinja-contrib`'s pycompat callback is now installed.
  Without it the model's own template does not render.
- Re-ran the policy comparison under the real tokenizer. The hit rate that the
  development tokenizer reported as 89.1% is 80.9% under the model's own, since
  the two cut blocks in different places.
- New result, both regimes in `RESULTS.md`: with the fleet holding 336 blocks
  against a working set near 200, affinity cuts p99 TTFT from 47.5ms to 19.5ms
  with non-overlapping intervals. With the fleet at 192, below the working set,
  the median still improves 3.4x and the p99 does not move, because the worst
  one percent is full misses under every policy. Cache-aware routing needs the
  fleet to have room for the working set.
- Replaced a self-contradictory test that allowed 50ms of dispatch lag and then
  asserted the two clocks agreed within 10ms.

## 2026-02-10 — R0.3 prefix-affinity routing

- New crate `warmpath-core`: chat template rendering, tokenization, and the
  chained block hash. Shared by the router and the mock worker so each computes
  hashes from the request body independently.
- Block hashes chain parent into child, so a hash identifies a prefix rather
  than a block. That is what lets the index be a flat map instead of a radix
  tree, and prefix matching cost O(prompt blocks).
- Approximate block index with in-flight block reservation, so a burst of
  identical prefixes is not scattered before the first one completes.
- Eviction is leaf-first, not plain least-recently-used. Plain LRU evicts a
  chain's *first* block, which strands every block behind it and collapses the
  modelled hit rate to zero while the modelled memory stays full.
- Policies `prefix-affinity` and `prefix-affinity-balanced`, with cache and
  balance thresholds. The plain one ignores load on purpose, so R0.4 can find
  the workload where it hotspots.
- Mock worker gains a simulated block-level prefix cache and prefill cost, which
  is what makes cache-aware routing observable without a GPU. It exports
  `prefix_cache_queries` and `prefix_cache_hits` in vLLM's shape.
- `warmpath-bench` gains a workload with real prefix sharing: a pool of shared
  prefixes, each request adding its own question.
- `scripts/policy-compare.sh`: restarts workers per arm, rotates arm order
  across repetitions, and reports the workers' own hit rate alongside latency.
- Result: on an oversubscribed working set, affinity raises the workers'
  reported hit rate from 35.6% to 89.1% and cuts p50 TTFT from 38.3ms to 8.5ms
  at equal throughput. The p99 confidence intervals overlap at three runs, so
  the tail improvement is not yet established, and `RESULTS.md` says so.
- First documented crossover: when the working set fits in every worker's
  cache, round-robin already hits on ~91% of blocks and affinity adds nothing.
- Config sections now default field by field, so a partial `[server]` or
  `[index]` table no longer fails to parse.
- Spec cut to v3.0 mid-milestone: R0.1 through R0.5 are the commitment, and
  what were R0.6 through R1.0 became Appendix A. Renumbered every stale release
  reference in code comments and docs.
- Added `automated_docs/retrospective.md`, a running log of findings and wrong
  turns kept for an eventual write-up.
- Next: close the tokenizer and block-hash fidelity gap, which the spec names as
  the highest-risk item, then R0.4.

## 2026-02-06 — R0.2 measurement harness

- New crate `warmpath-bench`: an open-loop load generator usable against any
  OpenAI-compatible endpoint. Poisson arrival schedule computed before the run
  starts; latency recorded as `t_response - t_intended`; warmup exclusion;
  HdrHistogram summaries; per-request JSONL; run reports carrying config, seed,
  and git SHA; median and 95% confidence intervals across runs.
- Every request is timed twice, from its intended dispatch time and from its
  actual one, so the coordinated-omission gap falls out of any run.
- A run whose p99 dispatch lag exceeds its budget, or whose error rate exceeds
  1%, is marked invalid and excluded from its campaign with the reason recorded.
- Router: Prometheus metrics at `/metrics` (request outcomes, routing decisions
  by worker and policy, TTFB and end-to-end histograms, in-flight gauge,
  ingress rejections) and a `round-robin` policy alongside `first`, selected
  under a new `[routing]` config section.
- Mock worker: bounded concurrency with real queueing, which is what makes an
  overloaded worker reproducible without a GPU.
- Fixed a deadlock in metric handle resolution. `Family::get_or_create` returns
  a guard holding a read lock and takes a write lock for a new label set, so
  several of them as temporaries in one struct literal deadlocked the thread.
  This would have hung the router at startup, not only under test.
- `RESULTS.md` with the first measured finding: against the same worker at high
  utilization, a closed-loop generator moved more traffic than an open-loop one
  and reported a p99 TTFT about seven times better (16.2ms against 116.9ms).
  Reproduces with `make co-demo`.
- Fixed `achieved_rate_per_second`, which divided successful measured requests
  by the whole wall clock including warmup. That deflated the rate by the
  warmup fraction and made open-loop and closed-loop throughput look different
  when they were not.
- Grafana dashboard and Prometheus scrape config under `deploy/`.
- Next: R0.3, prefix-affinity routing.

## 2026-02-03 — R0.1 skeleton

- Cargo workspace with two crates: `warmpath` (the router) and `warmpath-mock`
  (a GPU-free mock worker). Rust toolchain pinned via `rust-toolchain.toml`.
- Router: TOML config with validation, worker pool, streaming proxy for
  `/v1/chat/completions` and `/v1/completions`, health endpoint, structured
  logging, request ids spanning ingress to worker.
- Mock worker: deterministic OpenAI-compatible output, configurable TTFT and
  inter-token delay, slot counters at `/debug/stats`.
- Both R0.1 exit criteria covered by tests. SSE bytes through the router are
  byte-identical to the worker's own output; a client that hangs up mid-stream
  frees the worker slot, recorded as cancelled rather than completed.
- GitHub Actions CI and a `make check` target running fmt, clippy with warnings
  denied, and the test suite.
- Replaced the live Hugging Face token in `.env.example` with a placeholder.
  The token was never committed, but it should be rotated.
- Next: R0.2, the measurement harness.

## 2026-02-01 — Project scaffolding
- Repo initialized. Added `README.md`, `project_spec.md` (v2.0, full product + engineering spec), `writing_prompt.md` (prose style rules), `.env.example`.
- Added `CLAUDE.md` with architecture summary and engineering requirements for AI-assisted development.
- Added `automated_docs/` (`architecture.md`, `changelog.md`, `project_status.md`) to track live architecture and progress against the R0.1–R1.0+ roadmap.
- No application code yet. Next milestone is R0.1 — Skeleton (see `project_status.md`).
