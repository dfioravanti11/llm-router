# Results

Every number here regenerates from a command in this repo. Numbers measured
against the mock worker say so, because a mock is not evidence about real
inference; it is evidence about the harness and the router.

Real vLLM enters at R0.5. Nothing below has touched a GPU.

## Prefix-affinity routing against round-robin

**Command:** `make policy-compare`

Three mock workers, each caching 64 blocks. The workload is a pool of ten
shared prefixes of about 16 blocks each, so the working set is roughly 160
blocks: too much for any one worker to hold, and comfortable across the fleet.
Each request carries one prefix plus its own short question. Sixty arrivals per
second, open loop.

Three repetitions per policy. Workers are restarted for every arm, so no policy
inherits a cache another policy warmed, and the arm order rotates between
repetitions.

| policy | throughput | p50 TTFT | p99 TTFT | 95% CI on p99 | worker-reported hit rate |
|---|---:|---:|---:|---:|---:|
| round-robin | 58.6/s | 38.3 ms | 43.4 ms | [39.9, 47.8] ms | 35.6% |
| prefix-affinity | 58.6/s | 8.5 ms | 33.1 ms | [27.7, 40.5] ms | 89.1% |
| prefix-affinity-balanced | 58.6/s | 8.5 ms | 36.8 ms | [29.6, 42.7] ms | 89.2% |

Two of those columns are worth believing and one is not yet.

**The median improves by about 4.5x, and the hit rate explains why.** Round-robin
shows every prefix to every worker, so each worker is asked to hold 160 blocks
in 64 and spends the run evicting what it is about to need. Affinity partitions
the prefixes across the fleet; each worker then holds a share that fits. The
workers' own counters, which the router does not control, go from 35.6% to
89.1% of blocks served from cache.

**The p99 improvement is not established.** The intervals overlap:
[39.9, 47.8] against [27.7, 40.5]. Three runs is the floor this project set for
reporting anything, and at the tail it is not enough to separate these two. The
honest statement is that affinity clearly improves the median and the hit rate,
and that its effect on the tail is unresolved at this sample size. More
repetitions would settle it, and R0.4 will need them.

**The balanced policy is indistinguishable from the plain one here, as
expected.** Nothing in this workload creates a hotspot: ten prefixes over three
workers spread evenly on their own, so the balance override never fires and the
two policies make the same choices. R0.4 exists to build the workload where
that stops being true.

### The crossover: when affinity buys nothing

Covered by a test, in `crates/warmpath-bench/tests/affinity.rs`.

Shrink the pool from ten prefixes to three and the whole working set fits in
every worker's cache. Round-robin then hits on about 91% of blocks without any
help, and affinity measures the same. There is nothing to arrange, and the only
thing affinity adds is a constraint on where requests may go.

This was found by accident: the first version of the comparison used three
prefixes, and round-robin scored 91.2% against affinity's 94.4%. The workload
was not oversubscribed, so it was not measuring routing at all. Cache-aware
routing pays when the working set exceeds one worker's cache and fits across the
fleet. Outside that band it is overhead.

### What is not being claimed

The mock worker's cache and the router's index are separate implementations on
purpose, but both are models of the same idea, and they agree partly because
they were written by someone with the same idea in mind. Agreement between them
is not evidence. The predicted-versus-actual hit rate check only becomes real at
R0.5, where the worker's cache is vLLM's and not a model at all.

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
