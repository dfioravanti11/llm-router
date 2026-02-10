# Warmpath

A KV-cache-aware request router for LLM inference fleets. It is an
OpenAI-compatible HTTP proxy that fronts N inference workers and picks the
worker most likely to already hold a request's prompt prefix in its KV cache,
subject to load and memory pressure.

Working name. Expect it to change before a public release.

## Status

R0.3. Prefix-affinity routing works and beats round-robin on the median, on a
workload with prefix reuse. `RESULTS.md` has the numbers, including the part
that is not yet established and the regime where the whole idea buys nothing.

What works today:

- OpenAI-compatible `/v1/chat/completions` and `/v1/completions`, streaming and
  non-streaming, with SSE output byte-identical to what the worker wrote.
- A client disconnect cancels the upstream request and frees the worker slot.
- Prompt building: the full conversation rendered through a chat template,
  tokenized, and cut into a chained block hash per 16 tokens.
- An approximate block index inferred from the router's own dispatches, with
  leaf-first least-recently-used eviction and in-flight block reservation.
- Four policies, switchable in config: `round-robin` and `first` as baselines,
  `prefix-affinity` and `prefix-affinity-balanced`.
- Prometheus metrics and a Grafana dashboard.
- `warmpath-bench`: an open-loop load generator with intended-time latency
  accounting, warmup exclusion, run manifests carrying config and seed and git
  SHA, generator-saturation detection, and confidence intervals across runs.
- A mock worker with bounded concurrency, queueing, and a simulated block-level
  prefix cache, so all of the above runs without a GPU.

Not built yet: worker load and KV-pressure polling, session affinity, and
health checking (R0.4), then real vLLM, the router's own overhead, and the
ship-ready repo (R0.5).

## Layout

| Path | What it is |
|---|---|
| `crates/warmpath` | The router: config, index, policy, worker pool, proxy path |
| `crates/warmpath-core` | Prompt rendering, tokenization, block hashing |
| `crates/warmpath-bench` | The load generator and statistics harness |
| `crates/warmpath-mock` | Mock inference worker for GPU-free development |
| `config/warmpath.toml` | Router configuration |
| `deploy/` | Prometheus scrape config and the Grafana dashboard |
| `scripts/co-demo.sh` | The coordinated-omission comparison |
| `scripts/policy-compare.sh` | Routing policies measured against each other |

## Running it

Two terminals. First the worker:

```
make run-mock
```

Then the router:

```
make run
```

Then send it a request:

```
curl -N http://127.0.0.1:8080/v1/chat/completions \
  -H 'content-type: application/json' \
  -d '{"model":"mock-model","stream":true,"max_tokens":16,
       "messages":[{"role":"user","content":"hello"}]}'
```

## Measuring it

With a router already running, three open-loop runs and their confidence
interval:

```
make bench
```

Two comparisons that start and stop everything themselves:

```
make policy-compare
make co-demo
```

Each run writes a directory under `results/` holding `report.json` (config,
seed, git SHA, validity, latency summaries), `records.jsonl` (one line per
request), and `percentiles.csv` (ready to plot).

`warmpath-bench` works against any OpenAI-compatible endpoint, not just this
router:

```
cargo run --release -p warmpath-bench -- run --target http://your-endpoint:8000
```

## Development

`make check` runs the same gate as CI: format check, clippy with warnings
denied, and the test suite.

## License

Apache-2.0.
