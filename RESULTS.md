# Results

Every number here regenerates from a command in this repo. Numbers measured
against the mock worker say so, because a mock is not evidence about real
inference; it is evidence about the harness and the router.

Real vLLM enters at R0.5. Nothing below has touched a GPU.

Every chart redraws from the committed data with `python3 scripts/plot.py`,
and every number regenerates with `make bench`.

## The policy matrix

**Command:** `make policy-matrix`

Three mock workers, ten shared prefixes of 256 words each, and every request
carrying one prefix plus its own short question. Prompts are rendered with
Qwen3-1.7B's chat template and tokenized with its tokenizer, so block boundaries
are the ones a real worker would use. Three repetitions per cell, workers
restarted per arm, arm order rotated between repetitions.

Two workload shapes, because they ask different questions.

### Even traffic: every prefix equally popular

![Time to first token by policy on even traffic](docs/charts/even-ttft-tail.png)

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

![Naive affinity concentrates traffic and loses the tail](docs/charts/skewed-affinity-hotspot.png)

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

## Against real vLLM, on one L4

**Date:** 2026-04-06. **Hardware:** one NVIDIA L4 on a GCE `g2-standard-8`,
us-central1-a. **Engine:** vLLM 0.27.1, Qwen3-1.7B, `--block-size 16`,
`--num-gpu-blocks-override 112`, `--max-model-len 1024`.

Everything above this section ran against the mock worker. This section did not.

Only one GPU was available, so two vLLM servers shared it, each capped at 112
blocks to reproduce the cache scarcity the mock runs use. That arrangement makes
cache behaviour measurable and latency not, because the two engines take turns on
one device and the contention is larger than the effect. Both halves are reported
below, and the second half is reported as unusable rather than left out.

### The cache result reproduced

| policy | hit rate, mock | hit rate, real vLLM |
|---|---:|---:|
| round-robin | 52.1% | 52.5% |
| prefix-affinity-balanced | 88.4% | 84.3% |

Three runs per policy. The router's model of prefix reuse holds up against a real
engine, and the numbers land within a few points of the simulated ones.

This also settles the question the whole milestone existed for. A worker cannot
report a 52% hit rate on traffic a router scattered, nor 84% on traffic it
gathered, unless the router is cutting blocks at the same boundaries the engine
does. The prompt rendering, the tokenizer, and the block hash chain agree with
vLLM.

### The latency result did not reproduce

| policy | p99 TTFT, mock | p99 TTFT, real vLLM |
|---|---:|---:|
| round-robin | 46.0 ms | 44.6 ms [44.3, 45.1] |
| prefix-affinity-balanced | **17.2 ms** | 45.4 ms [44.9, 46.0] |

The mock predicted the tail would improve by 2.7x. It did not move.

Mean time to first token did improve, from 33.8ms to 30.0ms, and affinity won all
three runs with no overlap between the groups. That is an 11% improvement in a
statistic this project does not otherwise report, and it is recorded here for
completeness rather than as a headline. The median and the tail are what the
policy is sold on, and they did not move.

Two things explain the shape. Hit rate is counted in blocks, so at 84% almost
every request is a partial hit rather than a clean one, and every request still
pays to be scheduled. And the fixed cost of being scheduled is large here,
because two engines are sharing one device.

A cache hit does save real time on this hardware. Sending the same 190-token
prompt to an idle server three times gave 151ms, then 127ms, then 126ms. The
25ms saved is real, and it is smaller than the contention this setup adds.

### What this run cannot say

Nothing about tail latency under cache-aware routing. Two engines on one GPU
cannot show one worker saturating while another sits idle, which is the entire
mechanism the skewed-traffic result depends on. That needs two devices.

### The predicted-versus-actual gap

**Command:** `make validate-hit-rate`

| | rate | total |
|---|---:|---:|
| router predicted | 85.06% | 5,634 blocks |
| workers reported | 76.15% | 92,959 tokens |
| gap | **+8.91%** | |

The router is optimistic by nine points. The likely causes are its in-flight
reservations, which credit a worker with blocks a dispatched request has not
finished writing, and an eviction model that is its own rather than vLLM's.

The two totals count different things, which cost an hour to notice. vLLM counts
prefix cache queries and hits in tokens; its help text says so. The router counts
blocks. Converted at 16 tokens per block the router's 5,634 becomes 90,144
against the engines' 92,959, a 3% difference, which is the agreement it should be
rather than the 16x disagreement it looked like. The comparison script now labels
each side and warns when the two totals drift apart.

## What the router itself costs

![Router overhead against direct-to-worker](docs/charts/router-overhead.png)

**Command:** `make overhead`

The spec asks the router to add under 1ms to p99 time to first token, measured
rather than asserted. This is the measurement, and it does not fully succeed.

