#!/usr/bin/env bash
#
# What the router costs.
#
# The spec asks for under 1ms added to p99 time to first token, measured rather
# than asserted. This measures it, against one worker, three ways:
#
#   direct                    the load generator talks to the worker
#   round-robin               through the router, which does not read the prompt
#   prefix-affinity-balanced  through the router doing all of its work: render
#                             the chat template, tokenize, chain the block
#                             hashes, query the index, reserve the blocks
#
# One worker in every arm, so the difference is the hop and the work, not a
# routing decision. Round-robin with a single worker still parses the request,
# proxies it, and streams it back; it just never builds a fingerprint. The gap
# between the second and third arms is therefore the price of being cache aware,
# separated from the price of being a proxy at all.
#
# Two things are tuned to make a sub-millisecond difference visible at all.
#
# The offered rate is low, because overhead is a fixed cost and a queue would
# bury it. At high utilization the tail is made of waiting, and waiting is the
# worker's, not the router's.
#
# The worker is made as close to free as it can be: no time to first token, no
# simulated prefill, one token. The router's work does not depend on how slow
# the worker is, so every millisecond the worker spends is a millisecond of
# variance added to a measurement of something else. The first version of this
# script left the worker's prefill at 120us per token, which put about 10ms of
# simulated work under a difference near 1ms, and the p99 intervals came out
# wide enough to contain zero and a negative overhead.

set -euo pipefail

cd "$(dirname "$0")/.."

# shellcheck source=scripts/bin-dir.sh
. "$(dirname "$0")/bin-dir.sh"
BIN=$(resolve_bin_dir)

OUT=${OUT:-results/overhead}
RUNS=${RUNS:-5}
DURATION=${DURATION:-35}
WARMUP=${WARMUP:-5}
RATE=${RATE:-50}

# The same prompt shape the policy comparisons use, because the cost of
# tokenizing is a function of prompt length and a short prompt would flatter the
# result.
PREFIX_WORDS=${PREFIX_WORDS:-256}
PREFIX_POOL=${PREFIX_POOL:-10}
BLOCK_SIZE=16
CACHE_BLOCKS=${CACHE_BLOCKS:-4096}

MOCK_PORT=${MOCK_PORT:-19001}
ROUTER_PORT=${ROUTER_PORT:-19080}

# Point this at a real inference server and no mock worker is started:
#
#   WORKER_URL=http://10.0.0.4:8000 MODEL=Qwen/Qwen3-1.7B ./scripts/overhead.sh
#
# One worker is all this measurement needs, because it compares a request that
# goes through the router against the same request sent straight to the same
# worker. It answers what the router costs, and it does not need a fleet.
MODEL=${MODEL:-mock-model}
if [ -n "${WORKER_URL:-}" ]; then
  EXTERNAL_WORKER=1
  WORKER_BASE="$WORKER_URL"
  echo "using the external worker at ${WORKER_BASE}, model ${MODEL}"
else
  EXTERNAL_WORKER=0
  WORKER_BASE="http://127.0.0.1:${MOCK_PORT}"
fi

MODEL_DIR=${MODEL_DIR:-.cache/qwen3-1.7b}
if [ -f "${MODEL_DIR}/tokenizer.json" ]; then
  MOCK_MODEL_ARGS=(--model-dir "$MODEL_DIR")
  ROUTER_MODEL_TOML=$'\n[model]\ndirectory = "'"${MODEL_DIR}"$'"\n'
  echo "using the model tokenizer at ${MODEL_DIR}"
else
  MOCK_MODEL_ARGS=()
  ROUTER_MODEL_TOML=""
  echo "no model at ${MODEL_DIR}; using the development tokenizer, which is far" >&2
  echo "cheaper than the real one and will understate the cost. Run scripts/fetch-model.sh." >&2
fi

cargo build --release --workspace
mkdir -p "$OUT"

MOCK_PID=""
ROUTER_PID=""

stop_all() {
  if [ -n "$ROUTER_PID" ]; then
    kill "$ROUTER_PID" 2>/dev/null || true
    ROUTER_PID=""
  fi
  if [ -n "$MOCK_PID" ]; then
    kill "$MOCK_PID" 2>/dev/null || true
    MOCK_PID=""
  fi
}
trap stop_all EXIT

start_worker() {
  # Somebody else runs the real one.
  if [ "$EXTERNAL_WORKER" -eq 1 ]; then
    return 0
  fi
  "${BIN}/warmpath-mock" \
    --bind "127.0.0.1:${MOCK_PORT}" \
    --ttft-ms 0 --inter-token-ms 0 \
    --max-concurrency 32 \
    --cache-blocks "$CACHE_BLOCKS" --block-size "$BLOCK_SIZE" \
    --prefill-per-token-us 0 \
    ${MOCK_MODEL_ARGS[@]+"${MOCK_MODEL_ARGS[@]}"} \
    > "${OUT}/worker.log" 2>&1 &
  MOCK_PID=$!
  disown "$MOCK_PID" 2>/dev/null || true
}

