#!/usr/bin/env bash
#
# Regenerate every number published in RESULTS.md, in the order RESULTS.md
# presents them, and then redraw the charts.
#
# R0.5 ships when a stranger can check this repo out, run one command, and get
# the published numbers back. Before this, they came from five invocations and a
# test, one of them a bare call to a script with three environment variables set
# in front of it. Nothing wrote down what order they went in or which table each
# one fed. This closes that.
#
# Every phase below calls a script that already exists. Those scripts start
# their own workers, wait for health, stop everything on the way out, and print
# their own summary table. None of that is repeated here. What this adds is
# ordering, a clean output directory per phase so a re-run cannot aggregate
# yesterday's runs together with today's, a fast smoke mode, and a manifest of
# what landed where.
#
# Usage:
#
#   ./scripts/reproduce.sh            the real thing, about an hour
#   ./scripts/reproduce.sh --smoke    the same pipeline at toy settings
#
# Smoke output is written to results/smoke and is not publishable. It exists to
# prove the pipeline runs end to end before anyone commits an hour to it.

set -euo pipefail

cd "$(dirname "$0")/.."

# Rust on the development machine is keg-only homebrew rustup, so cargo is not
# on the default PATH. Exporting it here covers the child scripts too, since
# they inherit this environment. Prepending a directory that does not exist is
# harmless, so this is safe on a machine with a normal rustup install.
export PATH="/opt/homebrew/opt/rustup/bin:$PATH"

SMOKE=${SMOKE:-0}
for arg in "$@"; do
  case "$arg" in
    --smoke) SMOKE=1 ;;
    --full) SMOKE=0 ;;
    -h|--help)
      echo "usage: $0 [--smoke]"
      echo
      echo "  --smoke   short runs into results/smoke, for validating the pipeline"
      echo "  (default) the published configuration into results/"
      exit 0
      ;;
    *)
      echo "unknown argument '${arg}'; try --smoke or --help" >&2
      exit 2
      ;;
  esac
done

if [ "$SMOKE" = 1 ]; then
  RESULTS_ROOT=${RESULTS_ROOT:-results/smoke}
else
  RESULTS_ROOT=${RESULTS_ROOT:-results}
fi
LOG_DIR="${RESULTS_ROOT}/reproduce"

# Model tokenizer and chat template. Every published number was measured with
# the model's own tokenizer, and the one arm that deliberately uses the
# development tokenizer asks for it explicitly.
MODEL_DIR=${MODEL_DIR:-.cache/qwen3-1.7b}

# One retry per phase. The comparison scripts give a router and its workers
# twelve seconds to answer a health check, which is generous on an idle machine
# and not enough on one that is compiling something else, and a phase that dies
# that way wastes the whole run. Set RETRIES=0 when you want the first failure
# to stand.
RETRIES=${RETRIES:-1}
RETRY_PAUSE=${RETRY_PAUSE:-15}

# Phases, in the order RESULTS.md reads. Keys are function-name safe.
PHASES="policy_matrix capacity crossover overhead overhead_devtok co_demo"

phase_title() {
  case "$1" in
    policy_matrix)   echo "the policy matrix, even and skewed" ;;
    capacity)        echo "capacity, the under-provisioned fleet" ;;
    crossover)       echo "the crossover test, affinity buying nothing" ;;
    overhead)        echo "what the router itself costs" ;;
    overhead_devtok) echo "the same cost with the development tokenizer" ;;
    co_demo)         echo "open loop against closed loop" ;;
  esac
}

# Which section of RESULTS.md each phase feeds, for the manifest.
phase_feeds() {
  case "$1" in
    policy_matrix)   echo "The policy matrix (both tables)" ;;
    capacity)        echo "Capacity decides whether the tail improves" ;;
    crossover)       echo "The other crossover: when affinity buys nothing at all" ;;
    overhead)        echo "What the router itself costs" ;;
    overhead_devtok) echo "Most of that 1.2ms is tokenizing" ;;
    co_demo)         echo "Closed-loop load generators under-report the tail" ;;
  esac
}

# Output directory, or empty for a phase that writes no run data.
phase_dir() {
  case "$1" in
    policy_matrix)   echo "${RESULTS_ROOT}/policy-matrix" ;;
    capacity)        echo "${RESULTS_ROOT}/policy-compare" ;;
    crossover)       echo "" ;;
    overhead)        echo "${RESULTS_ROOT}/overhead" ;;
    overhead_devtok) echo "${RESULTS_ROOT}/overhead-devtok" ;;
    co_demo)         echo "${RESULTS_ROOT}/co-demo" ;;
  esac
}

