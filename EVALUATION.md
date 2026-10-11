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
- **Server levers first.** The per-model levers the harness was built with are listed in [`TUNING.md`](TUNING.md); try them before any client change.
- **Wording before mechanism.** Fix a defect in the instructions first. Mechanical harness changes (trimming tools, per-turn reminders, new runtime steps) come only after wording has been measured and failed.

## Changing the harness

The client is the upstream harness renamed, plus one kind of change: compensating for a weakness of the deployed model the way the upstream authors would have, had they shipped that model. Such a change is made only when all of these hold:

- **A measured weakness.** It targets a failure class seen in Terminal-Bench runs, citing the failing trials. A hunch, one transcript, or a preferred design is not enough.
- **The authors' mechanism.** It extends a mechanism upstream already uses to adapt to a model: a field on the model entry (as `laziness_detector` and `use_concise` are), a built-in agent preset, a prompt template, or a `<system-reminder>`. It follows that mechanism's structure, naming, tests and docs. A new mechanism needs a stated reason none of these fit.
- **Scoped to the model.** It is selected per model through the model entry and off by default, so every other model behaves exactly as upstream.
- **General, not fitted.** It addresses the failure class, never a task: no benchmark task names, file names, expected outputs, or hardcoded model ids.
- **Upstream quality.** It would pass the authors' review: every error path handled, tests on the real code, user-guide docs updated in the same commit.
- **It wins.** Measured before and after under the rules above, with the gain outside noise and no task category worse.

Never: shims, wrappers, proxies, preloads or patched dependencies on the client; rewriting or repairing model output to hide a failure; retries or loops that mask one; a flag kept to switch off a change that lost. A change that loses is removed.

The same bar holds for configuration and API: a value `cortex-api` sends in the model entry, or a new field in any response, ships only with a measured win, in the existing field conventions, and is recorded in [`TUNING.md`](TUNING.md).
