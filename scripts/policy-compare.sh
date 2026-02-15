#!/usr/bin/env bash
#
# Compare routing policies on one workload.
#
# The workload is a pool of long shared prefixes, each request adding its own
# short question. The workers' caches are sized so the whole pool does not fit
# on any single worker but does fit across the fleet, which is the regime where
# where a request goes decides whether it hits.
#
# Three things keep this a comparison rather than a story:
#
#   Workers are restarted for every arm, so no policy inherits a cache another
#   policy warmed. The warmup window then absorbs the cold start.
#
#   Repetitions are interleaved and the policy order rotates. Running one
#   policy's repetitions back to back would let a laptop that warms up, or one
#   that thermally throttles, masquerade as a routing result.
#
#   Every policy sees the same seeds, so the arms differ only in the router's
#   configuration.

set -euo pipefail

cd "$(dirname "$0")/.."

OUT=${OUT:-results/policy-compare}
RUNS=${RUNS:-3}
# 60/s over a 30s measurement window across 3 runs is about 5,400 measured
# requests per policy, which clears the spec's floor of 5,000.
DURATION=${DURATION:-35}
WARMUP=${WARMUP:-5}
RATE=${RATE:-60}
WORKERS=${WORKERS:-3}

# A 256 word prefix is about 20 blocks under the model's tokenizer, so ten
# prefixes are a working set near 200 blocks.
#
# The default gives each worker 112 blocks, so the fleet holds 336: no single
# worker can hold the working set, but the fleet has room to spare once the
# prefixes are partitioned. That is the regime cache-aware routing is for.
#
# Re-run with CACHE_BLOCKS=64 for the under-provisioned case, where the fleet
# total sits just below the working set. Affinity still improves the median
# there and stops improving the tail, which is a boundary worth knowing.
PREFIX_WORDS=${PREFIX_WORDS:-256}
PREFIX_POOL=${PREFIX_POOL:-10}
CACHE_BLOCKS=${CACHE_BLOCKS:-112}
BLOCK_SIZE=16

POLICIES=${POLICIES:-"round-robin prefix-affinity prefix-affinity-balanced"}

# The model's own tokenizer and chat template, when they have been fetched.
# Both the router and the workers must use the same one, or they are cutting
# blocks at different boundaries and describing different experiments.
MODEL_DIR=${MODEL_DIR:-.cache/qwen3-1.7b}
if [ -f "${MODEL_DIR}/tokenizer.json" ]; then
  MOCK_MODEL_ARGS=(--model-dir "$MODEL_DIR")
  ROUTER_MODEL_TOML=$'\n[model]\ndirectory = "'"${MODEL_DIR}"$'"\n'
  echo "using the model tokenizer at ${MODEL_DIR}"
else
  MOCK_MODEL_ARGS=()
  ROUTER_MODEL_TOML=""
  echo "no model at ${MODEL_DIR}; using the development tokenizer. Run scripts/fetch-model.sh." >&2
fi

MOCK_BASE_PORT=${MOCK_BASE_PORT:-19001}
ROUTER_PORT=${ROUTER_PORT:-19080}

cargo build --release --workspace
mkdir -p "$OUT"

MOCK_PIDS=()
ROUTER_PID=""

stop_all() {
  if [ -n "$ROUTER_PID" ]; then
    kill "$ROUTER_PID" 2>/dev/null || true
    ROUTER_PID=""
  fi
  for pid in ${MOCK_PIDS[@]+"${MOCK_PIDS[@]}"}; do
    kill "$pid" 2>/dev/null || true
  done
  MOCK_PIDS=()
}
trap stop_all EXIT

worker_toml=""
for index in $(seq 0 $((WORKERS - 1))); do
  port=$((MOCK_BASE_PORT + index))
  worker_toml+=$'\n[[workers]]\nname = "w'"${index}"$'"\nurl = "http://127.0.0.1:'"${port}"$'"\n'
done

start_workers() {
  for index in $(seq 0 $((WORKERS - 1))); do
    ./target/release/warmpath-mock \
      --bind "127.0.0.1:$((MOCK_BASE_PORT + index))" \
      --ttft-ms 1 --inter-token-ms 1 --max-concurrency 32 \
      --cache-blocks "$CACHE_BLOCKS" --block-size "$BLOCK_SIZE" \
      --prefill-per-token-us 120 \
      ${MOCK_MODEL_ARGS[@]+"${MOCK_MODEL_ARGS[@]}"} \
      > "${OUT}/worker${index}.log" 2>&1 &
    # Captured into a variable first: macOS ships bash 3.2, which has no
    # negative array subscripts.
    worker_pid=$!
    MOCK_PIDS+=("$worker_pid")
    disown "$worker_pid" 2>/dev/null || true
  done
}

