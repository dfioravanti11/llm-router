# Warmpath

A KV-cache-aware request router for LLM inference fleets. It is an
OpenAI-compatible HTTP proxy that fronts N inference workers and picks the
worker most likely to already hold a request's prompt prefix in its KV cache,
subject to load and memory pressure.

![Time to first token by policy, even prefix popularity](docs/charts/even-ttft-tail.png)

Time to first token against three mock workers, offered open loop at 60 requests
a second. Every policy sees the same workload and the same seeds. The step near
42ms is a cache miss, and the two cache-aware policies mostly stay left of it.
Regenerate with `make bench`.

## Status

R0.4. Prefix-affinity routing works, and the conditions under which it works
are measured. On even traffic with room in the fleet, balanced affinity cuts
p99 time to first token from 46.0ms to 17.2ms against round-robin, with
non-overlapping confidence intervals.

On skewed traffic the naive form drives 80% of requests onto one worker and
posts a median 64 times worse than round-robin, while recording the best cache
hit rate in the field. The balanced form holds throughput and keeps most of the
hit rate. Round-robin beats both, because heavy skew makes the hot prefix fit in
every worker's cache and there is nothing left to arrange.

`RESULTS.md` has the numbers, the confidence intervals, and the boundaries of
where the technique pays at all.

What works today:

- OpenAI-compatible `/v1/chat/completions` and `/v1/completions`, streaming and
  non-streaming, with SSE output byte-identical to what the worker wrote.
- A client disconnect cancels the upstream request and frees the worker slot.
- Prompt building: the full conversation rendered through the model's own chat
  template, tokenized with the model's own tokenizer, and cut into a chained
  block hash per 16 tokens.
- An approximate block index inferred from the router's own dispatches, with
  leaf-first least-recently-used eviction and in-flight block reservation.
- Six policies, switchable in config: `round-robin`, `least-loaded`,
  `power-of-two`, and `first` as cache-blind baselines, plus `prefix-affinity`
  and `prefix-affinity-balanced`.
- Worker state polled from each worker's `/metrics` in vLLM's own format: queue
  depth, KV utilization, and the worker's own prefix cache hit rate.
- Health checking with ejection and re-admission, and a single retry on another
  worker when the first never answered.
- Session affinity: a client sets `x-session-id` and its conversation sticks to
  one worker unless that worker fails or the fleet needs rebalancing.
- Prometheus metrics and a Grafana dashboard.
- `warmpath-bench`: an open-loop load generator with intended-time latency
  accounting, warmup exclusion, run manifests carrying config and seed and git
  SHA, generator-saturation detection, and confidence intervals across runs.
- A mock worker with bounded concurrency, queueing, and a simulated block-level
  prefix cache, so all of the above runs without a GPU.

Not built yet: real vLLM, which is the whole of R0.5 and the ship point.

## Layout

| Path | What it is |
|---|---|
| `crates/warmpath` | The router: config, index, policy, worker pool, proxy path |
| `crates/warmpath-core` | Prompt rendering, tokenization, block hashing |
| `crates/warmpath-bench` | The load generator and statistics harness |
| `crates/warmpath-mock` | Mock inference worker for GPU-free development |
| `config/warmpath.toml` | Router configuration |
| `deploy/` | Prometheus scrape config, Grafana dashboard and provisioning |
| `compose.yaml` | Router, three mock workers, Prometheus, Grafana |
| `docs/DESIGN.md` | Decisions, their costs, and what breaks at 100 workers |
| `Dockerfile` | Builds the router and the mock worker into one image |
| `scripts/reproduce.sh` | Regenerates every published number, behind `make bench` |
| `scripts/plot.py` | Redraws every chart from committed data |
| `scripts/co-demo.sh` | The coordinated-omission comparison |
| `scripts/policy-compare.sh` | Routing policies on one workload shape |
| `scripts/policy-matrix.sh` | Every policy against every workload shape |
| `scripts/overhead.sh` | What the router itself costs, against one worker |
| `scripts/fetch-model.sh` | Downloads the model tokenizer and chat template |

## Running it

The whole stack, with three workers, Prometheus, and a Grafana dashboard:

```
docker compose up --build
```

The router is then on `http://localhost:8080` and Grafana on
`http://localhost:3000` with the dashboard already loaded and no login. The
workers are mocks, so this is somewhere to watch the router route rather than a
measurement of inference.

To run it from source instead, the router wants the target model's tokenizer and
chat template, so it cuts prompts into the same blocks a worker will. Only those
two files are needed, not the weights:

```
make fetch-model
```

Then two terminals. First the worker:

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

Every number published in `RESULTS.md` regenerates from one command. It takes
about an hour and starts and stops everything it needs:

```
make bench
```

The same pipeline at toy settings, to check the machinery works before
committing an hour to it. Output goes to `results/smoke` and is not publishable,
since a single short run cannot support a confidence interval:

```
make bench-smoke
```

The pieces, if you want one on its own:

```
make policy-matrix
make co-demo
make overhead
```

Each run writes a directory under `results/` holding `report.json` (config,
seed, git SHA, validity, latency summaries), `records.jsonl` (one line per
request), and `percentiles.csv` (ready to plot). Everything except the
per-request record stream is committed, so the data behind every published
number is in the repository.

`warmpath-bench` works against any OpenAI-compatible endpoint, not just this
router:

```
cargo run --release -p warmpath-bench -- run --target http://your-endpoint:8000
```

## Development

`make check` runs the same gate as CI: format check, clippy with warnings
denied, and the test suite.

## Design

`docs/DESIGN.md` covers the decisions, what each one costs if it is wrong, and
what breaks at a hundred workers. `automated_docs/architecture.md` describes the
structure as built.

## License

Apache-2.0. See `LICENSE`.
