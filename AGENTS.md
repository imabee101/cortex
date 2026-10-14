# Cortex

Fork of the Grok harness. cortex-api is our server and implements what the harness expects, the way Grok's API would, under our name. This file is the only project instruction. The README is the overview.

llama.cpp, cortex-api, and the harness have to agree. The API bends to the harness and translates between them. No shims, bolt-ons, or hardcoding. A request we cannot serve, or have no infrastructure for, is rejected by the API. Do not edit the harness to hide that.

Edit the harness only to rebrand it or point it at us. Any other harness change needs a direct yes, discussed first. A nearby sentence is not a yes. It also needs evidence the original authors would have made that change to fit a 9B model as a first-class citizen. Suggest it only as a last resort.

Make the trio stable, then take a baseline, then improve the 9B frontier experience by optimizing prompts. Do not ship an unreliable change, or one whose complexity can break the trio.

## Layout

- Harness: `crates/codegen`. Binary crate `cortex-pager-bin`, shipped as `cortex`. Client state is `~/.cortex`.
- API: `cortex-api`. The schemas it implements are `prod/mc/cli-chat-proxy-types`.
- llama-server: `/home/imma/projects/llm/.opt/llama.cpp/bin/llama-server`. Unit `cortex-llama` on `127.0.0.1:8080`. Embeddings unit `cortex-embed` on `127.0.0.1:8081`.
- Public host `https://llm.imabee.com` (`cortex-edge`). cortex-api on `127.0.0.1:8787`. Postgres on `127.0.0.1:5432`.

## Build and test

This machine: 62 GB RAM, no swap, one RTX 4090. Uncapped builds have taken the desktop down. One build or test at a time, never during a benchmark. Budget: two llama-servers ~14 GB, containers 22 GB (`tbench.slice`, MemoryMax=22G), one build 12 GB, desktop ~14 GB. MGLRU `min_ttl_ms` stays 0.

`systemd-run --user --scope -p MemoryMax=12G -p MemorySwapMax=0 nice cargo <cmd> -j 8 ...`

Rust is pinned in `rust-toolchain.toml`. Pass `--locked`. `protoc` is `bin/protoc` or `$PROTOC`. The root `Cargo.toml` is generated; edit the crate manifest. Build one crate, not the workspace.

```sh
cargo fmt -p cortex-api --check
cargo clippy -p cortex-api --all-targets --locked -- -D warnings
cargo test -p cortex-api --locked
cargo check -p cortex-pager-bin --locked
```

API tests need Postgres on `127.0.0.1`. CI also creates `cortex_api_infer` and `cortex_api_phase4`. Harness crates are compile-checked, not linted.

Run the TUI from source with `cargo run -p cortex-pager-bin`. `cargo build -p cortex-pager-bin --release` writes `target/release/cortex-pager`.

## Live

`curl -s https://llm.imabee.com/healthz` and `/readyz` return 200. `/readyz` needs Postgres. `journalctl -u cortex-api` is one line per request. A line with prompt or completion text is a defect.

Web search is off until `/etc/cortex/cortex-api.env` holds `CORTEX_API_BRAVE_TOKEN`. Put it there with `secd`, then `systemctl restart cortex-api`. With the token, settings advertise `web_search_model`.

A merge to `main` builds, deploys, and promotes. Do not release from a workstation. Roll back with `sudo ln -sfn /opt/cortex/releases/<previous> /opt/cortex/current && sudo systemctl restart cortex-api`, and write that version into `/opt/cortex/dist/stable`.

## Baseline

Not until the trio is stable. Do not delete scripts or evals. Data is `~/data/cortex-tbench`. `eval/tbench/baseline.sh <label> <binary> [task ...]` runs it and `eval/tbench/watch.sh <job dir>` exits on a fault of ours. One agent on the model. Excluded: chess-best-move, code-from-image, extract-moves-from-video, path-tracing.
