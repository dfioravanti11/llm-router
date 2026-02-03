# Changelog

Notable changes to this project, newest first. Update this alongside any commit that changes behavior, architecture, or scope — not for pure prose/typo fixes.

Format per entry: `## YYYY-MM-DD — short title`, then a few bullets of what changed and why (link to the relevant roadmap release from `project_status.md` when applicable).

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
