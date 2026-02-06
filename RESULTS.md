# Results

Every number here regenerates from a command in this repo. Numbers measured
against the mock worker say so, because a mock is not evidence about real
inference; it is evidence about the harness and the router.

Real vLLM enters at R0.5. Nothing below has touched a GPU.

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

## Round-robin baseline

**Command:** run a router and a worker, then `make bench`

Not yet captured as a published figure. A baseline is only meaningful next to
the thing it is a baseline for, and prefix affinity does not exist until R0.3.
The harness, the run manifest, and the three-run confidence interval are in
place, so R0.3 produces a comparison rather than a first attempt at one.

## What is not measured yet

- Anything about cache hit rate. There is no block index.
- Anything on real hardware. R0.5.
- The router's own added latency. R0.9 measures it with flamegraphs and
  publishes the number whether or not it is flattering.
