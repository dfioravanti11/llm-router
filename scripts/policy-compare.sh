#!/usr/bin/env bash
#
# Compare routing policies on one workload shape.
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

# shellcheck source=scripts/bin-dir.sh
. "$(dirname "$0")/bin-dir.sh"
BIN=$(resolve_bin_dir)

OUT=${OUT:-results/policy-compare}
RUNS=${RUNS:-3}
# 60/s over a 30s measurement window across 3 runs is about 5,400 measured
# requests per policy, which clears the spec's floor of 5,000.
DURATION=${DURATION:-35}
WARMUP=${WARMUP:-5}
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

POLICIES=${POLICIES:-"round-robin least-loaded power-of-two prefix-affinity prefix-affinity-balanced"}

# Workload shape.
#
#   even    every prefix equally popular. Nothing hotspots, so this isolates
#           cache locality.
#   skewed  most requests share one prefix, as real traffic does. A policy that
#           only maximises locality sends most of the fleet's work to one
#           worker.
SHAPE=${SHAPE:-even}
case "$SHAPE" in
  even)
    HOT_SHARE=0
    MAX_TOKENS=4
    WORKER_CONCURRENCY=32
    INTER_TOKEN_MS=1
    RATE=${RATE:-60}
    ;;
  skewed)
    HOT_SHARE=0.8
    # Decode is not helped by the prefix cache, so a request holds a slot for
    # about 64ms whether or not its prefill hit. Without that, the worker
    # holding the hot prefix is the fastest one, concentrating traffic on it
    # costs nothing, and there is no hotspot to route around.
    # Eight tokens at 8ms is the same 64ms of worker occupancy as 32 at 2ms,
    # with a quarter of the timer wakeups and SSE chunks. The router, three
    # workers, and the load generator all share one laptop here, and the finer
    # schedule was enough to make the generator itself fall behind, which
    # correctly invalidated the run.
    MAX_TOKENS=8
    WORKER_CONCURRENCY=4
    INTER_TOKEN_MS=8
    # A worker with four slots and 64ms per request serves about 62/s. Eighty
    # percent of sixty arrivals a second is 48, so the hot worker sits near 77%
    # utilization: deep enough to queue, stable enough to have a steady state.
    #
    # Offering more does not make a better experiment. Past the hot worker's
    # capacity the queue grows for as long as the run lasts, so the reported
    # tail becomes a function of run length, and far enough past it the
    # generator itself falls behind and the run is correctly thrown out. The
    # fleet as a whole is at about a third of capacity either way, so only the
    # hotspot is under pressure.
    RATE=${RATE:-60}
    ;;
  *)
    echo "unknown SHAPE '${SHAPE}'; expected even or skewed" >&2
    exit 1
    ;;
esac

MOCK_BASE_PORT=${MOCK_BASE_PORT:-19001}
ROUTER_PORT=${ROUTER_PORT:-19080}

