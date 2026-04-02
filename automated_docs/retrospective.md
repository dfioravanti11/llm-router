# Retrospective

> Running log of findings, wrong turns, and decisions, kept for an eventual
> write-up. Append as work happens; do not tidy the mistakes out. The bugs and
> the dead ends are the parts worth reading, and several of them are better
> material for a blog post than the results are.

Entries are newest last within each milestone.

---

## Environment and tooling

**Rust was not installed.** Fixed with `brew install rustup`, which is keg-only,
so `cargo` is not on the default `PATH`. Every shell invocation needs
`export PATH="/opt/homebrew/opt/rustup/bin:$PATH"`, including inside repo
scripts. `scripts/co-demo.sh` failed once for exactly this reason when invoked
without the export.

**Docker is not installed on this machine.** The `docker compose up` story in
the spec is therefore unverified. It is a real gap for the R0.5 exit criterion,
which is a stranger reproducing results from a clean checkout.

**Piping `cargo` through `head` wedges it.** Two `cargo test --workspace`
processes hung for twenty minutes producing no output. The cause is that Rust
ignores `SIGPIPE`, so when `head` exits and closes the pipe, cargo does not die
the way a C program would; it blocks. Cost about twenty minutes of confusion,
some of it spent suspecting a deadlock that turned out to be real but unrelated.
The rule now is to redirect cargo output to a file and grep the file.

**macOS ships bash 3.2.** `${array[-1]}` is a syntax error there. Caught
immediately by the first run of `scripts/policy-compare.sh`.

---

## R0.1 — Skeleton

**A live Hugging Face token was sitting in `.env.example`.** That file is meant
to be committed. It was replaced with a placeholder before the first code
commit, so it never entered this repo's history, but it should still be rotated
and has not been confirmed rotated.

**Cancellation came out of the design rather than being built.** Putting the
upstream response and a drop guard inside the response body's generator means a
client disconnect drops the generator, which closes the upstream connection and
records the cancellation. There is no separate cancellation path to keep in
sync, and therefore no way for one to drift from the other. The same shape was
reused for the block reservation at R0.3 on purpose.

**Making the mock deterministic turned a fuzzy test into an exact one.** Fixing
the completion id and setting `created` to zero means byte-identical passthrough
can be asserted with `assert_eq!` on the whole body, instead of decoding and
comparing selected fields. Cheap decision, large payoff.

**A test that could have passed for the wrong reason.** The cancellation test
originally streamed 1000 tokens at 10ms each, and polled for up to 10 seconds.
Those two numbers are the same, so a broken disconnect could have raced the
deadline. Raised to 5000 tokens, making natural completion impossible inside
the deadline, so reaching zero in-flight can only mean the disconnect worked.

---

## R0.2 — Measurement harness

**A deadlock that would have hung the router at startup.**
`prometheus_client::Family::get_or_create` returns a guard holding a read lock
on the label map, and takes a write lock when the label set is new. Resolving
six metric handles as temporaries inside one struct literal keeps all six guards
alive until the end of the statement, so the second lookup deadlocks against the
first. Found because the metrics unit tests hung; it would have hit production
startup, not just tests. Fixed by binding each lookup to its own `let`. Worth
remembering as a general Rust hazard: temporaries in a struct literal live to
the end of the statement, which is longer than it looks.

**The validity gate caught a broken script before it produced a wrong number.**
`scripts/co-demo.sh` wrote service logs into `results/`, which did not exist, so
the redirect failed and neither the router nor the worker started. Every request
errored. The harness marked all three runs invalid on a 100% error rate and
refused to produce a campaign. That is the machinery working exactly as
intended, and it is the single most reassuring thing that happened in this
milestone.

**`achieved_rate_per_second` was quietly wrong.** It divided successful measured
requests by the entire wall clock, warmup included, while the numerator excluded
warmup. The rate came out deflated by the warmup fraction, which made an
open-loop run at 38/s look slower than a closed-loop run at the same offered
load. Fixed to divide by the measurement window, and the window is now reported
alongside the rate so the arithmetic is checkable.