# Rough wall clock per phase at published settings, for the plan printed up
# front. Measured on the laptop these numbers came from.
phase_estimate() {
  case "$1" in
    policy_matrix)   echo "~22 min" ;;
    capacity)        echo "~11 min" ;;
    crossover)       echo "~2 min" ;;
    overhead)        echo "~11 min" ;;
    overhead_devtok) echo "~7 min" ;;
    co_demo)         echo "~4 min" ;;
  esac
}

# Smoke settings, applied inside each phase's subshell so they reach the child
# script and its own children. The published configuration is whatever each
# script defaults to, so full mode exports nothing and the defaults stand.
#
# Eight seconds with a two second warmup leaves six measured seconds, which at
# the rates these scripts offer is a few hundred requests. Far too few to say
# anything about a p99, plenty to prove every service starts, every run
# validates, and every aggregation writes a campaign.
smoke_env() {
  [ "$SMOKE" = 1 ] || return 0
  export RUNS=1
  export DURATION=8
  export WARMUP=2
}

phase_policy_matrix() {
  (
    smoke_env
    export OUT="${RESULTS_ROOT}/policy-matrix"
    ./scripts/policy-matrix.sh
  )
}

phase_capacity() {
  (
    smoke_env
    export OUT="${RESULTS_ROOT}/policy-compare"
    export CACHE_BLOCKS=64
    ./scripts/policy-compare.sh
  )
}

phase_crossover() {
  # The claim that affinity buys nothing when the working set fits everywhere
  # lives in a test rather than in a benchmark run, so reproducing it means
  # running that test. Release, because a debug build would compile the world a
  # second time for a test that takes two minutes.
  #
  # Building the test target can invalidate the release binaries, so the phase
  # after this one may rebuild them. That costs a couple of minutes and it
  # happens between phases, never while anything is being measured.
  cargo test --release -p warmpath-bench --test affinity
}

phase_overhead() {
  (
    smoke_env
    export OUT="${RESULTS_ROOT}/overhead"
    ./scripts/overhead.sh
  )
}

phase_overhead_devtok() {
  (
    smoke_env
    export OUT="${RESULTS_ROOT}/overhead-devtok"
    # A path that cannot hold a tokenizer is how the script is told to fall back
    # to the development one. That fallback is the experiment here.
    export MODEL_DIR=/nonexistent
    export ARMS="direct prefix-affinity-balanced"
    ./scripts/overhead.sh
  )
}

phase_co_demo() {
  (
    smoke_env
    export OUT="${RESULTS_ROOT}/co-demo"
    ./scripts/co-demo.sh
  )
}

elapsed() {
  local seconds=$1
  printf '%dm%02ds' $((seconds / 60)) $((seconds % 60))
}

rule() {
  echo "================================================================"
}

# ---------------------------------------------------------------- preflight

mkdir -p "$LOG_DIR"
STATUS_FILE="${LOG_DIR}/.status"
: > "$STATUS_FILE"

for tool in cargo python3 curl; do
  if ! command -v "$tool" > /dev/null 2>&1; then
    echo "${tool} is not on PATH, and every phase needs it" >&2
    exit 1
  fi
done

if [ "$SMOKE" = 1 ]; then
  cat > "${RESULTS_ROOT}/DO-NOT-PUBLISH.txt" <<'TXT'
Smoke output.

These runs used a few seconds of traffic and one repetition per arm, so their
percentiles are noise and their confidence intervals are undefined. Nothing in
here belongs in RESULTS.md or in a commit. It exists only to show that the
pipeline starts its services, produces run directories, and aggregates them.

The publishable numbers come from ./scripts/reproduce.sh with no arguments,
which writes to results/ instead.
TXT
fi

# The published numbers were all measured with Qwen3's tokenizer, and the
# router cuts blocks where the tokenizer says. Running without it produces a
# different experiment that looks identical from the outside, which RESULTS.md
# calls out as the failure this project most wants to avoid.
if [ ! -f "${MODEL_DIR}/tokenizer.json" ]; then
  echo "no tokenizer at ${MODEL_DIR}, fetching it"
  if ! ./scripts/fetch-model.sh > "${LOG_DIR}/fetch-model.txt" 2>&1; then
    if [ "$SMOKE" = 1 ]; then
      echo "the fetch failed, see ${LOG_DIR}/fetch-model.txt" >&2
      echo "smoke mode carries on with the development tokenizer, since it is" >&2
      echo "checking the plumbing rather than measuring anything" >&2
    else
      echo "the fetch failed, see ${LOG_DIR}/fetch-model.txt" >&2
      echo "the published numbers need the model's own tokenizer, so this stops here" >&2
      exit 1
    fi
  fi
fi

# One build up front. Each script builds again and finds nothing to do, and a
# compile error surfaces in thirty seconds rather than after the first phase has
# already burned twenty minutes.
echo "building the workspace in release"
if ! cargo build --release --workspace > "${LOG_DIR}/build.txt" 2>&1; then
  echo "the release build failed; the last of it:" >&2
  tail -30 "${LOG_DIR}/build.txt" >&2
  exit 1
