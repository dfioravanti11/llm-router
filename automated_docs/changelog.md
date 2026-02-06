# Changelog

Notable changes to this project, newest first. Update this alongside any commit that changes behavior, architecture, or scope — not for pure prose/typo fixes.

Format per entry: `## YYYY-MM-DD — short title`, then a few bullets of what changed and why (link to the relevant roadmap release from `project_status.md` when applicable).

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
