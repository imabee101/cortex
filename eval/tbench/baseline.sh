#!/bin/bash
# Terminal-Bench 2.0 run through the Cortex harness, one attempt per task.
# Usage: baseline.sh <label> <cortex binary> [task name ...]   (no task names = every task)
#
# Fairness: only one agent talks to the model at a time (--n-concurrent-agents 1). Harbor takes that
# permit at the agent-start event, before the agent's timer starts, so waiting for the GPU never counts
# against a task. Up to three trials are in flight, so the next containers start and the previous
# one is graded while the current agent works.
# Tasks that need the model to see an image or video are excluded: the served model has no vision.
# VERIFIER_UV_CACHE=<dir> hands the grader a pre-filled uv cache (see the adapter); unset for normal runs.
# The key is read from the host environment by the adapter; it is never passed on the command line.
set -euo pipefail
export PYTHONPATH=$(cd "$(dirname "$0")" && pwd)
cd "${CORTEX_TBENCH_HOME:-$HOME/.cache/cortex-tbench}"
label=${1:?label} binary=${2:?cortex binary}
shift 2
source ~/.config/cortex-test/tbench.env

vision=(chess-best-move code-from-image extract-moves-from-video path-tracing)
tasks=("$@")
if [ ${#tasks[@]} = 0 ]; then
  for t in dataset/terminal-bench-2/*/task.toml; do tasks+=("$(basename "$(dirname "$t")")"); done
fi

# Every image is on disk before the first trial, so no task spends its environment-start window downloading.
for t in "${tasks[@]}"; do
  img=$(grep -oE 'docker_image *= *"[^"]+"' "dataset/terminal-bench-2/$t/task.toml" | cut -d'"' -f2)
  docker image inspect "$img" > /dev/null 2>&1 && continue
  free=$(df --output=avail -BG / | tail -1 | tr -dc 0-9)
  [ "$free" -ge 20 ] || { echo "only ${free}G free; not pulling $img" >&2; exit 1; }
  docker pull -q "$img" > /dev/null
done

filters=()
[ $# -gt 0 ] && for t in "${tasks[@]}"; do filters+=(-i "$t"); done
for t in "${vision[@]}"; do filters+=(-x "$t"); done

exec .venv/bin/harbor run \
  -p dataset/terminal-bench-2 \
  -a cortex_tbench.agent:Cortex \
  --ak binary="$binary" --ak label="$label" ${VERIFIER_UV_CACHE:+--ak verifier_uv_cache="$VERIFIER_UV_CACHE"} \
  -n 3 --n-concurrent-agents 1 -k 1 \
  "${filters[@]}" \
  -o "$PWD/jobs" --job-name "$label-$(date +%Y%m%d-%H%M%S)"