start_router() {
  local policy=$1
  # A stable path rather than a temp file, for two reasons. The router reads it
  # asynchronously after it is launched, so deleting it straight away is a race
  # this script lost once. And the exact configuration behind a published number
  # is worth keeping next to the number.
  local config="${OUT}/config-${policy}.toml"
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
${ROUTER_MODEL_TOML}
[[workers]]
name = "w0"
url = "${WORKER_BASE}"
TOML

  "${BIN}/warmpath" --config "$config" >> "${OUT}/router.log" 2>&1 &
  ROUTER_PID=$!
  disown "$ROUTER_PID" 2>/dev/null || true
}

wait_for() {
  local url=$1
  for _ in $(seq 1 60); do
    if curl -sf "$url" > /dev/null; then
      return 0
    fi
    sleep 0.2
  done
  echo "nothing came up at ${url}; see ${OUT}/*.log" >&2
  exit 1
}

# Arm name to the target the generator points at. "direct" has no router.
ARMS=${ARMS:-"direct round-robin prefix-affinity-balanced"}

run_arm() {
  local arm=$1 repetition=$2 target

  start_worker
  wait_for "${WORKER_BASE}/health"

  if [ "$arm" = "direct" ]; then
    target="${WORKER_BASE}"
  else
    start_router "$arm"
    wait_for "http://127.0.0.1:${ROUTER_PORT}/health"
    target="http://127.0.0.1:${ROUTER_PORT}"
  fi

  "${BIN}/warmpath-bench" run \
    --target "$target" \
    --model "$MODEL" \
    --rate "$RATE" --duration "$DURATION" --warmup "$WARMUP" \
    --runs 1 --seed "$((200 + repetition))" --settle 0 \
    --prompt-words 24 --max-tokens 1 \
    --shared-prefix-words "$PREFIX_WORDS" --prefix-pool "$PREFIX_POOL" \
    --max-dispatch-lag-ms 100 \
    --label "overhead/${arm}" \
    --out "${OUT}/${arm}"

  stop_all
  sleep 0.5
}

arm_list() {
  # Rotate, so no arm always runs on a cold laptop.
  local offset=$1
  local ordered=($ARMS)
  local count=${#ordered[@]}
  local i
  for ((i = 0; i < count; i++)); do
    printf '%s ' "${ordered[$(((i + offset) % count))]}"
  done
}

for repetition in $(seq 1 "$RUNS"); do
  for arm in $(arm_list "$repetition"); do
    echo
    echo "=== ${arm}, repetition ${repetition} of ${RUNS} ==="
    run_arm "$arm" "$repetition"
  done
done

for arm in $ARMS; do
  "${BIN}/warmpath-bench" aggregate \
    "${OUT}/${arm}"/*/ --out "${OUT}/${arm}/campaign.json" > /dev/null
done

echo
echo "=== router overhead, ${RUNS} runs per arm at ${RATE}/s ==="
python3 - "$OUT" "$ARMS" <<'PY'
import json
import sys

base, arms = sys.argv[1], sys.argv[2].split()

def metrics(arm):
    return json.load(open(f"{base}/{arm}/campaign.json"))["metrics"]

def cell(arm, key):
    stat = metrics(arm)[key]
    return stat["median"] / 1000.0, (stat["ci95_half_width"] or 0.0) / 1000.0

# Two clocks, and for this question they answer different things.
#
# From intended is the honest measure of what a client experiences, and it is
# the headline everywhere else in this project. It also carries the load
# generator's own scheduling lag, which on a laptop running the generator, the
# router and the worker at once is not small and is not the router's doing.
#
# From dispatch drops that lag. It would hide queueing that the router caused,
# which is why it is the wrong default. Here the worker is free and nothing
# queues, so what remains is the fixed cost of the hop, which is the thing being
# attributed.
CLOCKS = [("from intended", "ttft_from_intended"), ("from dispatch", "ttft_from_dispatch")]

for label, prefix in CLOCKS:
    rows = {
        arm: {
            "p50": cell(arm, f"{prefix}_p50_us"),
            "p99": cell(arm, f"{prefix}_p99_us"),
        }
        for arm in arms
    }

    print()
    print(f"time to first token, {label}")
    print(f"{'arm':<26} {'p50':>22} {'p99':>22}")
    for arm in arms:
        p50, p50_half = rows[arm]["p50"]
        p99, p99_half = rows[arm]["p99"]
        print(
            f"{arm:<26} {f'{p50:.2f} +/-{p50_half:.2f}ms':>22} "
            f"{f'{p99:.2f} +/-{p99_half:.2f}ms':>22}"
        )

    if "direct" not in rows:
        continue

    print(f"{'-- added by the router':<26} {'p50':>22} {'p99':>22}")
    for arm in arms:
        if arm == "direct":
            continue
        line = f"{arm:<26}"
        for key in ("p50", "p99"):
            value, half = rows[arm][key]
            base_value, base_half = rows["direct"][key]
            # Two independently measured medians, so the uncertainty on their
            # difference is the two half-widths added in quadrature.
            delta_half = (half**2 + base_half**2) ** 0.5
            delta = value - base_value
            flag = "" if abs(delta) > delta_half else " unresolved"
            line += f" {f'{delta:+.2f} +/-{delta_half:.2f}ms{flag}':>22}"
        print(line)

print()
print("A delta smaller than its own interval is marked unresolved, meaning this")
print("setup cannot tell it apart from zero. The spec asks for under 1ms added")
print("to p99, so an unresolved p99 is not a pass; it is a measurement that")
print("needs a quieter machine than a laptop running all three processes.")
PY