**Two of my own test assumptions were wrong, and one of them pointed at a real
design flaw.** The closed-loop test asserted that a request's intended time and
its dispatch time are identical, and they differed by a few microseconds because
the runner read the clock twice. The right fix was in the runner, not the test:
in closed loop there is genuinely nothing to be late against, so one clock read
now serves as both timestamps. The other assumption, that the coordinated
omission ratio stays near 1 on an unloaded run, was too strict at small absolute
latencies, where a fraction of a millisecond of jitter is a large ratio and a
trivial problem. Changed to bound the absolute difference instead.

**Threw out a much more impressive number because it was not a property of the
system.** The first coordinated-omission demo offered 150 arrivals per second
against a worker with capacity near 43, and reported a 775x gap between
closed-loop and open-loop p99. Persistent overload has no steady state: the
queue grows for as long as the run lasts, so a 30 second run reports about 55
seconds of p99 and a 60 second run would report roughly double. The published
demo now offers 38 against 43, which is high utilization with a queue that
settles, and reports a 7x gap. Less dramatic, and actually about the system
rather than the run length.

---

## R0.3 — Prefix-affinity routing

**The block index does not need to be a radix tree.** The spec calls for a radix
tree over block-hash sequences. Because each block hash is computed from its
parent, it already encodes every token before it, so two prompts share hash *i*
only if they agree on the entire prefix. A flat map from hash to a worker bitset
therefore answers prefix queries exactly as a tree would, one lookup per block,
with much less to get wrong. The tree's extra information is the parent-child
structure, which reuse-aware eviction would want; that is Appendix A3 and the
module can grow it then. Documented as a deliberate deviation rather than an
oversight.

**Plain LRU eviction is actively harmful for a prefix cache, and a failing test
found it.** A test asserting "a reused prefix survives eviction" failed. The
reason is structural: a chain's oldest block is its *first* block, so plain LRU
evicts the head and strands everything behind it. The worker keeps paying to
store those blocks and no request can ever match them again, so the modelled hit
rate collapses to zero while modelled memory stays full. Real engines evict
leaves first for exactly this reason. Both the router's index and the mock
worker's cache were changed to leaf-first least-recently-used, after which a
chain under pressure is eaten from the tail and prefix matching degrades one
block at a time. This was the single most valuable bug of the milestone, and it
came from writing the test before believing the implementation.

**A test failure that was correct behaviour.** Two conversations differing only
in the trailing partial block hashed identically. That is right: a worker cannot
serve a cache hit from a block it never finished filling, so hashing a partial
block would claim a shared prefix the worker cannot honour and inflate every
predicted hit rate by up to one block per request. The assertion was wrong, not
the code. Kept as its own named test so the property is asserted rather than
rediscovered.

**The first policy comparison was measuring nothing.** With three shared
prefixes of about 24 blocks each against a 96 block cache, the entire working
set fit on every worker. Round-robin scored 91.2% and affinity 94.4%, and the
difference was noise. Cache-aware routing only pays when the working set exceeds
one worker's cache and fits across the fleet. The workload was resized to ten
prefixes against a 64 block cache, and the original configuration was kept as
the project's first documented crossover: a regime where the approach buys
nothing.

**Two confounds in the comparison harness, both found by reading the output.**
The worker cache counters were cumulative across arms, so the second and third
policies appeared to have served three times as many blocks. Worse, workers were
not restarted between arms, so each policy inherited a cache the previous policy
had warmed. Both fixed: workers restart per arm, so counters cover one arm and
every arm starts cold, and the policy order rotates between repetitions so a
machine that warms up or throttles cannot masquerade as a routing result.

**Config sections needed field-level defaults.** A generated config with a
partial `[server]` table failed to parse because `max_request_bytes` was
missing. Found when `policy-compare.sh` could not start the router. Every
section now defaults field by field while still rejecting unknown keys, so a
config can override one value without restating the section, and a typo is still
an error rather than a silent default.

