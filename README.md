# Warmpath

A KV-cache-aware request router for LLM inference fleets. It is an
OpenAI-compatible HTTP proxy that fronts N inference workers and picks the
worker most likely to already hold a request's prompt prefix in its KV cache,
subject to load and memory pressure.

Working name. Expect it to change before a public release.

## Status

R0.2. The measurement harness ships before the routing policy it exists to
judge, because a baseline captured by a different harness is not a baseline.

What works today:

- OpenAI-compatible `/v1/chat/completions` and `/v1/completions`, streaming and
  non-streaming, with SSE output byte-identical to what the worker wrote.
- A client disconnect cancels the upstream request and frees the worker slot.
- Round-robin and single-worker routing, switchable in config. Neither looks at
  cache state; they are the baselines R0.3 has to beat.
- Prometheus metrics and a Grafana dashboard.
- `warmpath-bench`: an open-loop load generator with intended-time latency
  accounting, warmup exclusion, run manifests carrying config and seed and git
  SHA, generator-saturation detection, and confidence intervals across runs.
- A mock worker with bounded concurrency and queueing, so all of the above runs
  without a GPU.

Not built yet: the prompt builder, the block index, and prefix-affinity
routing. Those are R0.3.

`RESULTS.md` has the one measured finding so far, on why closed-loop load
generators under-report tail latency.

## Layout

| Path | What it is |
|---|---|
| `crates/warmpath` | The router: config, worker pool, proxy path, metrics |
| `crates/warmpath-bench` | The load generator and statistics harness |
| `crates/warmpath-mock` | Mock inference worker for GPU-free development |
| `config/warmpath.toml` | Router configuration |
| `deploy/` | Prometheus scrape config and the Grafana dashboard |
| `scripts/co-demo.sh` | The coordinated-omission comparison |

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

The coordinated-omission comparison, which starts and stops everything itself:

```
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
