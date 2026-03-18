#!/usr/bin/env python3
"""Turn a real fleet's metrics into the per-arm stats file the charts expect.

The mock worker serves a `/debug/stats` endpoint that reports exactly what this
project wants to know, because it was written for this project. A real inference
server reports Prometheus text and knows nothing about arms or repetitions, so
the same numbers have to be reconstructed:

  started                 how many requests each worker was given, which the
                          router knows, and its counters begin at zero for every
                          arm because it is restarted for each one

  prefix_cache_queries    the worker's own cache counters, which run for the
  prefix_cache_hits       life of the server process, so an arm's share of them
                          is the difference measured across it

Writes one json object per worker to stdout, in the shape
`scripts/plot.py` already reads.

Usage:
  worker-stats.py <before> <after> <router-metrics>

The before and after files hold each worker's raw `/metrics` output, separated
by a marker line, in worker order.
"""

import json
import sys

SEPARATOR = "### END OF WORKER ###"


def split_workers(path):
    """One block of Prometheus text per worker, in the order they were polled."""
    with open(path) as handle:
        body = handle.read()
    blocks = body.split(SEPARATOR)
    # The split leaves a trailing empty piece after the final marker.
    return [block for block in blocks[:-1]]


def sum_metric(block, name):
    """Every sample of one metric in a block, added up.

    Summed rather than taken singly because a server running several engines
    reports one series per engine. A metric that is absent returns None, which
    the caller reports rather than quietly treating as a zero.
    """
    total = None
    for line in block.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        head, _, value = line.rpartition(" ")
        if head.split("{", 1)[0].strip() != name:
            continue
        try:
            parsed = float(value)
        except ValueError:
            continue
        total = parsed if total is None else total + parsed
    return total


def router_decisions(path):
    """Requests per worker, keyed by the worker name the router config gave it."""
    counts = {}
    try:
        with open(path) as handle:
            body = handle.read()
    except OSError:
        return counts

    for line in body.splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        head, _, value = line.rpartition(" ")
        name = head.split("{", 1)[0].strip()
        if name != "warmpath_routing_decisions_total":
            continue
        labels = head[head.find("{") + 1:head.rfind("}")] if "{" in head else ""
        worker = None
        for pair in labels.split(","):
            key, _, raw = pair.partition("=")
            if key.strip() == "worker":
                worker = raw.strip().strip('"')
        if worker is None:
            continue
        try:
            counts[worker] = counts.get(worker, 0) + int(float(value))
        except ValueError:
            continue
    return counts


def main():
    if len(sys.argv) != 4:
        print(__doc__, file=sys.stderr)
        return 2

    before_blocks = split_workers(sys.argv[1])
    after_blocks = split_workers(sys.argv[2])
    decisions = router_decisions(sys.argv[3])

    if len(before_blocks) != len(after_blocks):
        print(
            "before and after hold different numbers of workers, so no arm can "
            "be measured across them",
            file=sys.stderr,
        )
        return 1

    for index, (before, after) in enumerate(zip(before_blocks, after_blocks)):
        queries_before = sum_metric(before, "vllm:prefix_cache_queries_total")
        queries_after = sum_metric(after, "vllm:prefix_cache_queries_total")
        hits_before = sum_metric(before, "vllm:prefix_cache_hits_total")
        hits_after = sum_metric(after, "vllm:prefix_cache_hits_total")

        if queries_after is None or hits_after is None:
            print(
                "worker %d reported no prefix cache counters. vLLM needs prefix "
                "caching switched on, and without it there is nothing here to "
                "compare the router against." % index,
                file=sys.stderr,
            )
            return 1

        # A server restarted mid-run resets its counters, which shows up as a
        # negative difference. Saying so is better than publishing it.
        queries = queries_after - (queries_before or 0.0)
        hits = hits_after - (hits_before or 0.0)
        if queries < 0 or hits < 0:
            print(
                "worker %d went backwards between the two readings, so it was "
                "restarted during the arm and this run is not usable" % index,
                file=sys.stderr,
            )
            return 1

        print(json.dumps({
            "started": decisions.get("w%d" % index, 0),
            "cache": {
                "prefix_cache_queries": int(queries),
                "prefix_cache_hits": int(hits),
            },
        }))

    return 0


if __name__ == "__main__":
    sys.exit(main())
