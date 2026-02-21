# Results

Every number here regenerates from a command in this repo. Numbers measured
against the mock worker say so, because a mock is not evidence about real
inference; it is evidence about the harness and the router.

Real vLLM enters at R0.5. Nothing below has touched a GPU.

## The policy matrix

**Command:** `make policy-matrix`

Three mock workers, ten shared prefixes of 256 words each, and every request
carrying one prefix plus its own short question. Prompts are rendered with
Qwen3-1.7B's chat template and tokenized with its tokenizer, so block boundaries
are the ones a real worker would use. Three repetitions per cell, workers
restarted per arm, arm order rotated between repetitions.

Two workload shapes, because they ask different questions.

### Even traffic: every prefix equally popular

Nothing hotspots here, so this isolates cache locality. Each worker holds 112
blocks against a working set near 200, so no worker can hold it all and the
fleet can.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | hit rate | busiest worker |
|---|---:|---:|---:|---:|---:|---:|
| round-robin | 59.0/s | 13.7 ms | 46.0 ms | [43.0, 49.7] ms | 52.1% | 33% |
| least-loaded | 59.0/s | 12.5 ms | 46.0 ms | [42.8, 48.1] ms | 52.8% | 34% |
| power-of-two | 59.0/s | 12.8 ms | 46.0 ms | [43.6, 47.6] ms | 52.9% | 34% |
| prefix-affinity | 59.0/s | 12.5 ms | 19.8 ms | [-103.7, 214.1] ms | 88.3% | 40% |
| prefix-affinity-balanced | 59.0/s | 12.7 ms | **17.2 ms** | [15.0, 19.8] ms | 88.4% | 40% |

The three cache-blind policies land on the same p99, to the tenth of a
millisecond. Reacting to load cannot help when nothing is overloaded, and here
the tail is made of cache misses. Both cache-aware policies cut it by more than
half and take the hit rate from 52% to 88%.

Note the interval on `prefix-affinity`: a lower bound below zero is not a
latency, it is three runs disagreeing so much that a three-sample interval says
nothing. The balanced policy's interval on the same workload is tight. Naive
affinity is less stable run to run, which turns out to be the mild version of
what the next table shows.

### Skewed traffic: 80% of requests share one prefix

Real prefix popularity is heavily skewed, so this is the more realistic shape.
Each worker now has four serving slots and a request occupies one for about
64ms whether or not its prefill was cached, because decode is not helped by the
prefix cache. Sixty arrivals a second puts the hot worker near 77% utilization
while the fleet as a whole sits at about a third of capacity.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | hit rate | busiest worker |
|---|---:|---:|---:|---:|---:|---:|
| round-robin | 58.9/s | **11.4 ms** | **44.2 ms** | [39.1, 50.9] ms | 77.5% | 33% |
| least-loaded | 58.9/s | 11.8 ms | 126.7 ms | [110.5, 137.7] ms | 77.4% | 34% |
| power-of-two | 58.9/s | 11.5 ms | 74.4 ms | [68.0, 83.3] ms | 77.4% | 34% |
| prefix-affinity | 56.5/s | 726.0 ms | 1295.4 ms | [-608.8, 3705.4] ms | **88.3%** | 80% |
| prefix-affinity-balanced | 58.9/s | 13.3 ms | 120.5 ms | [54.8, 170.6] ms | 85.5% | 37% |

Three things in that table are worth stopping on.

**Naive affinity records the best cache hit rate in the field and a median
sixty-four times worse than the simplest possible policy.** It drives 80% of
requests onto the one worker holding the hot prefix, that worker saturates, and
throughput drops below the offered rate while every other worker idles. The hit
rate keeps climbing the whole time. This is the clearest statement available
that hit rate is not the objective: a router optimising the metric that looks
like success runs the system into the ground, and the metric goes up while it
happens.

**The balanced policy fixes it, and that is the R0.4 exit criterion.** Busiest
worker back to 37%, median back to 13.3ms, throughput restored, and it still
holds a hit rate well above the cache-blind policies. Giving up three points of
hit rate to avoid a hotspot is the whole trade the balanced variant exists to
make.

**Round-robin wins this workload outright, and that is a real result.** When
80% of traffic shares one prefix, that prefix fits comfortably in every worker's
cache, so plain rotation gets 77.5% without trying. Skew makes the caching
problem easy and the balancing problem hard, which is the opposite of the
intuition that skew is where cache-aware routing should shine. On this shape the
router's cleverness is worth nothing and costs a little.

