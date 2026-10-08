#!/usr/bin/env bash
# Install or update the Cortex deployment on this host. Run as root:
#   sudo deploy/install.sh <cortex-api release binary>
# Host provisioning. Releases after the first come from the CI workflow, not from here. Idempotent. Replaces llm.service (llama-server on the public address) with
# postgres + llama-server (generation and embeddings) on loopback + cortex-api + the TLS edge.
set -euo pipefail
BIN=${1:?path to the cortex-api release binary}
HERE=$(cd "$(dirname "$0")" && pwd)
[[ $EUID -eq 0 ]] || { echo "run as root" >&2; exit 1; }

install -d -m 0755 /opt/cortex /opt/cortex/releases /opt/cortex/deploy /etc/cortex
install -d -m 0750 -o imma -g imma /var/backups/cortex
rel=/opt/cortex/releases/$(date -u +%Y%m%dT%H%M%SZ)
install -d "$rel"
install -m 0755 "$BIN" "$rel/cortex-api"
ln -sfn "$rel" /opt/cortex/current
# Keep the newest five releases for rollback.
find /opt/cortex/releases -mindepth 1 -maxdepth 1 -type d -printf '%T@ %p\n' | sort -rn | tail -n +6 | cut -d' ' -f2- | xargs -r rm -rf

# What the release workflow publishes: /opt/cortex/dist is the directory cortex-api serves under /cli
# (CORTEX_API_DIST_DIR). The CI runner's user owns it and /opt/cortex/releases and may do two privileged
# things only: repoint /opt/cortex/current and restart cortex-api.
id cortex-deploy >/dev/null 2>&1 || useradd --system --create-home --home-dir /var/lib/cortex-deploy --shell /usr/bin/nologin cortex-deploy
install -d -m 0755 -o cortex-deploy -g cortex-deploy /opt/cortex/dist /opt/cortex/dist/changelogs
chown cortex-deploy:cortex-deploy /opt/cortex/releases
SCRIPTS=$(cd "$HERE/../crates/codegen/cortex-pager/scripts" && pwd)
install -m 0644 -o cortex-deploy -g cortex-deploy "$SCRIPTS"/install.sh "$SCRIPTS"/install.ps1 "$SCRIPTS"/install-enterprise.sh "$SCRIPTS"/install-enterprise.ps1 /opt/cortex/dist/
install -m 0644 -o cortex-deploy -g cortex-deploy "$HERE"/changelogs/* /opt/cortex/dist/changelogs/
install -m 0440 "$HERE/cortex-deploy.sudoers" /etc/sudoers.d/cortex-deploy
visudo -cf /etc/sudoers.d/cortex-deploy >/dev/null

# Embedding model, pinned by hash (cortex-embed.service refuses any other file).
EMBED_SHA=06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439
EMBED=/opt/cortex/models/Qwen3-Embedding-0.6B-Q8_0.gguf
install -d -m 0755 /opt/cortex/models
if ! echo "$EMBED_SHA  $EMBED" | sha256sum -c --quiet - 2>/dev/null; then
  curl -fL --retry 3 -o "$EMBED.part" https://huggingface.co/Qwen/Qwen3-Embedding-0.6B-GGUF/resolve/main/Qwen3-Embedding-0.6B-Q8_0.gguf
  echo "$EMBED_SHA  $EMBED.part" | sha256sum -c --quiet - || { rm -f "$EMBED.part"; echo "embedding model hash mismatch" >&2; exit 1; }
  mv "$EMBED.part" "$EMBED"
fi

install -m 0755 "$HERE/backup.sh" "$HERE/restore-test.sh" /opt/cortex/deploy/
install -m 0644 "$HERE/Caddyfile" /etc/cortex/Caddyfile

pacman -Q caddy >/dev/null 2>&1 || pacman -S --noconfirm --needed caddy
for unit in cortex-postgres cortex-llama cortex-embed cortex-api cortex-edge cortex-backup; do
  install -m 0644 "$HERE/$unit.service" /etc/systemd/system/
done
install -m 0644 "$HERE/cortex-backup.timer" /etc/systemd/system/
install -d /etc/systemd/system/llm-renew.service.d
install -m 0644 "$HERE/llm-renew.service.d/cortex-edge.conf" /etc/systemd/system/llm-renew.service.d/
systemctl daemon-reload

# Secrets are provisioned, never installed from the repo: the Brave token goes in
# /etc/cortex/cortex-api.env as CORTEX_API_BRAVE_TOKEN=..., root:imma 0640.
[[ -f /etc/cortex/cortex-api.env ]] || echo "note: /etc/cortex/cortex-api.env is missing; web search stays off until it exists" >&2

# The old public llama-server holds [::7]:443; the edge needs it.
systemctl disable --now llm.service 2>/dev/null || true
systemctl enable --now cortex-postgres.service cortex-llama.service cortex-embed.service
systemctl enable --now cortex-api.service
# An update is a new binary under /opt/cortex/current; restart picks it up after a drain.
systemctl restart cortex-api.service
systemctl enable --now cortex-edge.service
systemctl enable --now cortex-backup.timer
systemctl --no-pager --lines=0 status cortex-postgres cortex-llama cortex-embed cortex-api cortex-edge cortex-backup.timer | grep -E '●|Active:' || true
