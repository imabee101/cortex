# Cortex runbook

`https://llm.imabee.com` (TLS 1.2+, port 443) is the only public surface. Everything else binds loopback.

| Unit | Role | Bind |
|---|---|---|
| `cortex-edge` | Caddy: TLS, HSTS, 32 MB body cap, 10 min idle/read, no response buffering, `/metrics` hidden | the host's public IPv6 address, port 443, LAN IPv4 reaches it through the `llm-v4` socket unit |
| `cortex-api` | OIDC issuer, control plane, inference gateway, search, sync, WebSocket relay | `127.0.0.1:8787` |
| `cortex-llama` | `llama-server --jinja`, bearer token from `/etc/llm/api-key` | `127.0.0.1:8080` |
| `cortex-embed` | second `llama-server --embeddings` on the CPU (the GPU is full), same bearer token | `127.0.0.1:8081` |
| `cortex-postgres` | accounts, sessions, quotas, telemetry, signing keys | `127.0.0.1:5432` |
| `cortex-backup.timer` | nightly `pg_dump -Fc` to `/var/backups/cortex`, newest 14 kept | |
| `llm-renew.timer` | reissues the leaf from Vault when under 30 days remain, then reloads `cortex-edge` | |

Outbound from cortex-api: `api.search.brave.com:443` only, plus the host resolver.

## Pins

