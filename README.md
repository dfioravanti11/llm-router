# Warmpath

A KV-cache-aware request router for LLM inference fleets. It is an
OpenAI-compatible HTTP proxy that fronts N inference workers and picks the
worker most likely to already hold a request's prompt prefix in its KV cache,
subject to load and memory pressure.

Working name. Expect it to change before a public release.

## Status

R0.1. The router is a correct streaming proxy and nothing more: no prompt
builder, no block index, no routing policy. Those arrive in R0.3.

What works today:

- OpenAI-compatible `/v1/chat/completions` and `/v1/completions`, streaming and
  non-streaming.
- SSE passthrough that is byte-identical to what the worker wrote.
- A client disconnect cancels the upstream request and frees the worker slot.
- TOML config, structured logging, a health endpoint.
- A mock worker, so everything above runs without a GPU.

Both correctness claims are covered by tests in
`crates/warmpath/tests/proxy.rs`.

## Layout

| Path | What it is |
|---|---|
| `crates/warmpath` | The router: config, worker pool, proxy path |
| `crates/warmpath-mock` | Mock inference worker for GPU-free development |
| `config/warmpath.toml` | Router configuration |

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

## Development

`make check` runs the same gate as CI: format check, clippy with warnings
denied, and the test suite.

## License

Apache-2.0.