### Load-reactive baselines do worse than plain rotation

`least-loaded` posts a p99 nearly three times round-robin's, and `power-of-two`
sits between them. All three achieve the same hit rate and the same even spread,
so the difference is not cache behaviour.

It is herding. Queue depth is polled every 100ms, so for most of that window
every routing decision sees the same stale snapshot and sends requests to the
same "least loaded" worker until the next poll corrects it. Power-of-two only
ever compares two candidates, so it mitigates the pile-up without eliminating
it, which is the behaviour power-of-two-choices is known for. Round-robin cannot
herd, because it does not look.

Worth remembering when reading the balanced policy's tail here: part of its cost
is the same staleness, not just the affinity it retains.

### When cache-aware routing pays, and when it does not

Putting the tables together with the capacity result below, the technique pays
inside a fairly narrow band:

- The working set must **exceed one worker's cache**, or plain rotation already
  hits on nearly everything and there is nothing to arrange.
- The fleet must have **room for the working set in aggregate**, or the tail is
  made of full misses under every policy and only the median improves.
- Traffic must not be **so skewed that the hot prefix fits everywhere**, or
  rotation gets the hit rate for free and affinity only risks a hotspot.

Inside the band it is a clear win. Outside it, the honest answer is that a
simpler policy is better, and the balanced variant's job is mostly to lose
gracefully.

### Capacity decides whether the tail improves

**Command:** `CACHE_BLOCKS=64 make policy-compare`

With each worker holding 64 blocks, the fleet holds 192 against the same working
set near 200. Even a perfect partition does not quite fit.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | hit rate |
|---|---:|---:|---:|---:|---:|
| round-robin | 59.0/s | 41.0 ms | 44.6 ms | [39.1, 53.5] ms | 31.3% |
| prefix-affinity | 59.0/s | 12.2 ms | 44.3 ms | [40.9, 49.3] ms | 80.9% |
| prefix-affinity-balanced | 59.0/s | 13.6 ms | 46.1 ms | [8.1, 104.2] ms | 81.4% |

The median improves by 3.4x and the p99 does not move at all. Time to first
token is a small fixed cost plus prefill for whatever was not cached, so a full
miss on a 20 block prefix costs about 45ms and a full hit costs almost nothing.
Affinity moves most requests from miss to hit, which is the median. But with the
fleet total below the working set, eviction churn never stops, so about one
request in a hundred still misses completely wherever it is sent. The worst one
percent is full misses under every policy, and that is the p99.

### The other crossover: when affinity buys nothing at all

Covered by a test, in `crates/warmpath-bench/tests/affinity.rs`.

Shrink the prefix pool so the working set fits in *every* worker's cache, and
round-robin hits on about 91% of blocks unaided. Affinity measures the same,
because there is nothing left to arrange.

This was found by accident. The first version of the comparison used three
prefixes, round-robin scored 91.2% against affinity's 94.4%, and the workload
turned out not to be oversubscribed at all. It was not measuring routing.

### What is not being claimed

The mock worker's cache and the router's index are separate implementations on
purpose, but both are models of the same idea, and they agree partly because one
person wrote both with the same idea in mind. Agreement between them is not
evidence. The predicted-versus-actual hit rate check only becomes real at R0.5,
where the worker's cache is vLLM's and not a model at all.

Session affinity is implemented and tested but has not been shown to add
anything on top of prefix affinity. On multi-turn traffic the index already
sends turn N+1 to the worker that served turn N, because the two share a prefix.
Whether the explicit mechanism earns its place is an open question rather than a
claim.

### Tokenizer choice changes the measurement

An earlier version of these comparisons used a development whitespace tokenizer
instead of the model's. On an identical workload it reported an 89.1% hit rate
where the real tokenizer reports 80.9%, because the two cut the prompt into
blocks at different places.

Neither number is wrong about its own experiment, which is the problem: the
difference is invisible from inside a run. It is a small instance of the failure
the spec names as the project's highest risk, and the reason the router now
refuses to start when a configured model directory will not load rather than
falling back.

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
- The router's predicted hit rate against a worker's own reported hit rate,
  where the worker is vLLM rather than a model of vLLM. R0.5.
- The router's own added latency. R0.5 measures it with a flamegraph and
  publishes the number whether or not it is flattering.
- Whether session affinity adds anything on top of prefix affinity. The
  mechanism is implemented and tested; no run has shown it earning its place.
