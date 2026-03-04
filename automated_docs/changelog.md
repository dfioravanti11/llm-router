# Changelog

Notable changes to this project, newest first. Update this alongside any commit that changes behavior, architecture, or scope — not for pure prose/typo fixes.

Format per entry: `## YYYY-MM-DD — short title`, then a few bullets of what changed and why (link to the relevant roadmap release from `project_status.md` when applicable).

## 2026-03-04 — Ship preparation, and two silent defects found by writing the docs

- **A stalled worker could freeze the router's whole view of the fleet.** The
  metrics poller walks workers one at a time and shares the proxy's HTTP client,
  whose read timeout is measured in tens of seconds because a long generation is
  healthy. A worker that accepted the connection and then said nothing held the
  loop for a minute, and routing ran on load figures from a minute ago. The
  metrics fetch now carries its own timeout of one poll interval. A regression
  test stalls one worker and asserts the next is still read; it fails without the
  fix.
- **`--session-turns` did nothing and was recorded in every run manifest.** The
  generator only ever builds one system message and one user question, and never
  sends an `x-session-id` header. Any value above one described a workload that
  did not happen. It is now refused rather than ignored. The consequence is that
  the router's session affinity has never influenced a published number, and
  `RESULTS.md` now says so instead of calling it merely unvalidated.
- `docs/DESIGN.md`: the request path, six decisions with the cost of being wrong
  for each, what breaks at 100 workers, and what the measurements changed about
  the design. It records that the balance override degenerates to its absolute
  condition whenever any worker is idle, which rarely matters at three workers
  and would disable affinity fleet-wide at a hundred.
- `make bench` now regenerates every published number and then the charts, in
  about an hour, which is what the shipping exit criterion has always claimed it
  did. `make bench-smoke` runs the same pipeline at toy settings in a few
  minutes, into `results/smoke`, which is gitignored and marked not publishable.
  The old quick run is `make bench-one`.
- `scripts/plot.py` and four charts under `docs/charts`, redrawn from committed
  data with no arguments. CDFs on a log tail axis rather than bar charts of
  means, every run drawn behind its median, and intervals that contain zero
  drawn as containing zero. The README now opens with one.
- The compose stack was brought up and verified end to end: streaming
  completions through the router, all four Prometheus targets up, and Grafana
  serving the provisioned dashboard against live router metrics. Three things
  had to be fixed. Four services building the same image tag raced inside
  containerd and wedged the daemon, so only one service builds now. A missing
  `.dockerignore` was uploading an 8.8GB build context. And bind-mounted config
  files that iCloud had evicted fail to read inside the container with a
  confusing deadlock error, which is documented in the compose header.
- `LICENSE`, the Apache-2.0 text the manifests have claimed since R0.1.
- The working-name caveat is gone from the README. The project is called
  Warmpath.
- Verified that the Hugging Face token that was briefly in `.env.example` never
  entered git history. Every commit carrying that file has the placeholder.

## 2026-02-28 — Router overhead measured, and the compose stack

- `scripts/overhead.sh` and `make overhead` measure what the router costs, by
  running the same near-free worker three ways: direct, through the router
  without reading the prompt, and through the router doing the full fingerprint.
  One worker in every arm, so nothing measured is a routing decision.
- Result: proxying costs under about 0.3ms and cannot be separated from zero.
  Building the fingerprint costs 1.2ms at the median, which holds under both
  clocks and is four times its own confidence interval. Roughly two thirds of
  that is tokenizing a 280 word prompt.
- The spec's actual requirement is under 1ms at p99, and that came back
  unresolved: every p99 interval is wide enough to contain zero. An earlier
  version of the experiment reported the router as 4.33ms *faster* than not
  using it, which is what a measurement with no resolution looks like. Recorded
  as unverified rather than passed.
- `compose.yaml` and a `Dockerfile` bring up the router, three mock workers,
  Prometheus, and Grafana with the dashboard provisioned. Prometheus now scrapes
  the workers as well as the router, since the workers report the hit rate the
  router does not control. Unverified: Docker is not installed on this machine.
- Benchmark artifacts are committed from this point on, per the spec's
  reproducibility requirement. Per-request record streams and service logs stay
  ignored.

## 2026-02-21 — R0.4 load-aware and session-aware

- The mock worker exposes `/metrics` using vLLM's own metric names, so the
  router's poller and parser are exercised against the format they meet at R0.5
  rather than a project-specific one.
- The router polls every worker for queue depth, KV utilization, and its own
  prefix cache counters. Load is now the worker's view rather than the router's
  in-flight count, which cannot see work queued inside the engine and cannot see
  memory pressure at all.
- The balanced score combines match ratio with queue headroom and KV headroom,
  taking whichever is tighter. A worker holding a whole prefix but out of memory
  can now lose to one with room.
- Health checking with ejection after repeated failed polls and re-admission
  after repeated successes. A request whose worker never answered is retried
  once elsewhere, which is safe only because nothing has streamed yet.
- `least-loaded` and `power-of-two` cache-blind baselines, so the policy field
  is fair.
- Session affinity: clients set `x-session-id` and the conversation sticks to
  one worker. Layered on top of any policy and yields to health and to the
  balance override. The map is bounded, since the ids come from clients.
- `warmpath-bench` gains a skew knob, so most requests can share one prefix as
  real traffic does, and `scripts/policy-matrix.sh` runs every policy against
  every workload shape.
- Result: on skewed traffic naive affinity records the highest cache hit rate in
  the field and a median 64 times worse than round-robin, with throughput
  dropping below the offered rate, because it drives 80% of requests onto one
  worker. The balanced policy holds throughput and keeps a better hit rate than
  the cache-blind baselines. Hit rate is not the objective.
- Two findings that do not flatter the router, published alongside the wins.
  Round-robin wins the skewed workload outright, because heavy skew puts the hot
  prefix in every worker's cache and leaves nothing to arrange. And
  `least-loaded` posts a p99 nearly three times round-robin's on identical cache
  behaviour, because a queue depth polled every 100ms is stale enough to herd.
- `RESULTS.md` carries both workload matrices with confidence intervals, plus a
  statement of the band in which cache-aware routing pays at all.

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
