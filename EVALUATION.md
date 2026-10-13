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

## Running Terminal-Bench

The tooling is in [`eval/tbench`](eval/tbench); its working data (Harbor, tasks, binaries, jobs) lives in `$CORTEX_TBENCH_HOME`, default `~/.cache/cortex-tbench`, and the API key in `~/.config/cortex-test/tbench.env`.

1. `setup.sh` installs Harbor 0.23.0, fetches the tasks (their hash goes to `dataset.sha256`) and fills the uv cache one grader needs.
2. `build.sh <commit> <version>` builds the client at a commit in Debian 11, as old as the oldest task image.
3. `baseline.sh <label> <binary> [task ...]` runs the tasks (vision tasks excluded) through the adapter `cortex_tbench`, which uploads the binary and the server's CA into each container and stops the agent, with everything it started, when its time is up. `VERIFIER_UV_CACHE=$CORTEX_TBENCH_HOME/uv-cache/cache` hands that cache to the grader; it is used only for pytorch-model-recovery.
4. `watch.sh <job dir>` exits with the reason as soon as a trial fails for a cause that is ours, not the model's. Such a trial is rerun.

## Changing the wording

A change to the system prompt or a tool description is written and measured on the decision it targets before it reaches Terminal-Bench. A single run per task cannot show it: two runs differing only by three sentences disagreed on 7 of 10 previously passing tasks, each time for a reason the traces tie to sampling, not to the sentences.

**Writing it**

- State how the system behaves and what to do, with the reason; put the how where the decision is made (a tool call's how goes in that tool's description).
- Narrow or replace an existing sentence the change contradicts; never append a caveat to it.
- Plain tone, matching the surrounding text: no capitals, MUST, CRITICAL or ALWAYS.
- Leave the model's verification and done-claim rules alone unless they are the change under test.
- Nothing from a task: no task names, phrasing, files or examples.

**Measuring it with [`eval/replay.py`](eval/replay.py)**

1. **Decision points.** From saved Terminal-Bench sessions, list every point where the model made the targeted decision, wrong and right, as the session directory and the history index before its reply. Set about a third aside before writing the wording.
2. **Controls.** Add points from neighbouring decisions the new text could change, to catch overcorrection.
3. **Scoring rule.** Write it before replaying, as a mechanical check on the first tool call.
4. **Replay.** About 20 samples per point for each variant, with no benchmark running. Only the substituted text differs; the request is rebuilt as the client sends it.
5. **Keep it only if** the held-out points improve clearly and the controls do not move. Then confirm on Terminal-Bench with repeats on the target tasks and the previous passes.

Replay holds the history before the decision fixed, so it measures that decision alone; effects later in a task show only in the benchmark. Take points only from sessions run with the unchanged prompt, whose tool set never changed (a session saves only its last tool definitions) and that set no sampling overrides (replay sends none); cut indices count the history as saved, after any compaction. Unit tests: `python3 -m unittest discover -s eval`.

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