- Rust `1.94.0` (`rust-toolchain.toml`); dependencies by committed `Cargo.lock`; build with `--locked`.
- llama.cpp `99b9548` (version `0.5.0-dev`, build 1), at `/home/imma/projects/llm/.opt/llama.cpp/bin/llama-server`.
- Model profile `qwen3.5-9b-uncensored`, set in `cortex-llama.service`: `Qwen3.5-9B-Uncensored-BF16.gguf`, 18407321440 bytes, sha256 `d41cb54685118b4bfb00189c42154d7c2a14fbc6193a1bccf97f056812e79b71`, alias `Qwen3.5-9B-Uncensored` (also `CORTEX_API_MODEL` in `cortex-api.service`); `serve.sh` refuses a file that does not match. Changing the model means changing both units.
- Embedding model `Qwen3-Embedding-0.6B-Q8_0.gguf` at `/opt/cortex/models`, sha256 `06507c7b42688469c4e7298b0a1e16deff06caf291cf0a5b278c308249c3e439`; `cortex-embed.service` refuses any other file. Vectors are 1024-dimensional (`CORTEX_API_EMBED_DIMENSIONS`).
- `llm.service` (the llm project's public llama-server) must stay disabled: it binds the same address on 443, which the edge owns, and the GPU the generation model needs.
- PostgreSQL 18.6, Caddy 2.11.4.

## Release and deploy

One workflow, `.github/workflows/ci.yml`, picks its behaviour from the event. Pull requests and merges run `gates` (fmt, clippy, audit, brand and secret scan, changed paths) then `tests` (nextest with coverage); `ci-ok` is the single required check and native auto-merge waits on it. A merge to `main` that changed Rust paths then builds the four platforms once (Windows is cross-built on Linux with `cargo-xwin`; its proto helper is Unix-only, so it cannot build or test natively), and `publish` tags `v<version>-build.<run number>` on that commit and creates a pre-release with the artifacts, `SHA256SUMS` and a provenance attestation. The binary is stamped with the plain `<version>` (`--version` adds the commit), never the `-build.N` id: the updater's stable channel rejects pre-release versions, so the feed serves the same bytes as `cortex-<version>-<platform>` with `<version>` in `stable`. A merge that changed no Rust path builds and publishes nothing, so build numbers can skip. The version is computed from tags in the run (newest `vX.Y.Z`, patch bump, `minor`/`major` label on the merged PR raises it); nothing is committed.

Every published build then ships with no manual step: `deploy` runs on the self-hosted runner (label `cortex-prod`, user `cortex-deploy`, `production` environment limited to `main`): it checks all four platforms against `SHA256SUMS`, copies them into `/opt/cortex/dist` as `.part` then renames, installs `cortex-api` under `/opt/cortex/releases/<utc stamp>`, repoints `/opt/cortex/current` and restarts the service (open streams drain for up to `CORTEX_API_DRAIN_SECS`, 900; the unit allows 930 s), then writes `/opt/cortex/dist/stable` last. It verifies through `https://llm.imabee.com/cli`: served sizes, byte ranges, changelog, install scripts, and the previous build running `cortex update` to the new one. Any failure restores the previous pointer and binary, and nothing is promoted; re-run the failed job once the cause is fixed. After a good deploy, `promote` tags `v<version>` on the commit and republishes the same bytes as the latest release, so the next build's version starts from it. `cortex-deploy`'s only sudo rights are in `deploy/cortex-deploy.sudoers`. Nothing is released from a workstation.

`sudo deploy/install.sh <cortex-api binary>` provisions the host once (units, Postgres, Caddy, the `cortex-deploy` user and its sudoers file, install scripts); it does not publish clients. The runner lives in `/var/lib/cortex-deploy/runner` (unpacked from the actions/runner release, hash-checked) and runs as `deploy/cortex-runner.service`. To register or replace it: `sudo -u cortex-deploy ./config.sh --unattended --url https://github.com/imabee101/cortex --token <token> --name cortex-prod-1 --labels cortex-prod --replace`, with a token from `gh api -X POST repos/imabee101/cortex/actions/runners/registration-token --jq .token`, then `systemctl enable --now cortex-runner`. Only `main` reaches it; fork code never does.

Migrations are versioned (`schema_migrations`), applied under an advisory lock at start, and only add tables.

Clients install and update from the same host: `curl -fsSL https://llm.imabee.com/cli/install.sh | bash` puts the build in `~/.cortex/downloads`, links `~/.cortex/bin/{cortex,cortex-agent}`, edits the shell rc `PATH`, and `cortex update` repeats it. `/cli/stable` reports the contents of `<dist>/stable`, falling back to `CORTEX_API_RELEASE_VERSION` when that file is absent. Only exact install-script names, `cortex-<version>-<platform>[.exe][.zst|.gz]` and `changelogs/<version>.external.{md,json}` are served from `CORTEX_API_DIST_DIR` (default `/opt/cortex/dist`). Platforms: `linux-x86_64`, `linux-aarch64`, `macos-aarch64`, `windows-x86_64`.

Web search is off until `/etc/cortex/cortex-api.env` (root:imma 0640) holds `CORTEX_API_BRAVE_TOKEN=...`. Put it there with `secd`, never by hand in a shell history, then `systemctl restart cortex-api`. With the token the settings response advertises `web_search_model`; without it, it does not.

Limits in `cortex-api.service` override the code defaults: queue 256 deep and 540 s long (the edge allows 10 min), 6000 requests a minute per user and per address, 1,000,000 inference and 20,000 search requests a day. Login and registration limits stay strict. Every request is capped at `CORTEX_API_MAX_OUTPUT_TOKENS` (24576, about 8 minutes at the model's speed, so under the 540 s queue wait; 0 turns it off). A stream whose last 8 KB of answer or reasoning text is almost all repeats is cut and counted in `cortex_api_loops_aborted_total`; the client sees a broken stream and retries with a fresh sample. A generation that spends the whole output cap on reasoning alone (finish `length`, no answer, no tool call) is cut the same way and counted in `cortex_api_reasoning_overruns_total`, because the harness does not retry a `length` finish but does retry a broken stream; a `length` finish after an answer or tool call, or under a limit the client set itself, passes through. Tool-call arguments are never watched because one generation that never stops, such as a model repeating itself on a request with no limit, holds the only slot. A 429 on inference means the single llama slot stayed busy longer than the queue wait: the harness gives up after two in a row, so find the long generation first: `/slots` on llama (`n_decoded`, `n_remain`) and `cortex_api_inference_in_flight` show who holds the slot, and `systemctl kill -s KILL cortex-api` releases it at once (a plain restart drains it for up to 15 minutes). Then raise the wait or the slot count (GPU memory permitting), not the rate limits.

`/v1/models` reports `CORTEX_API_CONTEXT_PERCENT` (40) of the real window on purpose: the harness counts a conversation as bytes/4 and never reads the server's token count, which runs about twice that on code, so compaction at 85% of the real window comes after llama-server has already refused the request. Raise it only after measuring the ratio on real sessions.

Knobs (environment, defaults in `cortex-api/src/main.rs`): `CORTEX_API_EMBED`, `_EMBED_MODEL`, `_EMBED_DIMENSIONS` (memory embeddings are advertised in settings only when the first two are set), `_DIST_DIR`, `CORTEX_API_PARALLEL` (0 reads slots from llama), `_QUEUE_MAX` 8, `_QUEUE_WAIT_SECS` 30, `_IP_RATE_PER_MIN`, `_USER_RATE_PER_MIN`, `_KEY_LIMIT` 25 (live API keys per account), `_INFERENCE_PER_MIN`, `_DAILY_INFERENCE_QUOTA`, `_DAILY_SEARCH_QUOTA`, `_TELEMETRY_RETENTION_DAYS`.

## Check

- `curl -s https://llm.imabee.com/healthz` and `/readyz` return 200; `/readyz` needs Postgres.
- `curl -s http://127.0.0.1:8787/metrics`: requests, throttled, slots, in-flight, waiting, rejected, breaker flags.
- `journalctl -u cortex-api`: one line per request (method, path, status, request id, elapsed) plus `WARN` lines naming an upstream and its error class. A line with prompt or completion text is a defect.
- A saturated gateway answers 429 with `Retry-After`; an open breaker answers 503 with `Retry-After`. The client retries both.

## Rollback

1. Binary and pointer: `sudo ln -sfn /opt/cortex/releases/<previous> /opt/cortex/current && sudo systemctl restart cortex-api`, and write the previous version into `/opt/cortex/dist/stable`. Signing keys live in Postgres, so issued tokens keep verifying.
2. Data: `restore-test.sh` shows the procedure; to roll back for real, stop cortex-api, `pg_restore --clean --if-exists -d cortex /var/backups/cortex/cortex.dump`, start it.
3. Done when `/readyz` is 200 and a signed-in `GET /v1/models` is 200.

## Backup and restore

`systemctl start cortex-backup` takes a dump now. `/opt/cortex/deploy/restore-test.sh` restores the newest into `cortex_restore_test`, compares every table's row count with the source, and drops the scratch database.

## Certificate

`systemctl start llm-renew` is safe at any time: it prints `valid beyond 30d, nothing to do` unless the leaf is close to expiry. A real reissue replaces `/etc/llm/tls/{fullchain,key}.pem` and reloads `cortex-edge` without dropping open streams.