One worker in all three arms, so nothing here is a routing decision. The worker
is configured as close to free as it goes, with no simulated prefill and one
token, because the router's work does not depend on how slow the worker is and
every millisecond the worker spends is variance added to a measurement of
something else.

- **direct** sends the load generator straight at the worker.
- **round-robin** goes through the router, which parses the request, proxies it
  and streams it back without ever reading the prompt.
- **prefix-affinity-balanced** goes through the router doing all of its work:
  render the chat template, tokenize, chain the block hashes, query the index,
  reserve the blocks.

Five runs per arm at 50 arrivals a second, arm order rotated, real Qwen3-1.7B
tokenizer.

| arm | p50 TTFT | p99 TTFT |
|---|---:|---:|
| direct | 4.35 +/-0.16 ms | 7.51 +/-2.02 ms |
| round-robin | 4.63 +/-0.24 ms | 8.00 +/-8.25 ms |
| prefix-affinity-balanced | 5.55 +/-0.19 ms | 10.79 +/-5.09 ms |

| added by the router | p50 | p99 |
|---|---:|---:|
| round-robin | +0.28 +/-0.29 ms, unresolved | +0.49 +/-8.49 ms, unresolved |
| prefix-affinity-balanced | **+1.20 +/-0.25 ms** | +3.28 +/-5.48 ms, unresolved |

Being a proxy is nearly free. The 0.28ms is smaller than its own interval, so
this setup cannot separate it from zero, and the honest reading is that it is
under about 0.3ms rather than that it is 0.28ms.

Being cache aware is not free. The 1.20ms is four times its interval and holds
under both clocks, so it is a real cost and it is larger than the whole budget
the spec set for p99.

### The p99 answer is that this laptop cannot answer it

Every p99 delta came back smaller than its own confidence interval. The
round-robin arm's p99 interval is +/-8.25ms around a number near 8ms, which is
wide enough to contain zero and to contain a negative overhead. An earlier
version of this experiment, with the worker's prefill left switched on, actually
reported round-robin as **4.33ms faster** than talking to the worker directly.
That is not a result. It is the sound of a measurement with no resolution.

The cause is the setup. The load generator, the router and the worker share one
laptop, so the worst one percent of requests is mostly the operating system
scheduling three processes against each other. That noise is larger than the
quantity being measured.

So the spec's requirement is **not currently verified**. It is not refuted
either. Verifying it needs the generator and the router on separate quiet
machines, which is R0.5 hardware work.

### Most of that 1.2ms is tokenizing

**Command:** `OUT=results/overhead-devtok MODEL_DIR=/nonexistent ARMS="direct prefix-affinity-balanced" ./scripts/overhead.sh`

The same experiment with the development tokenizer, which splits on whitespace
and hashes, in place of Qwen3's.

| added by the router | p50 from dispatch |
|---|---:|
| with the model's tokenizer | +1.23 +/-0.23 ms |
| with the development tokenizer | +0.42 +/-0.10 ms |

So roughly 0.8ms of the 1.2ms is the tokenizer, and the remaining 0.4ms covers
rendering the chat template, chaining the block hashes, querying the index and
reserving the blocks. Tokenizing a 280 word prompt is the expensive part of
being cache aware, and it is the part that cannot be given up: matching the
worker's block boundaries is the entire mechanism.

Two caveats on the subtraction. The two tokenizers produce different token
counts and therefore different numbers of blocks, so the index work is not held
quite constant. And the workers differ between the two arms in the same way, so
each delta is router-attributable within its own configuration while their
difference is an estimate rather than a measurement.

The obvious response is to stop tokenizing whole conversations repeatedly. A
multi-turn session re-sends its entire history every turn, and the prefix of
that history has been tokenized before. Per-session tokenizer caching is in the
spec and is not implemented, which makes this number a ceiling rather than a
fixed property.

### Both clocks agree

Latency from intended dispatch time is the honest measure of what a client
experiences and it is the headline everywhere else here. It also carries the
generator's own scheduling lag, which on a shared laptop is not small and is not
the router's fault. Measuring from actual dispatch drops that lag, at the cost of
hiding queueing that the router caused. Nothing queues in this experiment, so
both views should agree, and they do.

| added by the router | p50 from intended | p50 from dispatch |
|---|---:|---:|
| round-robin | +0.28 ms | +0.29 ms |
| prefix-affinity-balanced | +1.20 ms | +1.23 ms |

Agreement to a hundredth of a millisecond is the reason to believe the p50
numbers at all.

### A 40ms stall in the proxy, found and fixed by measuring against a real engine

The first attempt to measure router overhead against vLLM said the router added
**+40.17ms** at the median, with an interval of 0.63ms.

A cost that constant is not work. Work scales with the prompt; a number that
lands on the same tenth of a millisecond three runs running is a timer. Forty
milliseconds is the length of one specific timer: Linux's delayed
acknowledgement, which stalls exactly this long when it meets Nagle's algorithm
on the other side.

