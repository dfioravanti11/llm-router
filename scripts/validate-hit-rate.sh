#!/usr/bin/env bash
#
# Does the router know what it is talking about?
#
# The router decides where to send a request by predicting how much of the
# prompt a worker already holds. Everything published about cache behaviour
# rests on that prediction being roughly true. Every other cache number in this
# project comes from the same idea that produced the prediction, so agreement
# between them is not evidence of anything.
#
# This compares two counters that were produced independently:
#
#   the router     warmpath_predicted_hit_blocks_total / warmpath_predicted_blocks_total
#   the worker     vllm:prefix_cache_hits_total / vllm:prefix_cache_queries_total
#
# The two count in different units. The router counts blocks. vLLM counts tokens,
# which its own help text says and which is easy to miss. Both are read here as
# a fraction, and a fraction does not care about the unit, so the rates compare
# directly. The totals do not, so the router's are multiplied by the block size
# before the two are checked against each other.
#
# The second pair the router does not control and cannot talk itself into. A
# large gap is a real finding and gets published either way, which is the whole
# point of the exercise.
#
# Works against the mock worker, where it mostly tests this script. It means
# something against vLLM, which is what R0.5 is for.
#
# Usage, against whatever is already running:
#
#   ROUTER=http://localhost:8080 \
#   WORKERS="http://gpu-0:8000 http://gpu-1:8000" \
#   ./scripts/validate-hit-rate.sh
#
# It sends no traffic of its own. Run a workload through the router first, or
# point it at a fleet that has been serving, then read the answer.

set -euo pipefail

cd "$(dirname "$0")/.."

ROUTER=${ROUTER:-http://127.0.0.1:8080}
WORKERS=${WORKERS:-}
OUT=${OUT:-results/hit-rate}
# Must match `[index] block_size` in the router config and `--block-size` on the
# workers. Used only to compare the two totals, never the two rates.
BLOCK_SIZE=${BLOCK_SIZE:-16}

if [ -z "$WORKERS" ]; then
  echo "WORKERS is empty. Set it to the worker base URLs, space separated." >&2
  echo "  WORKERS=\"http://gpu-0:8000 http://gpu-1:8000\" $0" >&2
  exit 2
fi

mkdir -p "$OUT"

router_metrics="${OUT}/router-metrics.txt"
if ! curl -sf "${ROUTER}/metrics" -o "$router_metrics"; then
  echo "could not read ${ROUTER}/metrics" >&2
  exit 1
fi

# One file per worker, so a failure names the worker it belongs to.
index=0
worker_files=()
for worker in $WORKERS; do
  file="${OUT}/worker-${index}-metrics.txt"
  if ! curl -sf "${worker}/metrics" -o "$file"; then
    echo "could not read ${worker}/metrics" >&2
    exit 1
  fi
  worker_files+=("$file")
  index=$((index + 1))
done

python3 - "$BLOCK_SIZE" "$router_metrics" "${worker_files[@]}" <<'PY'
import sys


def totals(path, names):
    """Sum every sample of each metric, across whatever labels it carries.

    The router reports one series per worker and vLLM reports one per engine, so
    the fleet total is the sum. Anything absent stays at zero and is reported as
    absent rather than as a zero rate, since those mean very different things.
    """
    found = {name: None for name in names}
    with open(path) as handle:
        for line in handle:
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            head, _, value = line.rpartition(" ")
            metric = head.split("{", 1)[0].strip()
            for name in names:
                if metric == name:
                    try:
                        parsed = float(value)
                    except ValueError:
                        continue
                    found[name] = (found[name] or 0.0) + parsed
    return found


block_size = int(sys.argv[1])
router_path, worker_paths = sys.argv[2], sys.argv[3:]

# The registry prefixes every name with `warmpath` and counters get a `_total`
# suffix in the exposition.
router = totals(
    router_path,
    ["warmpath_predicted_blocks_total", "warmpath_predicted_hit_blocks_total"],
)
predicted_blocks = router["warmpath_predicted_blocks_total"]
predicted_hits = router["warmpath_predicted_hit_blocks_total"]

if not predicted_blocks:
    print("The router reported no predicted blocks at all.")
    print()
    print("Either no traffic has been through it, or the policy in use does not")
    print("build a prompt fingerprint. Only the prefix-affinity policies do, so")
    print("a round-robin run has nothing to validate.")
    sys.exit(2)

worker_queries = 0.0
worker_hits = 0.0
missing = []
for path in worker_paths:
    found = totals(
        path,
        ["vllm:prefix_cache_queries_total", "vllm:prefix_cache_hits_total"],
    )
    if found["vllm:prefix_cache_queries_total"] is None:
        missing.append(path)
        continue
    worker_queries += found["vllm:prefix_cache_queries_total"]
    worker_hits += found["vllm:prefix_cache_hits_total"] or 0.0

if missing:
    print("These workers did not report prefix cache counters:")
    for path in missing:
        print(f"  {path}")
    print()
    print("vLLM needs prefix caching switched on for them to exist. Without them")
    print("there is nothing to check the router against.")
    sys.exit(2)

if not worker_queries:
    print("The workers reported zero prefix cache queries, so nothing to compare.")
    sys.exit(2)

predicted_rate = predicted_hits / predicted_blocks
actual_rate = worker_hits / worker_queries
gap = predicted_rate - actual_rate

print("prefix cache hit rate")
print(f"  router predicted   {predicted_rate:7.2%}   "
      f"over {predicted_blocks:,.0f} blocks routed")
print(f"  workers reported   {actual_rate:7.2%}   "
      f"over {worker_queries:,.0f} tokens looked up")
print(f"  gap                {gap:+7.2%}")
print()

# Same traffic, different units. If these two totals disagree by much, the two
# sides did not see the same requests, and comparing their rates means nothing.
# A few percent is normal: the router counts a part-used block as a whole one.
router_tokens = predicted_blocks * block_size
drift = abs(worker_queries - router_tokens) / router_tokens
print(f"  the router routed {router_tokens:,.0f} tokens by its own count, and the")
print(f"  workers looked up {worker_queries:,.0f}, a difference of {drift:.1%}.")
if drift > 0.10:
    print()
    print("  WARNING: those totals are too far apart to be the same traffic.")
    print("  Check that the block size matches on both sides, and that no other")
    print("  client is sending requests to these workers.")
print()

# The two counters do not count identical things, and pretending otherwise
# would make a small gap look like a finding. The router counts blocks in the
# prompts it routed. vLLM counts block lookups its scheduler performed, which
# includes work the router never saw and excludes anything the router sent that
# the engine chose not to look up.
print("These counters are close cousins rather than the same quantity. The")
print("router counts blocks in the prompts it routed. The engine counts the")
print("lookups its own scheduler did. Read the gap as a direction and a rough")
print("size, and treat a few points either way as agreement.")
print()

if abs(gap) <= 0.05:
    print("VERDICT: the router's model of the fleet's cache matches what the")
    print("fleet reports, within five points.")
elif gap > 0:
    print("VERDICT: the router is optimistic. It believed the workers held more")
    print("than they did, so requests were routed for hits that did not happen.")
    print("Look first at block boundaries, since a tokenizer or chat template")
    print("that disagrees with the engine produces exactly this.")
else:
    print("VERDICT: the router is pessimistic. The workers hit more often than")
    print("the router predicted, which usually means the engine is caching")
    print("something the router does not model, and it costs routing quality")
    print("rather than correctness.")
PY

echo
echo "raw counters saved under ${OUT}"