**The result is good and one column of it is not established.** Affinity raises
the workers' own reported hit rate from 35.6% to 89.1% and cuts p50 time to
first token from 38.3ms to 8.5ms at equal throughput. The p99 confidence
intervals overlap, so the tail improvement is not demonstrated at three runs.
`RESULTS.md` says so rather than quoting the point estimates and moving on.

**The balanced policy is currently indistinguishable from the plain one.**
Nothing in this workload creates a hotspot, so the balance override never fires
and the two policies make identical choices. That is expected, and building the
workload where it stops being true is what R0.4 is for.

### Outstanding at the end of R0.3

**The highest-risk item in the project is not closed.** The spec names
tokenization and hash mismatch with vLLM as the top risk precisely because it
fails silently: a mismatch produces mediocre hit rates that look like a weak
result rather than a bug. R0.3 currently uses a deterministic whitespace
tokenizer and a plain chat template of the project's own design. Everything
built on top is internally consistent and the routing logic is correct, but
nothing has been checked against a real model's tokenization or against vLLM's
block hash construction.

Part of that gap does not need a GPU and should not have been deferred: the real
Qwen3-1.7B tokenizer and its chat template can both be fetched and exercised on
a laptop, and vLLM's hash construction can be implemented from its source.
Only the final comparison against `vllm:prefix_cache_hits` needs hardware.
Closing the CPU-side part of this is the next piece of work, ahead of R0.4.

---

## Closing the tokenizer and template gap

Taken on immediately after R0.3 was committed, because the spec names it the
highest-risk item and the reason is that it fails silently.

**The risk is narrower than it first appears, and worth stating precisely.** The
router's block hashes are its own and never leave it, so they do not have to
equal anything vLLM computes. What has to be equal is the layer underneath: the
token sequence, and where the 16-token boundaries fall in it. If the router
tokenizes or renders differently from the worker, prefixes that the worker
considers shared are not shared here, and the only symptom is a mediocre hit
rate that reads as a weak result. So the fix is to run the model's own tokenizer
and the model's own chat template, which needs no GPU at all. Only the final
predicted-versus-actual hit rate comparison needs hardware, and that is R0.5.

**Hugging Face chat templates are Python Jinja, and a Rust Jinja engine is not
enough.** Qwen3-1.7B's template calls `startswith`, `endswith`, `split`,
`strip`, `lstrip`, and `rstrip` on strings. minijinja has none of them, so the
real template failed to render with `unknown method: string has no method named
startswith`. Fixed with `minijinja-contrib`'s `pycompat` unknown-method
callback. Worth noting that this failed *loudly*, which was luck as much as
design: a renderer that fell back to a simpler template on error would have
produced exactly the silent mediocre-hit-rate failure the whole exercise is
about. The router now refuses to start when a configured model cannot be
loaded, for the same reason.

**The tokenizer changes the answer.** Re-running the identical policy comparison
with the model's tokenizer instead of the development one moved the reported hit
rate from 89.1% to 80.9%, because the two cut the prompt into blocks in
different places. Neither run is wrong about itself, and from inside either one
the discrepancy is invisible. That is the risk in miniature, and it is now a
paragraph in `RESULTS.md`.

**A test that contradicted itself.** The unloaded coordinated-omission test
allowed 50ms of dispatch lag as valid and then asserted the gap between the two
clocks was under 10ms. The gap *is* the lag, so on a machine busy running the
rest of the suite the test failed itself. Replaced with the actual invariant:
per request, `intended_latency = dispatch_latency + lag`, so every percentile of
the intended-time distribution exceeds the same percentile of the dispatch-time
one by at most the largest observed lag. That is guaranteed by construction, and
it does not degrade into measuring the test runner.

**The flat p99 had a mechanism, and finding it turned a shrug into a result.**
With the real tokenizer the comparison showed the median improving 3.4x and the
hit rate going from 31.3% to 80.9% while the p99 did not move at all. Rather
than reporting that as unresolved, the arithmetic was worth doing: at about 21
blocks per request the ten-prefix pool is a working set near 200 blocks, and
three workers at 64 blocks each hold 192. Even a perfect partition does not fit,
so eviction churn never stops and roughly one request in a hundred misses
entirely under every policy. The worst one percent is made of full misses, which
is the p99.