Nagle holds a small write back to see whether another one is coming. The peer
holds its acknowledgement back for the same reason. Neither moves until the timer
expires. A streaming proxy writes small things constantly, response headers and
then one event per token, and the first of those writes is time to first token.

Neither socket had `TCP_NODELAY` set. Not the connection from the client, not the
connection to the worker. Both do now, and the fix was measured on the same
hardware an hour later.

| | before | after |
|---|---:|---:|
| added at p50 | +40.17 +/-0.63 ms | **+1.84 +/-2.17 ms, unresolved** |
| added at p99 | +2.43 +/-0.92 ms | +2.18 +/-7.13 ms, unresolved |

The stall is gone. What the router adds is now smaller than the interval around
it, which is the same answer the mock gives, and the first time a real engine has
agreed.

Read the deltas, not the arms. Absolute latency moved between the two sessions,
because the direct arm's median went from 3.70 ms to 30.46 ms as the engine's
cache state changed underneath it. Only the difference between the two arms of a
single run is a controlled comparison, and that is what the table above reports.

This never showed against the mock worker, on a laptop, over loopback. It took a
real engine on a real network stack to make it visible, which is the argument for
the validation gate in one sentence.

The overhead numbers in the section above were all measured through this stall
and are therefore measurements of the stall. They stay as a record of what was
found.

## Closed-loop load generators under-report the tail

![Open loop against closed loop on the same worker](docs/charts/coordinated-omission.png)

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
- ~~The router's predicted hit rate against a worker's own reported hit rate,
  where the worker is vLLM rather than a model of vLLM.~~ Done on 2026-04-06.
  The gap is +8.9 points, and the section above reports it.
- The p99 overhead figure the spec asks for, which is under one millisecond.
  Three attempts have now failed to resolve it, twice on a laptop and once on a
  shared GPU, each time because the machine's own noise is wider than the
  quantity. The last attempt put it at +2.18 ms with an interval of 7.13 ms. It
  needs the generator, the router, and the worker on machines that are doing
  nothing else.

  The router now exports what it predicted, as
  `warmpath_predicted_hit_blocks_total` over `warmpath_predicted_blocks_total`,
  and `make validate-hit-rate` compares it against the workers' own
  `vllm:prefix_cache_hits_total` over `vllm:prefix_cache_queries_total`. Until
  now the router recorded no prediction at all, so the comparison this whole
  milestone rests on had nothing to compare.

  Against the mock fleet it reports an exact zero gap on identical block counts,
  87.34% either way. That is not reassurance and it is not a result. The mock
  computes its block hashes with the same `warmpath-core` code the router uses,
  so the two counters are one calculation performed twice, and an exact match
  shows only that the plumbing carries numbers. vLLM will be the first
  independent opinion this project has had.

  The router now exports what it predicted, as
  `warmpath_predicted_hit_blocks_total` over `warmpath_predicted_blocks_total`,
  and `make validate-hit-rate` compares it against the workers' own
  `vllm:prefix_cache_hits_total` over `vllm:prefix_cache_queries_total`. Until
  now the router recorded no prediction at all, so the comparison the whole
  milestone rests on had nothing to compare.

  Against the mock fleet it reports an exact zero gap on identical block counts,
  87.34% either way. That is not reassurance and it is not a result. The mock
  computes its block hashes with the same `warmpath-core` code the router uses,
  so the two counters are one calculation performed twice, and an exact match
  shows only that the plumbing carries numbers. vLLM will be the first
  independent opinion this project has had.
- The router's added latency at p99. Measured above and unresolved, because the
  noise floor of one laptop running all three processes is larger than the
  quantity. Needs separate quiet machines, which is R0.5.
- Where the 1.2ms of fingerprinting goes, at the level of a profile rather than
  a subtraction. R0.5 flamegraphs it.
- Whether session affinity adds anything on top of prefix affinity. Nothing
  here exercises it at all. The load generator builds one system message and one
  user question per request and never sends an `x-session-id` header, so every
  measured request is a first turn. The router's session map is implemented and
  unit tested and has never influenced a published number.

  The generator used to accept a `--session-turns` argument that did nothing,
  while recording the value in every run manifest. It now refuses any value
  other than one, because a manifest describing a workload the run did not
  perform is worse than a missing feature. Multi-turn and agentic replay are
  Appendix A3 in the spec, closed until R0.5 ships.

- Whether per-session tokenizer caching would recover the 1.2ms. It is specified
  and unimplemented, and it cannot help the workloads here even in principle,
  since caching a conversation's tokenized history needs a second turn and there
  is never a second turn. The measurement above is therefore the uncached cost on
  single-turn traffic, which is the worst case for the router and the best case
  for the honesty of the number.
