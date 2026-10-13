#!/bin/bash
# Exits, with the reason on stdout, the moment a benchmark run shows a fault that is ours rather than the model's:
# a trial that ended in a setup/environment error, an error response from cortex-api or llama-server, a container
# killed by the shared memory ceiling, the host running short of memory or disk, or two agents running at once.
# Usage: watch.sh <job dir>. State lives in <job dir>/.watch so a restarted watcher does not re-report.
set -uo pipefail
job=${1:?job dir}
state=$job/.watch
mkdir -p "$state"
touch "$state/seen"
since=$(cat "$state/since" 2>/dev/null || date '+%Y-%m-%d %H:%M:%S')
# The "oom" line of memory.events.local counts only the slice's own 22 GB ceiling being hit (the 24 GB test hit it twice); a task container hitting its own
# memory_mb limit (part of the task spec, so a model outcome) is counted in memory.events but not here.
slice_kills() { awk '$1 == "oom" {print $2}' /sys/fs/cgroup/tbench.slice/memory.events.local 2>/dev/null || echo 0; }
kills=$(cat "$state/slice_kills" 2>/dev/null || slice_kills)
while :; do
  now=$(date '+%Y-%m-%d %H:%M:%S')
  report=
  for f in "$job"/*/result.json; do
    [ -f "$f" ] || continue
    trial=$(basename "$(dirname "$f")")
    grep -qx "$trial" "$state/seen" && continue
    line=$(python3 -I - "$f" <<'EOF'
import json, re, sys
r = json.load(open(sys.argv[1]))
if not r.get("finished_at"):
    sys.exit(3)
e = r.get("exception_info") or {}
kind = e.get("exception_type") or ""
msg = (e.get("exception_message") or "").replace("\n", " ")
# Model outcomes: a wrong answer, an agent timeout, or the client ending its own turn on a model error. Harbor
# relabels an agent exit by regex over the whole output (NetworkConnectionError on "Connection refused" from a
# mirror the model tried), so any exit of the agent command counts as the model's unless it names our server.
# Exit 137 is a SIGKILL: a container OOM shows up in the slice kill count below; otherwise the model killed it.
# The agent must have started (its first output line lists its tools); a usage error before that is ours.
started = '"type":"available_commands"' in msg
agent_exit = re.match(r'Command failed \(exit \d+\): mkdir -p /logs/agent && (CORTEX_API_KEY="\$\(cat [^)]*\)" )?cortex', msg) is not None and started and "llm.imabee.com" not in msg
if kind in ("", "AgentTimeoutError") or (kind.endswith("Error") and agent_exit):
    sys.exit(0)
print(f"{kind}: {msg[:300]}")
EOF
    )
    rc=$?
    [ $rc = 3 ] && continue
    echo "$trial" >> "$state/seen"
    [ -n "$line" ] && report+="trial $trial: $line"$'\n'
  done
  # 429 is backpressure the client handles and 499 a client that hung up (side calls cancelled at session end).
  # A 400 is cortex-api refusing an image the model cannot read (the client strips it and retries) only when an
  # agent log records an `invalid_image` 400 in the same window; any other 400 is reported. Agent logs are UTC.
  inference=$(journalctl -u cortex-api --since "$since" --until "$now" --no-pager -o cat 2>/dev/null \
    | grep -E 'path=/v1/(chat/completions|responses|messages) status=[45][0-9][0-9]' | grep -vE 'status=(429|499)')
  api=$(grep -v 'status=400' <<<"$inference" | grep -oE 'status=[0-9]+' | sort | uniq -c | tr '\n' ' ')
  rejected=$(grep -c 'status=400' <<<"$inference")
  if [ "$rejected" -gt 0 ]; then
    from=$(date -u -d "@$(date -d "$since" +%s)" +%Y-%m-%dT%H:%M:%S); to=$(date -u -d "@$(date -d "$now" +%s)" +%Y-%m-%dT%H:%M:%S)
    images=$( { for c in $(docker ps -q); do docker exec "$c" cat /logs/agent/cortex.log 2>/dev/null; done
                find "$job" -name cortex.log -mmin -10 -exec cat {} + ; } \
      | grep 'status_code=400' | grep '"code":"invalid_image"' \
      | awk -v a="$from" -v b="$to" '{t=substr($1,1,19)} t>=a && t<=b' | wc -l)
    [ "$rejected" -gt "$images" ] && api+="$((rejected - images)) status=400 (not image rejections) "
  fi
  [ -n "$api" ] && report+="cortex-api inference errors: $api"$'\n'
  llama=$(journalctl -u cortex-llama --since "$since" --until "$now" --no-pager -o cat 2>/dev/null \
    | grep -oE 'got exception: .{0,160}' | sort | uniq -c | head -3)
  [ -n "$llama" ] && report+="llama-server: $llama"$'\n'
  # One agent at a time is what keeps timings fair; count the harness processes inside task containers.
  # Setup also runs short `cortex --version` / `cortex models`; only `cortex --single` is an agent.
  agents=$(pgrep -cf '^cortex --single[ =]' || true)
  [ "${agents:-0}" -gt 1 ] && report+="$agents agents running at once"$'\n'
  now_kills=$(slice_kills)
  [ "$now_kills" -gt "$kills" ] && report+="a container was killed by the 22 GB benchmark memory ceiling"$'\n'
  kills=$now_kills
  avail=$(awk '/MemAvailable/ {print int($2/1048576)}' /proc/meminfo)
  [ "$avail" -lt 6 ] && report+="host memory available is down to ${avail} GB"$'\n'
  disk=$(df --output=avail -BG / | tail -1 | tr -dc 0-9)
  [ "$disk" -lt 15 ] && report+="disk free is down to ${disk} GB"$'\n'
  since=$now
  echo "$since" > "$state/since"
  echo "$kills" > "$state/slice_kills"
  if [ -n "$report" ]; then
    printf '%s' "$report"
    exit 1
  fi
  pgrep -f "harbor run" > /dev/null || { echo "harbor exited"; exit 0; }
  sleep 60
done