Re-running with 112 blocks per worker, so the fleet holds 336 against the same
200, separated the tail cleanly: p99 47.5ms for round-robin against 19.5ms for
affinity, with intervals that do not overlap. So the honest finding is a
condition rather than a number: cache-aware routing needs the fleet to have
enough aggregate cache to hold the working set, and below that line it improves
the median and stops improving the tail. That is a better result than the one
originally hoped for, and it came from taking a disappointing number seriously
instead of reporting it as noise.

---

## R0.4 — Load-aware and session-aware

**A hotspot needs somewhere to hurt, and the first attempt had nowhere.** The
skew test concentrated 83% of requests on one worker, exactly as intended, and
the balanced policy did nothing about it, because the hot worker never queued.
The reason is a nice inversion: the worker holding the hot prefix serves it
*from cache*, so its requests are the fastest in the fleet and it absorbs the
load easily. Concentration only costs something once each request occupies a
worker for a while regardless of caching, which means decode time. Raising the
generated response to 32 tokens at 2ms each, and giving each worker four serving
slots rather than thirty-two, made the hotspot real. Worth remembering as a
modelling lesson: prefix caching removes prefill cost, not decode cost, so a
mock where decode is free cannot show a cache-aware policy overloading anything.

**Two silent patch failures, same cause.** Two edits to `policy.rs` did not
apply and the compiler caught neither, because the surrounding code still made
sense without them. Both had been reformatted by `cargo fmt` since the text was
written, so the search string no longer matched. The fix is to assert the
replacement happened rather than trusting it; the one that slipped through
silently would have let the router send traffic to workers it had already
ejected.

**A test that asserted more than the arithmetic supported.** The first version
of the KV-headroom test gave both workers a queue of two, and expected the
worker at 98% KV to lose to one at 5% despite holding less of the prefix. It
did not, and the scoring was right: with queue depth equal but non-zero, the
match advantage narrowly outweighed the memory difference. Setting both queues
to empty isolates the thing the test claims to be about, and it passes at the
default weighting. The lesson is that a test of "signal X is used" should vary
only X.

**Naive affinity fails in the most instructive way possible.** On skewed traffic
it produces the *highest* cache hit rate in the field and by far the worst
latency, with throughput collapsing to well under half the offered rate. That is
the clearest possible statement that hit rate is not the objective. A router
optimising the metric that looks like success drives the system into the ground,
and the metric keeps going up while it happens.

**Skew makes the caching problem easy and the balancing problem hard.** When
80% of requests share one prefix, that prefix fits comfortably in every worker's
cache, so even round-robin achieves a high hit rate without trying. Cache-aware
routing has almost nothing left to win and a great deal to lose. Together with
the earlier crossover, the picture is that the technique pays in a fairly narrow
band, and the honest version of the project's claim has to say so.

**The best policy on the skewed workload is round-robin.** Not the balanced
policy, and not by a little: 44.2ms p99 against balanced affinity's 120.5ms.
This is the result the project would most like not to have found, which is
exactly why it is in `RESULTS.md` next to the wins. The explanation is the one
above, that skew makes caching easy, but the discipline is separate from the
explanation. A comparison harness that only ever confirms its author's policy
was not a comparison harness.

**A stale load signal is worse than no load signal.** On the skewed workload
`least-loaded` posted a p99 nearly three times round-robin's, with an identical
hit rate and an identical spread of requests across workers, so cache behaviour
explains none of it. Queue depth is polled every 100ms; for most of that window
every routing decision reads the same snapshot and piles onto the same worker
until the next poll corrects it. `power-of-two` landed between the two, which is
the behaviour power-of-two-choices is known for and was not something the
experiment was designed to show. Round-robin cannot herd because it does not
look.

Two consequences. Part of the balanced policy's tail on that workload is the
same staleness rather than the affinity it retains, so the two costs are not
currently separable. And the poll interval is now a routing parameter, not a
monitoring parameter, which is not how it was originally treated.

