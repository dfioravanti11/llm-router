#!/usr/bin/env bash
#
# The policy comparison matrix: every policy against every workload shape.
#
# Two shapes, because they ask different questions. On an even workload the
# only thing that varies is cache locality. On a skewed one, where most requests
# share a single prefix as real traffic does, a policy that only maximises
# locality piles the fleet's work onto one worker.
#
# Everything lands under results/policy-matrix, one directory per shape.

set -euo pipefail

cd "$(dirname "$0")/.."

OUT=${OUT:-results/policy-matrix}
RUNS=${RUNS:-3}

for shape in even skewed; do
  echo
  echo "############ ${shape} workload ############"
  OUT="${OUT}/${shape}" SHAPE="$shape" RUNS="$RUNS" ./scripts/policy-compare.sh
done

echo
echo "Matrix written to ${OUT}"
