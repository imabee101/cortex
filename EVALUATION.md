# Evaluation

Decided 2026-10-10. This file records how Cortex measures changes to what the harness tells the model: the system prompt, tool descriptions, reminders, and compaction prompts.

## Benchmark

- **Terminal-Bench 2.0 is the benchmark for every harness and model change.** The agent under test is the Cortex harness itself: the `cortex` binary, with its own system prompt, tools, and compaction, driving the model through `cortex-api`. [Harbor](https://www.tbench.ai/docs/run-terminal-bench-2-0), the official runner, only provisions each task's container and grades it, with Cortex plugged in as a custom agent (`--agent-import-path`).
- The earlier hand-written cases (`L1`–`L13`, `R1`–`R4`) are retired once their last run finishes. Their results stay as history and are not extended.

## Rules for a measured run

- **Bare model, harness defaults.** Nothing may be imported into the run: no `CLAUDE.md`, no `AGENTS.md`, no other harness's rules, skills, settings, or plugins, and no user home config beyond sign-in. Every compat cell stays off. A run that loaded any of these is discarded.
- **One variable.** Before and after are two release builds of the same commit, differing only by the change under test, against the same model, server, and sampling.
- **One inference slot.** The local `llama-server` serves one request at a time, so trials run sequentially (concurrency 1).
- **Repeat trials.** The model runs without a seed, so each comparison repeats every task enough times to separate a change from noise; one passing run proves nothing.
- **A change stays only if it wins.** If after does not beat before, the change is not merged and the measured result is reported.
- **Wording before mechanism.** Fix a defect in the instructions first. Mechanical harness changes (trimming tools, per-turn reminders, new runtime steps) come only after wording has been measured and failed.
