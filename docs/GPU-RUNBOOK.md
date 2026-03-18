# The GPU session

What R0.5 needs from real hardware, in the order it should be done, written to
be followed while the meter is running.

Everything else in R0.5 is finished. This is the part that cannot be faked, and
the reason it cannot be faked is that the mock worker and the router were
written by the same person from the same idea. When they agree, that is one
calculation performed twice.

## Before you start the instances

Have these ready, since none of them need a GPU and all of them cost money to
work out later.

- The repository on a path that is not synced by iCloud. Bind mounts of evicted
  files fail inside containers with an error about deadlock that mentions
  nothing about iCloud.
- `make fetch-model`, so the tokenizer and chat template are local.
- `make check` green.
- A decision about where the load generator runs. See the last section, because
  getting this wrong wastes the whole session for one of the four goals.

## What to rent

Two instances running vLLM with prefix caching on, serving Qwen3-1.7B. Two is
the minimum that makes routing mean anything, and the spec asks for no more.

A third small instance, or your laptop if the network is close enough, for the
router and the load generator.

## 1. Bring vLLM up and confirm the counters exist

The whole validation rests on two vLLM metrics. Check them before anything else,
because if prefix caching is off, every later step produces numbers that look
fine and mean nothing.

```
curl -s http://gpu-0:8000/metrics | grep prefix_cache
```

You want `vllm:prefix_cache_queries_total` and `vllm:prefix_cache_hits_total`.
If they are absent, prefix caching is not enabled and nothing below is worth
running.

## 2. Point the router at them

Copy `config/warmpath.toml`, set the two worker URLs, and set
`[model] directory` to the fetched tokenizer. The router refuses to start if it
cannot load a configured model directory, which is deliberate: against a real
engine, a router using a different tokenizer cuts blocks at different boundaries
and the symptom is a mediocre hit rate rather than an error.

Set `[index] block_size` to whatever vLLM is using. The default of 16 matches
vLLM's default. If they disagree, the router and the engine are describing
different things and the hit rate collapses without any error.

## 3. The one measurement that cannot be done anywhere else

Run a workload with real prefix sharing through the router, then:

```
ROUTER=http://router:8080 \
WORKERS="http://gpu-0:8000 http://gpu-1:8000" \
./scripts/validate-hit-rate.sh
```

It compares the router's own predicted hit rate against what the engines report,
and prints the gap with a verdict. It sends no traffic itself, so run a workload
first.

Use `prefix-affinity-balanced`. A cache-blind policy builds no fingerprint, so
there is no prediction to check and the script says so rather than printing a
zero.

**Publish the gap either way.** A large gap is a finding about the router and a
small one is a finding about the mock. Both are worth more than silence. If the
router is optimistic, suspect block boundaries first: a tokenizer or chat
template that disagrees with the engine produces exactly that shape.

Against the mock this script reports an exact zero gap on identical block
counts. That is not reassurance. The mock computes its block hashes with the
same `warmpath-core` code the router uses, so the two counters are one
calculation done twice, and the exact match only shows the plumbing carries
numbers correctly. vLLM is the first independent opinion this project has ever
had.

## 4. Re-run the comparisons on real workers

`make bench` starts its own mock workers, so it is not the command here. Point
the comparison at the real ones instead, from the load generator machine:

```
WORKER_URLS="http://10.0.0.4:8000 http://10.0.0.5:8000" \
MODEL=Qwen/Qwen3-1.7B \
WORKERS=2 ./scripts/policy-matrix.sh
```

The model name has to be one vLLM will answer to, since it rejects a request
whose model field it does not recognise, and the default is the mock's name.

Two things happen differently against real servers. Each arm asks every worker
to drop its prefix cache first, because the servers keep running between arms
and would otherwise hand the next policy a cache the previous one warmed. If a
worker refuses that request the script says so loudly, and the run is not
comparable. And each arm's cache figures are measured as the difference across
it, since vLLM's counters run for the life of the process.

Expect this to take longer than against mocks, since decode is no longer
simulated. Redraw the charts afterwards with `python3 scripts/plot.py`.

Expect the numbers to move. Real prefill is far more expensive than the mock's
model of it, which should make cache-aware routing look better on the median,
and real decode occupies a worker for much longer, which should make the
hotspot on skewed traffic worse. If neither happens, that is worth understanding
before publishing anything.

## 5. Flamegraph the router

`cargo flamegraph` against the router while a workload runs. The release profile
already carries debug info, so no rebuild is needed.

What to look for: `RESULTS.md` attributes about 1.2ms at the median to building
the fingerprint, roughly two thirds of it tokenization, arrived at by
subtraction rather than by profile. The flamegraph either agrees or it does not,
and either answer is publishable.

## 6. The p99 overhead question, which needs planning rather than hardware

The spec asks for under 1ms added to p99. That is currently unverified, and it
is unverified because the noise floor of one laptop running the load generator,
the router and the worker was wider than the quantity being measured. Every p99
delta came back smaller than its own confidence interval, and one arm reported
the router as faster than not using it.

Renting GPUs does not fix this on its own. It fixes it only if the load
generator runs on a machine that is doing nothing else. Put the generator on its
own instance, the router on another, and run `scripts/overhead.sh` there.

If you cannot separate them, do not publish a p99 overhead number. Say it is
unverified, which is what `RESULTS.md` says today.

## Shutting down

Tear the instances down before writing anything up. The results are files, and
the analysis does not need the hardware.
