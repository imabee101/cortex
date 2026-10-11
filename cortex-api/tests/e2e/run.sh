#!/usr/bin/env bash
# Real client, real server, real model. The published `cortex` binary signs in with an API key at
# https://llm.imabee.com over TLS (the production edge config, a CA trusted through the OS store,
# verification on) and completes inference through this build of cortex-api and a llama-server.
# The caller maps llm.imabee.com to 127.0.0.1 and ::1; root or sudo is needed to trust the CA and
# bind 443.
set -euo pipefail
: "${CORTEX:?published cortex client}" "${CORTEX_API:?cortex-api binary}" "${LLAMA_SERVER:?llama-server binary}"
: "${MODEL:?GGUF path}" "${CADDY:?caddy binary}" "${DATABASE_URL:?postgres URL for a scratch database}"
here=$(cd "$(dirname "$0")" && pwd)
export E2E_DIR=${E2E_DIR:-$(mktemp -d)} E2E_API_PORT=${E2E_API_PORT:-18787}
llama_port=${LLAMA_PORT:-18080}
alias=e2e-model
SUDO=; [ "$(id -u)" = 0 ] || SUDO=sudo
pids=()
cleanup() { for p in "${pids[@]}"; do $SUDO kill "$p" 2>/dev/null || true; done; }
trap cleanup EXIT
fail() {
  echo "e2e: $*" >&2
  for f in "$E2E_DIR"/*.log; do [ -f "$f" ] && { echo "--- ${f##*/}"; tail -n 40 "$f"; }; done >&2
  exit 1
}
wait_for() { for _ in $(seq "$2"); do curl -sf -m 2 "$1" > /dev/null && return 0; sleep 1; done; fail "$1 never answered"; }

# Every address the name resolves to must be loopback, or the client could reach production instead.
addrs=$(getent ahosts llm.imabee.com | awk '{print $1}' | sort -u)
if [ -z "$addrs" ] || grep -qvxE '127\.0\.0\.1|::1' <<< "$addrs"; then
  fail "llm.imabee.com resolves to ${addrs//$'\n'/ }; map it to 127.0.0.1 and ::1"
fi

# Another server on these ports would answer in place of this run's and fail it misleadingly.
for port in 443 "$E2E_API_PORT" "$llama_port"; do
  ! (exec 3<> "/dev/tcp/127.0.0.1/$port") 2> /dev/null || fail "port $port is already in use"
done

# A CA for this run only, trusted through the OS store that both the client and Python read.
mkdir -p "$E2E_DIR/tls"
(
  cd "$E2E_DIR/tls"
  openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -days 1 -subj "/CN=cortex e2e CA" \
    -addext basicConstraints=critical,CA:TRUE -addext keyUsage=critical,keyCertSign -keyout ca.key -out ca.pem 2> /dev/null
  openssl req -newkey ec -pkeyopt ec_paramgen_curve:P-256 -nodes -subj "/CN=llm.imabee.com" -keyout leaf.key -out leaf.csr 2> /dev/null
  printf 'subjectAltName=DNS:llm.imabee.com\nextendedKeyUsage=serverAuth\n' > ext
  openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 1 -extfile ext -out leaf.pem 2> /dev/null
)
if command -v update-ca-certificates > /dev/null; then
  $SUDO install -m 0644 "$E2E_DIR/tls/ca.pem" /usr/local/share/ca-certificates/cortex-e2e.crt
  $SUDO update-ca-certificates > /dev/null
else
  $SUDO trust anchor --store "$E2E_DIR/tls/ca.pem"
fi

"$LLAMA_SERVER" -m "$MODEL" --alias "$alias" --jinja --host 127.0.0.1 --port "$llama_port" \
  --ctx-size 65536 --parallel 1 --no-kv-unified --temp 0.6 --top-p 0.95 --top-k 20 --min-p 0 \
  > "$E2E_DIR/llama.log" 2>&1 &
pids+=($!)
wait_for "http://127.0.0.1:$llama_port/health" 300

mkdir -p "$E2E_DIR/dist"
CORTEX_API_MODEL=$alias CORTEX_API_MAX_OUTPUT_TOKENS=512 CORTEX_API_DIST_DIR=$E2E_DIR/dist \
  "$CORTEX_API" --listen "127.0.0.1:$E2E_API_PORT" --llama "http://127.0.0.1:$llama_port" \
  --database-url "$DATABASE_URL" > "$E2E_DIR/cortex-api.log" 2>&1 &
pids+=($!)
wait_for "http://127.0.0.1:$E2E_API_PORT/readyz" 120

$SUDO env HOME="$E2E_DIR" XDG_DATA_HOME="$E2E_DIR/caddy" XDG_CONFIG_HOME="$E2E_DIR/caddy" \
  E2E_DIR="$E2E_DIR" E2E_API_PORT="$E2E_API_PORT" \
  "$CADDY" run --config "$here/Caddyfile" --adapter caddyfile > "$E2E_DIR/caddy.log" 2>&1 &
pids+=($!)
wait_for "https://llm.imabee.com/readyz" 60

python3 -I "$here/client.py" "$CORTEX" "$E2E_DIR" || fail "client checks failed"
echo "e2e: passed"
