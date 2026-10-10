<div align="center">

<img alt="Cortex" src="https://llm.imabee.com/v1/website/cortex-symbol-black-transparent-6435cf42.png" width="96">

# Cortex

**A terminal coding agent that runs on your own model.**
Full-screen TUI, headless mode for scripts and CI, and ACP for editors, all talking to a
single self-hosted gateway in front of [llama.cpp](https://github.com/ggml-org/llama.cpp).

[![release](https://img.shields.io/github/v/release/imabee101/cortex?style=flat-square&color=2f6f4f)](https://github.com/imabee101/cortex/releases/latest)
[![ci](https://img.shields.io/github/actions/workflow/status/imabee101/cortex/ci.yml?branch=main&style=flat-square&label=ci)](https://github.com/imabee101/cortex/actions/workflows/ci.yml)
[![license](https://img.shields.io/badge/license-Apache--2.0-2f6f4f?style=flat-square)](LICENSE)
![platforms](https://img.shields.io/badge/linux%20%C2%B7%20macOS%20%C2%B7%20windows-x86__64%20%2F%20arm64-444?style=flat-square)

[Install](#install) · [How it fits together](#how-it-fits-together) · [Releases](#releases) · [Build](#build-from-source) · [Docs](#documentation) · [License](#license)

![Cortex TUI](https://llm.imabee.com/v1/website/universe-tui-screenshot-6f7a0837.png)

</div>

## Install

```sh
curl -fsSL https://llm.imabee.com/cli/install.sh | bash
```

One command on every platform: macOS, Linux, WSL, and Windows in Git Bash (it installs `cortex.exe`). Builds: Linux x86_64 and aarch64, macOS Apple silicon, Windows x86_64. `cortex update` upgrades in place from the same host. The first launch opens your browser to sign in.

## How it fits together

```mermaid
flowchart LR
  C["cortex<br/>TUI / headless / ACP"] -- "TLS 443" --> E["llm.imabee.com<br/>Caddy"]
  E --> A["cortex-api<br/>auth, control plane, inference, search"]
  A --> L["llama-server<br/>--jinja"]
  A --> P[("PostgreSQL")]
  A -. "web search only" .-> B["Brave Search"]
```

- **One host.** The client contacts `llm.imabee.com` and nothing else. No telemetry leaves it.
- **`cortex-api`** (Rust, [`cortex-api/`](cortex-api)) is the server the client was built for: OIDC login,
  models and settings, `chat_completions` / `responses` / `messages` streaming, memory embeddings,
  server-side web search, session sync, and the update feed.
- **The client is a rename and nothing more.** All translation lives in the server, so the harness
  runs unmodified.

## Releases

One workflow, [`ci.yml`](.github/workflows/ci.yml), driven by the event. Cheap checks gate the expensive ones, and each release binary is compiled once.

```mermaid
flowchart LR
  P(["pull request"]) --> G["gates<br/>fmt · clippy · audit · brand"] --> T["tests<br/>nextest · coverage"]
  M(["merge to main"]) --> G
  T -- "main, Rust changed" --> B["build x4<br/>once per platform"] --> R["publish<br/>pre-release"]
  R --> D["deploy<br/>update feed"] --> PR["promote<br/>stable tag + latest"]
```

- Every build is `<version>-build.<run number>` in its tag, release and artifact names. The binary reports the plain `<version>` and its commit (`cortex 1.0.48 (0fcd6607943e)`), because the updater's stable channel refuses pre-release versions. The CI picks the version from tags (patch by default, a `minor` or `major` PR label raises it); no file is edited.
- Windows is cross-built on Linux, macOS and ARM Linux are native. Changes that touch no Rust path build and publish nothing, so build numbers can skip.
- Every merge that changes Rust ships with no manual step: the same verified bytes go to the feed `cortex update` reads, the previous client must update itself to them or the host rolls back, and only then does CI tag `vX.Y.Z` and mark the release latest.
- Assets carry `SHA256SUMS` and a build-provenance attestation: `gh attestation verify <file> --repo imabee101/cortex`.

Operations (deploy host, limits, backups, rollback) are in [`RUNBOOK.md`](RUNBOOK.md). How harness and model changes are measured (Terminal-Bench 2.0) is in [`EVALUATION.md`](EVALUATION.md). The per-model levers for tuning a model are in [`TUNING.md`](TUNING.md).

## Build from source

Rust is pinned by [`rust-toolchain.toml`](rust-toolchain.toml). Proto codegen needs `protoc`
(`$PROTOC`, a `protoc` on `PATH`, or [DotSlash](https://dotslash-cli.com) for [`bin/protoc`](bin/protoc)).

```sh
cargo run -p cortex-pager-bin                 # build and launch the TUI
cargo build -p cortex-pager-bin --release     # target/release/cortex-pager (shipped as `cortex`)
cargo test -p cortex-api --locked             # server contract tests (needs PostgreSQL on 127.0.0.1)
```

Target specific crates; full-workspace builds are slow. The root `Cargo.toml` is generated: edit per-crate manifests.

## Documentation

The user guide ships with the TUI crate:
[`crates/codegen/cortex-pager/docs/user-guide/`](crates/codegen/cortex-pager/docs/user-guide/)
(shortcuts, slash commands, configuration, MCP, skills, hooks, headless mode, sandboxing).

External contributions are not accepted ([`CONTRIBUTING.md`](CONTRIBUTING.md)); security reports go through [`SECURITY.md`](SECURITY.md).

## License

Apache License 2.0, see [`LICENSE`](LICENSE). Third-party and vendored code keeps its own licenses:
[`THIRD-PARTY-NOTICES`](THIRD-PARTY-NOTICES), [`third_party/NOTICE`](third_party/NOTICE), and the crate-local
[`notices`](crates/codegen/cortex-tools/THIRD_PARTY_NOTICES.md) for the ported tool code.
`SOURCE_REV` records the upstream commit this tree derives from.