**An invalid run is a result about the harness, not a run to retry.** Getting
the skewed matrix to produce three valid repetitions took three attempts. At
90/s the generator fell 292ms behind schedule. At 60/s with 32 tokens at 2ms it
still fell 158ms behind, and the cause was not load but timer wakeups: the
router, three workers, and the generator share one laptop, and a fine token
schedule swamped it. Eight tokens at 8ms is the same 64ms of worker occupancy
with a quarter of the wakeups, and produced zero invalid runs. The harness
refused to publish all three times without being asked to, which is the only
reason the first two numbers are not in `RESULTS.md` today.

**Measuring your own overhead is harder than measuring a policy.** A routing
policy changes latency by tens of milliseconds, which a laptop can resolve. The
router's own cost is around a millisecond, which it cannot. The first attempt
left the mock worker's simulated prefill switched on, putting about 10ms of
modelled work underneath a difference near 1ms, and it reported round-robin as
4.33ms faster than not using a router at all. A negative overhead is
arithmetically possible and physically nonsense, and it was the useful signal:
the experiment had no resolution.

Three changes fixed the median and not the tail. Make the worker as close to
free as it goes, since the router's cost does not depend on the worker's speed
and every millisecond the worker spends is variance added to a measurement of
something else. Raise the repetitions. Report the delta against its own
interval, and label it unresolved when it is smaller.

The tail is still unresolved and probably cannot be resolved here at all. The
worst one percent of requests on a laptop running the generator, the router and
the worker is mostly the operating system choosing between three processes.
That is not a number to publish, and the requirement is recorded as unverified
rather than met.

**Being a proxy is cheap and being cache aware is not.** Under 0.3ms to accept a
request and stream it back, against 1.2ms to render the chat template, tokenize,
chain the hashes and query the index. Re-running with the whitespace development
tokenizer put the same figure at 0.42ms, which locates roughly two thirds of the
cost in tokenization alone. That is the one part of the work that cannot be
dropped, since matching the worker's block boundaries is the whole mechanism.
It can be avoided rather than made cheaper: a conversation re-sends its history
every turn and the router tokenizes all of it again, so per-session caching of
the tokenized prefix is in the spec and is not implemented. The number is a
ceiling, not a property.

**The reflog timed out.** Committing R0.4 failed with an I/O timeout writing
`.git/logs/refs/heads/main`, and `git add -A` took two minutes and twenty
seconds at zero percent CPU. The repository is under `~/Desktop`, which is
synced by iCloud, so every git write goes through the file provider. A retry
landed in nine seconds. Worth knowing before diagnosing a phantom git problem,
and worth moving the checkout out of a synced directory.

**Writing the design document found two defects that testing had not.** Both
were invisible to the test suite because both were about things the code never
did rather than things it did wrong.

The metrics poller shares the proxy's HTTP client. That is reasonable until you
notice the proxy's read timeout is deliberately long, because a slow generation
is a healthy generation, and the poller inherited it. One worker that accepted a
connection and then went quiet would hold the sequential poll loop for a minute
while every other worker's load reading went stale. No test caught it because no
test had ever stalled a worker rather than closing the connection on it. The
regression test now does exactly that, and it fails without the fix.

The load generator's `--session-turns` argument did nothing. It was parsed,
threaded into `RunConfig`, and written into every run manifest, while the
workload code never read it and the request path never sent a session header.
Every published run therefore carries a field describing a workload property
that did not exist. This is the same class of failure the project names as its
worst, a wrong answer that looks like a right one, and it had been sitting in the
manifests since R0.4. It now refuses any value above one.

The second one has a consequence beyond the flag. Session affinity has never
been exercised by any benchmark in this repo, so calling it unvalidated was too
generous. `RESULTS.md` now says the mechanism has never influenced a published
number.

**An agent writing prose was a better reviewer than a reviewer.** Both defects
came out of an attempt to explain the design to a stranger, which forces a
question testing does not: what happens when N is not three. Two of my own
framings turned out to be wrong under that pressure. I had described the metrics
polling as a thundering herd, and it is the opposite shape, a single sequential
task whose period grows with fleet size. I had also quoted the poll interval as
100ms, which is what the benchmark scripts set, while the shipped default is
500ms and therefore worse.