fi

GIT_SHA=$(git rev-parse --short HEAD 2>/dev/null || echo unknown)
STARTED_AT=$(date '+%Y-%m-%d %H:%M:%S')
RUN_START=$(date +%s)

rule
if [ "$SMOKE" = 1 ]; then
  echo "warmpath reproduce, SMOKE MODE"
  echo
  echo "Short runs, one repetition per arm, into ${RESULTS_ROOT}."
  echo "The output is not publishable. It shows the pipeline works."
else
  echo "warmpath reproduce"
  echo
  echo "Regenerating every number in RESULTS.md into ${RESULTS_ROOT}."
fi
echo "git ${GIT_SHA}, started ${STARTED_AT}"
echo
for phase in $PHASES; do
  if [ "$SMOKE" = 1 ]; then
    printf '  %-18s %s\n' "$phase" "$(phase_title "$phase")"
  else
    printf '  %-18s %-8s %s\n' "$phase" "$(phase_estimate "$phase")" "$(phase_title "$phase")"
  fi
done
echo
if [ "$SMOKE" = 1 ]; then
  echo "A few minutes in total."
else
  echo "About an hour in total on a quiet laptop, longer if anything else is"
  echo "running. Every phase holds the machine to itself while it measures, so"
  echo "leave it alone."
fi
if [ "$RETRIES" -gt 0 ]; then
  echo "A phase that fails gets ${RETRIES} more attempt(s) before the run gives up on it."
fi
rule

# ------------------------------------------------------------------- phases

FAILED=""

for phase in $PHASES; do
  title=$(phase_title "$phase")
  out_dir=$(phase_dir "$phase")
  log="${LOG_DIR}/${phase}.txt"

  echo
  rule
  echo "PHASE ${phase}: ${title}"
  rule

  phase_start=$(date +%s)
  phase_status=failed
  attempt=0
  while [ "$attempt" -le "$RETRIES" ]; do
    attempt=$((attempt + 1))

    # A stale run directory is worse than no run directory. The comparison
    # scripts aggregate every run directory they find under a policy, so leaving
    # last week's runs in place would silently average them into this week's
    # number, and the worker cache counters would be summed across both. Git
    # holds the previous copy of anything published, so removing it is
    # recoverable. A retry clears the half-finished attempt for the same reason.
    if [ -n "$out_dir" ] && [ -d "$out_dir" ]; then
      echo "clearing ${out_dir} so nothing old is aggregated in"
      rm -rf "$out_dir"
    fi

    # A retry appends rather than overwrites, so the log still holds whatever
    # the first attempt said before it died. That is usually the useful half.
    if [ "$attempt" -eq 1 ]; then
      tee_mode=""
    else
      tee_mode="-a"
    fi
    if "phase_${phase}" 2>&1 | tee $tee_mode "$log"; then
      phase_status=ok
      break
    fi

    if [ "$attempt" -le "$RETRIES" ]; then
      # Almost every failure seen so far is a service missing its health window
      # because the machine was busy compiling something else. That is worth one
      # retry, and a pause first, because retrying into the same load fails the
      # same way.
      echo
      echo "!! ${phase} failed on attempt ${attempt}, waiting ${RETRY_PAUSE}s and trying once more" >&2
      sleep "$RETRY_PAUSE"
    fi
  done
  if [ "$phase_status" = failed ]; then
    FAILED="${FAILED} ${phase}"
  fi
  phase_end=$(date +%s)

  echo "${phase} ${phase_status} $((phase_end - phase_start))" >> "$STATUS_FILE"

  if [ "$phase_status" = failed ]; then
    # Carrying on rather than stopping. A phase that fails late costs the
    # phases after it too if this exits here, and the phases are independent
    # experiments against separate services. The exit code at the end still
    # says the run was not clean.
    echo
    echo "!! ${phase} failed after $(elapsed $((phase_end - phase_start))) and ${attempt} attempt(s)" >&2
    echo "!! its output is in ${log}; continuing with the rest" >&2
  else
    echo
    echo "-- ${phase} done in $(elapsed $((phase_end - phase_start))), ${attempt} attempt(s)"
  fi
done

# -------------------------------------------------------------------- charts

echo
rule
echo "PHASE charts: redrawing from ${RESULTS_ROOT}"
rule

CHART_STATUS="skipped"
CHART_NOTE=""
chart_log="${LOG_DIR}/charts.txt"

if [ ! -f scripts/plot.py ]; then
  CHART_NOTE="scripts/plot.py does not exist yet, so the charts were not drawn"
  echo "$CHART_NOTE"
