# Results

Every number here regenerates from a command in this repo. Numbers measured
against the mock worker say so, because a mock is not evidence about real
inference; it is evidence about the harness and the router.

Real vLLM enters at R0.5. Nothing below has touched a GPU.

## Prefix-affinity routing against round-robin

**Command:** `make policy-compare`

Three mock workers. The workload is a pool of ten shared prefixes of 256 words
each, and every request carries one prefix plus its own short question. Sixty
arrivals per second, open loop, three repetitions per policy, about 5,400
measured requests each.

Prompts are rendered with Qwen3-1.7B's own chat template and tokenized with its
own tokenizer, so the block boundaries are the ones a real worker would use. At
about 21 blocks per request (37,722 blocks queried per run across roughly 1,800
requests), ten prefixes come to a working set near 200 blocks.

Workers are restarted for every arm, so no policy inherits a cache another
warmed, and the arm order rotates between repetitions.

### With enough cache in the fleet

Each worker holds 112 blocks, so the fleet holds 336 against a working set near
200. No single worker can hold it; the fleet can, once the prefixes are
partitioned.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | worker-reported hit rate |
|---|---:|---:|---:|---:|---:|
| round-robin | 59.0/s | 13.4 ms | 47.5 ms | [41.1, 53.2] ms | 52.1% |
| prefix-affinity | 59.0/s | 12.4 ms | 19.5 ms | [9.1, 33.2] ms | 88.4% |

This is the result the project exists to produce. The p99 intervals do not
overlap, so the tail improvement is real at three runs: 47.5ms against 19.5ms.
The workers' own counters, which the router does not control, go from 52.1% to
88.4% of blocks served from cache.

The medians are close because at this capacity round-robin already hits often
enough that a typical request is mostly cached. The difference is in the tail,
which is exactly where it matters and exactly what a closed-loop harness would
have hidden.

### With the fleet slightly under-provisioned

**Command:** `CACHE_BLOCKS=64 make policy-compare`

Each worker holds 64 blocks, so the fleet holds 192 against the same working set
near 200. Even a perfect partition does not quite fit.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | worker-reported hit rate |
|---|---:|---:|---:|---:|---:|
| round-robin | 59.0/s | 41.0 ms | 44.6 ms | [39.1, 53.5] ms | 31.3% |
| prefix-affinity | 59.0/s | 12.2 ms | 44.3 ms | [40.9, 49.3] ms | 80.9% |
| prefix-affinity-balanced | 59.0/s | 13.6 ms | 46.1 ms | [8.1, 104.2] ms | 81.4% |

Here the median improves by 3.4x and the hit rate by more than half, and the
**p99 does not move at all**.

The mechanism is visible once the numbers are read together. Time to first
token is a small fixed cost plus prefill for whatever was not cached, so a full
miss on a 20 block prefix costs about 45ms and a full hit costs almost nothing.
Affinity moves most requests from miss to hit, which is the median. But with the
fleet total below the working set, eviction churn never stops, so about one
request in a hundred still misses completely no matter where it is sent. The
worst one percent is made of full misses under every policy, and that is the
p99.

Together the two tables say something neither says alone: **cache-aware routing
needs the fleet to have enough aggregate cache to hold the working set.** Below
that line it still improves the median, and it stops improving the tail.

### The other crossover: when affinity buys nothing at all

Covered by a test, in `crates/warmpath-bench/tests/affinity.rs`.

Shrink the pool so the whole working set fits in *every* worker's cache and
round-robin hits on about 91% of blocks unaided. Affinity measures the same,
because there is nothing left to arrange.

So the technique pays inside a band: the working set has to exceed one worker's
cache, and the fleet has to have room for it. Below the band it is unnecessary;
above it, it helps the median but not the tail.

This was found by accident. The first version of the comparison used three
prefixes, round-robin scored 91.2% against affinity's 94.4%, and the workload
turned out not to be oversubscribed at all. It was not measuring routing.

### What is not being claimed