**The disk filled up mid-run and everything started failing strangely.** Git
reported `mmap failed: Operation canceled`, an agent hit `ENOSPC`, and a
background task died writing its own output. The volume was at 100% with 139MB
free. `target/debug` alone was 7.6GB, which is regenerable, and removing it
recovered 12GB. Worth recognising early: when several unrelated tools start
failing in unrelated ways at the same moment, check the disk before debugging
any of them.

## The GPU session, 2026-04-06

One L4, not the two that were asked for. The quota request for four came back
approved for one. Two vLLM servers were run on the single device instead, each
held to 112 blocks with `--num-gpu-blocks-override`, which reproduces the
scarcity the mock runs are built around. Cache behaviour survives that
arrangement. Latency does not, because the two engines take turns on one device
and the contention is larger than anything routing could save.

**The mock was right about the cache and wrong about the tail.** Hit rate came in
at 52.5% and 84.3% against the mock's 52.1% and 88.4%, which is close enough to
call a reproduction. Tail latency did not move at all, against a predicted 2.7x
improvement. Nine months of tuning a policy against a simulator, and the
simulator turned out to model the thing the policy controls and not the thing the
policy is sold on. That asymmetry is the most useful thing the session produced,
and it is only visible because both numbers were published rather than the
flattering one.

**The credibility gate passed.** A worker cannot report 52% on scattered traffic
and 84% on gathered traffic unless the router cuts blocks where the engine cuts
them. The prompt rendering, the tokenizer, and the block hash chain agree with
vLLM. That was the largest open risk and it is closed.

**A 40ms stall had been in the proxy the whole time.** The first overhead
measurement said the router added 40.17ms at the median, interval 0.63ms. The
constancy is the tell. Work varies with the prompt; a figure that lands on the
same tenth of a millisecond three runs running is a timer, and 40ms is the length
of Linux's delayed acknowledgement when it meets Nagle's algorithm. Neither
socket had `TCP_NODELAY` set.

Every latency number this project has ever published was measured on a laptop
over loopback, where the stall never fires. The mock could not have found it. No
test could have found it. It took a real engine on a real network stack, which is
the argument for the validation gate stated as a single defect.

**Units are a place to be paranoid.** The hit rate comparison read 85% against
76% over totals of 5,634 and 92,959, which looks like the two sides measured
different traffic. They did not. vLLM counts prefix cache queries in tokens and
the router counts blocks, and 5,634 blocks is 90,144 tokens, which is within 3%
of 92,959. The rates were always comparable because a fraction has no unit. The
totals never were. An hour went into that.

**What one GPU could not buy.** No tail latency comparison, no hotspot cost, and
no verified sub-millisecond overhead. All three need two devices so that one
worker can saturate while another idles. The p99 overhead figure has now been
unverified across three separate attempts, on a laptop and on a shared GPU, and
the reason has been the same every time: the noise floor of the machine is wider
than the quantity.

---

## Scope change, spec v3.0

The spec was cut from ten releases to five while R0.3 was being committed.
R0.1 through R0.5 are now the commitment and everything else became Appendix A.
Concretely:

| Was | Now |
|---|---|
| R0.6 reliability | Appendix A1, except basic health checking and single retry, which moved into R0.4 |
| R0.7 event-driven index over ZMQ | Appendix A2 |
| R0.8 agentic workloads | Appendix A3 |
| R0.9 self-scrutiny | Folded into R0.5 |
| R1.0 public release | R0.5 is the ship point |

No code became wrong, but a lot of comments referenced releases that no longer
exist and had to be renumbered. The `BlockIndex` trait was introduced so a
second backend could slot in at what was R0.7; that backend is now optional, and
the trait is kept on the narrower grounds that it costs almost nothing and marks
the seam. If Appendix A2 never happens the trait is mild over-engineering, which
is an acceptable price and worth naming rather than pretending otherwise.

Sample size also came into focus: the spec asks for at least 5,000 measured
requests per configuration, and the R0.3 comparison used about 1,200 per run.
The comparison script's defaults were raised accordingly.