else
  # The plot script is developed separately from this one, so its argument
  # handling is not something to assume. Passing the results root positionally
  # is the contract; a script that ignores arguments and reads results/ still
  # does the right thing in a full run.
  #
  # In smoke mode a fallback to the bare call would redraw the committed charts
  # from the committed data, which looks like success and quietly overwrites
  # published output. So smoke only ever tries the form that names its own
  # directory, and reports rather than falling back.
  if python3 scripts/plot.py "$RESULTS_ROOT" > "$chart_log" 2>&1; then
    CHART_STATUS="ok"
    cat "$chart_log"
  elif [ "$SMOKE" = 0 ] && python3 scripts/plot.py >> "$chart_log" 2>&1; then
    CHART_STATUS="ok"
    CHART_NOTE="plot.py took no results-root argument, so it ran bare against results/"
    cat "$chart_log"
    echo "$CHART_NOTE"
  else
    CHART_STATUS="failed"
    CHART_NOTE="plot.py ran and failed, see ${chart_log}"
    echo "$CHART_NOTE" >&2
    tail -20 "$chart_log" >&2
    if [ "$SMOKE" = 1 ]; then
      echo "smoke output lives under ${RESULTS_ROOT}, which plot.py may not know" >&2
      echo "how to read; a full run is the real check on the charts" >&2
    else
      # Charts are part of what a full run is for, so a failure here belongs in
      # the exit code. Smoke is exempt: plot.py may only know how to read
      # results/, and a smoke run that measured everything correctly should not
      # be called a failure over a directory it was told to keep separate.
      FAILED="${FAILED} charts"
    fi
  fi
fi

# ------------------------------------------------------------------ manifest

RUN_END=$(date +%s)
TOTAL=$((RUN_END - RUN_START))

MANIFEST="${LOG_DIR}/manifest.txt"
{
  echo "warmpath reproduce manifest"
  echo
  if [ "$SMOKE" = 1 ]; then
    echo "MODE          smoke, NOT PUBLISHABLE"
  else
    echo "MODE          full, the published configuration"
  fi
  echo "git           ${GIT_SHA}"
  echo "started       ${STARTED_AT}"
  echo "took          $(elapsed "$TOTAL")"
  echo "results root  ${RESULTS_ROOT}"
  echo
  printf '%-18s %-8s %-9s %-6s %s\n' "phase" "result" "took" "runs" "output"
  while read -r phase phase_status phase_seconds; do
    [ -n "$phase" ] || continue
    out_dir=$(phase_dir "$phase")
    if [ -n "$out_dir" ] && [ -d "$out_dir" ]; then
      run_count=$(find "$out_dir" -name report.json 2>/dev/null | wc -l | tr -d ' ')
      where="$out_dir"
    else
      run_count="-"
      where="${LOG_DIR}/${phase}.txt"
    fi
    printf '%-18s %-8s %-9s %-6s %s\n' \
      "$phase" "$phase_status" "$(elapsed "$phase_seconds")" "$run_count" "$where"
  done < "$STATUS_FILE"
  printf '%-18s %-8s %-9s %-6s %s\n' "charts" "$CHART_STATUS" "-" "-" "scripts/plot.py"
  if [ -n "$CHART_NOTE" ]; then
    echo "                   ${CHART_NOTE}"
  fi

  echo
  echo "which section of RESULTS.md each phase feeds"
  for phase in $PHASES; do
    printf '  %-18s %s\n' "$phase" "$(phase_feeds "$phase")"
  done

  echo
  echo "per phase, the console output including its summary table"
  for phase in $PHASES; do
    echo "  ${LOG_DIR}/${phase}.txt"
  done

  echo
  echo "each run directory holds report.json with the config, seed and git SHA,"
  echo "percentiles.csv ready to plot, and records.jsonl with one line per"
  echo "request. Campaign aggregates with confidence intervals sit next to them"
  echo "in campaign.json."

  charts=$(find "$RESULTS_ROOT" charts -type f \( -name '*.png' -o -name '*.svg' \) 2>/dev/null | sort || true)
  if [ -n "$charts" ]; then
    echo
    echo "charts"
    echo "$charts" | sed 's/^/  /'
  fi

  if [ "$SMOKE" = 1 ]; then
    echo
    echo "This was a smoke run. One repetition of a few seconds per arm says"
    echo "nothing about a tail latency and its confidence intervals are"
    echo "meaningless. Do not put any of it in RESULTS.md and do not commit it."
    echo "Run ./scripts/reproduce.sh with no arguments for the real numbers."
  fi

  if [ -n "$FAILED" ]; then
    echo
    echo "FAILED PHASES:${FAILED}"
    echo "Anything published from this run is incomplete."
  fi
} > "$MANIFEST"

echo
rule
cat "$MANIFEST"
rule
echo "this manifest is also at ${MANIFEST}"

rm -f "$STATUS_FILE"

if [ -n "$FAILED" ]; then
  exit 1
fi