The balanced policy is not distinguishable from the plain one on this workload.
Nothing here creates a hotspot, so the balance override never fires and the two
make the same choices. Its wide interval in the under-provisioned table
([8.1, 104.2] ms) is noise, not a signal. Building the workload where the two
diverge is R0.4.

The mock worker's cache and the router's index are separate implementations on
purpose, but both are models of the same idea, and they agree partly because one
person wrote both with the same idea in mind. Agreement between them is not
evidence. The predicted-versus-actual hit rate check only becomes real at R0.5,
where the worker's cache is vLLM's and not a model at all.

### Tokenizer choice changes the measurement

An earlier version of this comparison used a development whitespace tokenizer
instead of the model's. On the identical workload it reported an 89.1% hit rate
where the real tokenizer reports 80.9%, because the two cut the prompt into
blocks at different places.

Neither number is wrong about its own experiment, which is the problem: the
difference is invisible from inside a run. It is a small, concrete instance of
the failure the spec names as the project's highest risk, and the reason the
router now refuses to start when a configured model directory will not load
rather than falling back to the development tokenizer.

## Closed-loop load generators under-report the tail

**Command:** `make co-demo`

The same worker, measured two ways.

The worker serves two requests at a time and takes about 46ms per response end
to end, so it tops out near 43 requests per second. The open-loop generator
offers 38 arrivals per second on a schedule fixed before the run starts. The
closed-loop generator runs two callers that each wait for a response before
sending the next.

Three repetitions each, 30 seconds per run with a 5 second warmup, mock worker,
one laptop:

| generator | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 |
|---|---:|---:|---:|---:|
| open loop, 38/s offered | 36.7/s | 22.4 ms | 116.9 ms | [87.6, 154.2] ms |
| closed loop, 2 callers | 46.1/s | 14.0 ms | 16.2 ms | [15.7, 17.0] ms |

The closed-loop harness moved *more* traffic and reported a p99 about seven
times better.

That combination is the whole point. It is not that the closed-loop run was
measuring a lighter load. It is that a closed-loop generator cannot produce the
load shape that causes tail latency at all. When a response is slow, its caller
simply does not send the next request, so arrivals thin out exactly when the
queue would have formed. Two callers keep two slots busy and nothing ever
waits. The generator has coordinated with the system under test, which is where
the name comes from.

An open-loop generator has no such feedback. Arrivals are scheduled before the
run starts, so when a burst lands on a busy worker the queue is real, the wait
is real, and it lands in the histogram.

This is why the open-loop requirement is not a preference. A routing change
evaluated on a closed-loop harness is evaluated in a regime where the tail has
been engineered away, and tail latency under load is the entire reason to care
about routing.

### On picking the offered rate

The default offers 38 against a capacity near 43, which is high utilization
with a queue that still reaches a steady state.

Raising `RATE` past capacity makes the gap far more dramatic and much less
meaningful. A persistently overloaded open-loop system has no steady state: the
queue grows for as long as the run lasts, so the reported p99 becomes a
property of the run length rather than of the system. A 30 second run at 150/s
against this worker reports a p99 TTFT near 55 seconds, and a 60 second run
would report roughly twice that. The comparison is still directionally true;
the number is not worth publishing.

### The same defect, inside one run

Every request the harness sends is timed twice: once from the moment it was due
according to the schedule, and once from the moment it was actually sent. On a
healthy run the two agree. When the generator itself falls behind, they
diverge, and the second number hides the difference.

The per-run report carries both, and `omission_gap_ttft` is their ratio at each
percentile. A run whose p99 dispatch lag exceeds its budget is marked invalid
and excluded from any campaign it belongs to, with the reason recorded. During
development the demo script was briefly broken so that neither service started;
the harness marked all three runs invalid on a 100% error rate and refused to
produce a campaign, which is the behaviour that makes the rest of this file
worth reading.

## What is not measured yet

- Anything on real hardware. R0.5.
- A workload where the naive affinity policy hotspots and the balanced one
  wins. R0.4.
- The router's own added latency. R0.5 measures it with a flamegraph and
  publishes the number whether or not it is flattering.