start_router() {
  local policy=$1
  local config
  config=$(mktemp)
  cat > "$config" <<TOML
[server]
bind = "127.0.0.1:${ROUTER_PORT}"

[routing]
policy = "${policy}"

[index]
block_size = ${BLOCK_SIZE}
block_budget = ${CACHE_BLOCKS}
${ROUTER_MODEL_TOML}${worker_toml}
TOML

  ./target/release/warmpath --config "$config" >> "${OUT}/router.log" 2>&1 &
  ROUTER_PID=$!
  disown "$ROUTER_PID" 2>/dev/null || true

  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:${ROUTER_PORT}/health" > /dev/null \
      && curl -sf "http://127.0.0.1:${MOCK_BASE_PORT}/health" > /dev/null; then
      rm -f "$config"
      return 0
    fi
    sleep 0.2
  done
  echo "router or workers did not come up for ${policy}; see ${OUT}/router.log" >&2
  exit 1
}

policy_list() {
  # Rotate the order by the repetition number, so no policy always runs first.
  local offset=$1
  local ordered=($POLICIES)
  local count=${#ordered[@]}
  local i
  for ((i = 0; i < count; i++)); do
    printf '%s ' "${ordered[$(((i + offset) % count))]}"
  done
}

for repetition in $(seq 1 "$RUNS"); do
  for policy in $(policy_list "$repetition"); do
    echo
    echo "=== ${policy}, repetition ${repetition} of ${RUNS} ==="

    # Fresh workers per arm: every policy starts from an empty cache, and the
    # warmup window absorbs the cold start.
    start_workers
    start_router "$policy"

    ./target/release/warmpath-bench run \
      --target "http://127.0.0.1:${ROUTER_PORT}" \
      --rate "$RATE" --duration "$DURATION" --warmup "$WARMUP" \
      --runs 1 --seed "$((100 + repetition))" --settle 0 \
      --prompt-words 24 --max-tokens 4 \
      --shared-prefix-words "$PREFIX_WORDS" --prefix-pool "$PREFIX_POOL" \
      --max-dispatch-lag-ms 100 \
      --label "$policy" \
      --out "${OUT}/${policy}"

    # The workers' own view, which is the number the router does not control.
    # Counters cover this arm alone, because these workers started with it.
    for index in $(seq 0 $((WORKERS - 1))); do
      curl -s "http://127.0.0.1:$((MOCK_BASE_PORT + index))/debug/stats"
      echo
    done > "${OUT}/${policy}/worker-stats-${repetition}.jsonl"

    stop_all
    sleep 0.5
  done
done

echo
echo "=== aggregating ==="
for policy in $POLICIES; do
  ./target/release/warmpath-bench aggregate \
    "${OUT}/${policy}"/*/ --out "${OUT}/${policy}/campaign.json" > /dev/null
done

python3 - "$OUT" "$POLICIES" "$RUNS" <<'PY'
import glob
import json
import sys

base, policies, runs = sys.argv[1], sys.argv[2].split(), int(sys.argv[3])

print(f"{'policy':<26} {'throughput':>12} {'p50 TTFT':>12} {'p99 TTFT':>12} {'95% CI on p99':>24}")
for policy in policies:
    metrics = json.load(open(f"{base}/{policy}/campaign.json"))["metrics"]
    p99 = metrics["ttft_from_intended_p99_us"]
    half = p99["ci95_half_width"] or 0.0
    interval = f"[{(p99['mean'] - half) / 1000:.1f}, {(p99['mean'] + half) / 1000:.1f}] ms"
    print(
        f"{policy:<26} {metrics['achieved_rate_per_second']['median']:>9.1f}/s "
        f"{metrics['ttft_from_intended_p50_us']['median'] / 1000:>10.1f}ms "
        f"{p99['median'] / 1000:>10.1f}ms {interval:>24}"
    )

print()
print(f"worker-reported prefix cache hit rate, in blocks, over {runs} run(s)")
for policy in policies:
    queries = hits = 0
    for path in sorted(glob.glob(f"{base}/{policy}/worker-stats-*.jsonl")):
        for line in open(path):
            line = line.strip()
            if not line:
                continue
            cache = json.loads(line)["cache"]
            queries += cache["prefix_cache_queries"]
            hits += cache["prefix_cache_hits"]
    rate = hits / queries if queries else 0.0
    print(f"  {policy:<26} {rate:>6.1%}   ({hits} of {queries} blocks)")
PY
