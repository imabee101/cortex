# Cortex

Bring Cortex into your terminal. Fast, flicker-free CLI built for plans, subagents, and parallel work.

**[Homepage](https://llm.imabee.com/cli)** | **[Documentation](https://llm.imabee.com/build/overview)**

## Install

```bash
curl -fsSL https://llm.imabee.com/cli/install.sh | bash
```

Or install with npm:

```bash
npm i -g @cortex-official/cortex
```

## Get Started

```bash
# Launch the interactive TUI
cortex

# Run a single task
cortex -p "Explain this codebase"
```

On first launch, Cortex opens your browser to authenticate. For CI or headless environments, use an API key from [llm.imabee.com](https://llm.imabee.com):

```bash
export CORTEX_API_KEY="cortex-..."
```

## Update

```bash
cortex update
```

Or if installed via npm:

```bash
npm i -g @cortex-official/cortex@latest
```

## Supported Platforms

| Platform | Architecture |
|---|---|
| macOS | Apple Silicon (arm64) |
| Linux | x86_64, arm64 |
| Windows | x86_64 |

## Documentation

For full documentation including configuration, MCP servers, custom models, headless mode, agent mode, and more, visit [llm.imabee.com/build/overview](https://llm.imabee.com/build/overview).

## Feedback

Run `/feedback` inside Cortex to report issues or send feedback directly.
