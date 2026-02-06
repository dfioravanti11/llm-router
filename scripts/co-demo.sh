#!/usr/bin/env bash
#
# The coordinated-omission demonstration.
#
# Runs the same worker under two generators: an open-loop one that keeps
# arriving on schedule, and a closed-loop one that waits for each response
# before sending the next. The closed-loop run moves at least as much traffic
# and reports a far better tail, because a generator that waits cannot produce
# the load shape that causes tail latency.
#
# Everything lands under results/co-demo, one directory per run plus a campaign
# summary per mode.

set -euo pipefail

cd "$(dirname "$0")/.."

OUT=${OUT:-results/co-demo}
DURATION=${DURATION:-30}
WARMUP=${WARMUP:-5}
RUNS=${RUNS:-3}
RATE=${RATE:-38}
CONCURRENCY=${CONCURRENCY:-2}

MOCK_PORT=${MOCK_PORT:-18001}
ROUTER_PORT=${ROUTER_PORT:-18080}

cargo build --release --workspace

# The output directory holds the run data and both service logs.
mkdir -p "$OUT"

CONFIG=$(mktemp)
cat > "$CONFIG" <<TOML
[server]
bind = "127.0.0.1:${ROUTER_PORT}"
max_request_bytes = 4194304

[upstream]
connect_timeout_ms = 2000
read_timeout_ms = 60000

[routing]
policy = "round-robin"

[[workers]]
name = "w0"
url = "http://127.0.0.1:${MOCK_PORT}"
TOML

# Two serving slots and about 46ms per response end to end, so the worker tops
# out near 43 requests per second. The default offered rate sits just under
# that: high utilization with a queue that still reaches a steady state.
#
# Push RATE past 43 and the comparison gets more dramatic and less meaningful.
# A persistently overloaded open-loop system has no steady state, so its
# latency grows with however long the run lasted, and the number stops being a
# property of the system.
./target/release/warmpath-mock \
  --bind "127.0.0.1:${MOCK_PORT}" \
  --ttft-ms 10 --inter-token-ms 5 --max-concurrency 2 \
  > "${OUT}/mock.log" 2>&1 &
MOCK_PID=$!
disown "$MOCK_PID" 2>/dev/null || true

./target/release/warmpath --config "$CONFIG" > "${OUT}/router.log" 2>&1 &
ROUTER_PID=$!
disown "$ROUTER_PID" 2>/dev/null || true

cleanup() {
  kill "$MOCK_PID" "$ROUTER_PID" 2>/dev/null || true
  rm -f "$CONFIG"
}
trap cleanup EXIT

# Wait for both to answer before measuring anything. Failing here beats
# running two minutes of benchmark against nothing.
ready=false
for _ in $(seq 1 50); do
  if curl -sf "http://127.0.0.1:${ROUTER_PORT}/health" > /dev/null \
    && curl -sf "http://127.0.0.1:${MOCK_PORT}/health" > /dev/null; then
    ready=true
    break
  fi
  sleep 0.2
done
if [ "$ready" != true ]; then
  echo "router or worker never came up; see ${OUT}/router.log and ${OUT}/mock.log" >&2
  exit 1
fi

BENCH_ARGS=(
  --target "http://127.0.0.1:${ROUTER_PORT}"
  --duration "$DURATION" --warmup "$WARMUP" --runs "$RUNS"
  --max-tokens 4 --prompt-words 32
)

echo
echo "=== open loop, ${RATE} arrivals per second ==="
./target/release/warmpath-bench run "${BENCH_ARGS[@]}" \
  --rate "$RATE" --max-dispatch-lag-ms 100 --out "${OUT}/open-loop"

echo
echo "=== closed loop, ${CONCURRENCY} concurrent callers ==="
./target/release/warmpath-bench run "${BENCH_ARGS[@]}" \
  --mode closed-loop --concurrency "$CONCURRENCY" --out "${OUT}/closed-loop"

echo
echo "=== comparison ==="
python3 - "$OUT" <<'PY'
import json
import sys

base = sys.argv[1]
print(f"{'generator':<22} {'throughput':>12} {'p50 TTFT':>12} {'p99 TTFT':>14} {'95% CI on p99':>26}")
for label, path in (
    ("open loop", f"{base}/open-loop/campaign.json"),
    ("closed loop", f"{base}/closed-loop/campaign.json"),
):
    metrics = json.load(open(path))["metrics"]

    def ms(key):
        return metrics[key]["median"] / 1000.0

    p99 = metrics["ttft_from_intended_p99_us"]
    half = p99["ci95_half_width"] or 0.0
    interval = f"[{(p99['mean'] - half) / 1000:.1f}, {(p99['mean'] + half) / 1000:.1f}] ms"
    rate = metrics["achieved_rate_per_second"]["median"]
    print(
        f"{label:<22} {rate:>9.1f}/s {ms('ttft_from_intended_p50_us'):>10.1f}ms "
        f"{ms('ttft_from_intended_p99_us'):>12.1f}ms {interval:>26}"
    )
PY