# Point this at real inference servers and no mock workers are started:
#
#   WORKER_URLS="http://10.0.0.4:8000 http://10.0.0.5:8000" \
#   MODEL=Qwen/Qwen3-1.7B ./scripts/policy-matrix.sh
#
# The model name has to be one the servers will answer to. vLLM rejects a
# request whose model field it does not recognise, and the default here is the
# mock's name.
MODEL=${MODEL:-mock-model}
EXTERNAL_WORKERS=0
if [ -n "${WORKER_URLS:-}" ]; then
  EXTERNAL_WORKERS=1
  # No `readarray` here: macOS ships bash 3.2.
  WORKER_URL_LIST=()
  for url in $WORKER_URLS; do
    WORKER_URL_LIST+=("$url")
  done
  WORKERS=${#WORKER_URL_LIST[@]}
  echo "using ${WORKERS} external workers, model ${MODEL}"
fi

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
  if [ "$EXTERNAL_WORKERS" -eq 1 ]; then
    worker_url="${WORKER_URL_LIST[$index]}"
  else
    worker_url="http://127.0.0.1:$((MOCK_BASE_PORT + index))"
  fi
  worker_toml+=$'\n[[workers]]\nname = "w'"${index}"$'"\nurl = "'"${worker_url}"$'"\n'
done

# The first worker, whichever kind it is, is the one the readiness loop waits on.
if [ "$EXTERNAL_WORKERS" -eq 1 ]; then
  FIRST_WORKER_URL="${WORKER_URL_LIST[0]}"
else
  FIRST_WORKER_URL="http://127.0.0.1:${MOCK_BASE_PORT}"
fi

start_workers() {
  # External servers are somebody else's to run. Each arm still needs to begin
  # with an empty cache, which is what `reset_worker_caches` is for.
  if [ "$EXTERNAL_WORKERS" -eq 1 ]; then
    return 0
  fi
  for index in $(seq 0 $((WORKERS - 1))); do
    "${BIN}/warmpath-mock" \
      --bind "127.0.0.1:$((MOCK_BASE_PORT + index))" \
      --ttft-ms 1 --inter-token-ms "$INTER_TOKEN_MS" \
      --max-concurrency "$WORKER_CONCURRENCY" \
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

[health]
poll_interval_ms = 100
${ROUTER_MODEL_TOML}${worker_toml}
TOML

  "${BIN}/warmpath" --config "$config" >> "${OUT}/router.log" 2>&1 &
  ROUTER_PID=$!
  disown "$ROUTER_PID" 2>/dev/null || true

  for _ in $(seq 1 60); do
    if curl -sf "http://127.0.0.1:${ROUTER_PORT}/health" > /dev/null \
      && curl -sf "${FIRST_WORKER_URL}/health" > /dev/null; then
      rm -f "$config"
      return 0
    fi
    sleep 0.2
  done
  echo "router or workers did not come up for ${policy}; see ${OUT}/router.log" >&2
  exit 1
}

# Each arm has to start from an empty cache, or it inherits whatever the
# previous policy left warm and the comparison measures the order the arms ran
# in. With mock workers this is free, since they are started fresh per arm.
# Real servers keep running, so ask them to drop the cache instead.
reset_worker_caches() {
  local url

  # Some engines expose no way to drop the cache. vLLM hides its endpoint behind
  # a dev flag, and older builds do not have it at all. WORKER_RESET_CMD is the
  # way out: give it a command that restarts the servers, and it runs instead.
  #
  #   WORKER_RESET_CMD="sudo docker restart vllm-0 vllm-1"
  if [ -n "${WORKER_RESET_CMD:-}" ]; then
    echo "resetting workers with: ${WORKER_RESET_CMD}"
    if ! eval "$WORKER_RESET_CMD"; then
      echo "WORKER_RESET_CMD failed, so this arm would start with a warm cache" >&2
      exit 1
    fi
    wait_for_workers
    return 0
  fi

  for url in ${WORKER_URL_LIST[@]+"${WORKER_URL_LIST[@]}"}; do
    if ! curl -sf -X POST "${url}/reset_prefix_cache" -o /dev/null 2>/dev/null; then
      echo "WARNING: ${url} did not accept /reset_prefix_cache. This arm starts" >&2
      echo "         with whatever the previous one left cached, and the run is" >&2
      echo "         not comparable. Set WORKER_RESET_CMD to restart the servers" >&2
      echo "         instead." >&2
    fi
  done
}

# A restarted inference server takes far longer to answer than a mock does, so
# this waits minutes rather than seconds.
wait_for_workers() {
  local url attempt
  for url in ${WORKER_URL_LIST[@]+"${WORKER_URL_LIST[@]}"}; do
    for attempt in $(seq 1 "${WORKER_READY_TRIES:-600}"); do
      if curl -sf "${url}/health" > /dev/null 2>&1; then
        break
      fi
      if [ "$attempt" -eq "${WORKER_READY_TRIES:-600}" ]; then
        echo "${url} did not come back after its reset" >&2
        exit 1
      fi
      sleep 0.5
    done
  done
  echo "all workers are answering again"
}

# vLLM's counters run for the life of the process, so an arm's own figures are
# the difference across it. The mock is restarted per arm and needs no such care.
snapshot_worker_counters() {
  local out=$1
  local url
  : > "$out"
  for url in ${WORKER_URL_LIST[@]+"${WORKER_URL_LIST[@]}"}; do
    curl -sf "${url}/metrics" >> "$out" 2>/dev/null || true
    echo "### END OF WORKER ###" >> "$out"
  done
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
    echo "=== ${SHAPE} / ${policy}, repetition ${repetition} of ${RUNS} ==="

    # Fresh workers per arm: every policy starts from an empty cache, and the
    # warmup window absorbs the cold start.
    start_workers
    if [ "$EXTERNAL_WORKERS" -eq 1 ]; then
      reset_worker_caches
      snapshot_worker_counters "${OUT}/.counters-before"
    fi
    start_router "$policy"

    "${BIN}/warmpath-bench" run \
      --target "http://127.0.0.1:${ROUTER_PORT}" \
      --rate "$RATE" --duration "$DURATION" --warmup "$WARMUP" \
      --runs 1 --seed "$((100 + repetition))" --settle 0 \
      --model "$MODEL" \
      --prompt-words 24 --max-tokens "$MAX_TOKENS" \
      --shared-prefix-words "$PREFIX_WORDS" --prefix-pool "$PREFIX_POOL" \
      --hot-prefix-share "$HOT_SHARE" \
      --max-dispatch-lag-ms 100 \
      --label "${SHAPE}/${policy}" \
      --out "${OUT}/${policy}"

    # The workers' own view, which is the number the router does not control.
    if [ "$EXTERNAL_WORKERS" -eq 1 ]; then
      # Real servers: this arm's figures are the difference across it, and the
      # per-worker request counts come from the router, whose own counters do
      # start at zero here because it is restarted for every arm.
      snapshot_worker_counters "${OUT}/.counters-after"
      curl -sf "http://127.0.0.1:${ROUTER_PORT}/metrics" \
        > "${OUT}/.router-metrics" 2>/dev/null || true
      python3 scripts/worker-stats.py \
        "${OUT}/.counters-before" "${OUT}/.counters-after" "${OUT}/.router-metrics" \
        > "${OUT}/${policy}/worker-stats-${repetition}.jsonl"
      rm -f "${OUT}/.counters-before" "${OUT}/.counters-after" "${OUT}/.router-metrics"
    else
      # Counters cover this arm alone, because these workers started with it.
      for index in $(seq 0 $((WORKERS - 1))); do
        curl -s "http://127.0.0.1:$((MOCK_BASE_PORT + index))/debug/stats"
        echo
      done > "${OUT}/${policy}/worker-stats-${repetition}.jsonl"
    fi

    stop_all
    sleep 0.5
  done
done

echo
echo "=== ${SHAPE} workload ==="
for policy in $POLICIES; do
  "${BIN}/warmpath-bench" aggregate \
    "${OUT}/${policy}"/*/ --out "${OUT}/${policy}/campaign.json" > /dev/null
done

python3 - "$OUT" "$POLICIES" "$RUNS" <<'PY'
import glob
import json
import sys

base, policies, runs = sys.argv[1], sys.argv[2].split(), int(sys.argv[3])

print(
    f"{'policy':<26} {'throughput':>11} {'p50 TTFT':>10} {'p99 TTFT':>10} "
    f"{'95% CI on p99':>22} {'hit rate':>9} {'busiest':>8}"
)

for policy in policies:
    metrics = json.load(open(f"{base}/{policy}/campaign.json"))["metrics"]
    p99 = metrics["ttft_from_intended_p99_us"]
    half = p99["ci95_half_width"] or 0.0
    interval = f"[{(p99['mean'] - half) / 1000:.1f}, {(p99['mean'] + half) / 1000:.1f}] ms"

    # The workers' own counters: prefix cache hit rate, and the share of the
    # fleet's requests the busiest worker took. A third is even across three
    # workers; anything near one is a hotspot.
    queries = hits = 0
    concentration = []
    for path in sorted(glob.glob(f"{base}/{policy}/worker-stats-*.jsonl")):
        started = []
        for line in open(path):
            line = line.strip()
            if not line:
                continue
            stats = json.loads(line)
            queries += stats["cache"]["prefix_cache_queries"]
            hits += stats["cache"]["prefix_cache_hits"]
            started.append(stats["started"])
        if started and sum(started):
            concentration.append(max(started) / sum(started))

    hit_rate = f"{hits / queries:.1%}" if queries else "n/a"
    busiest = f"{sum(concentration) / len(concentration):.0%}" if concentration else "n/a"

    print(
        f"{policy:<26} {metrics['achieved_rate_per_second']['median']:>8.1f}/s "
        f"{metrics['ttft_from_intended_p50_us']['median'] / 1000:>8.1f}ms "
        f"{p99['median'] / 1000:>8.1f}ms {interval:>22} {hit_rate:>9} {busiest:>8}"
    )

print()
print(f"{runs} run(s) per policy. 'busiest' is the share of requests the busiest")
print("worker took; a third is even across three workers.")
PY
