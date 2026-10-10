# Tuning a model

How to compensate for a model's weaknesses the way the harness was built to: from the server, per model, with no client change. The client stays a pure rename (see the project rules), so this is the first place to look. Measure every change per [`EVALUATION.md`](EVALUATION.md).

## Where the levers live

- The client builds each model from the entry `cortex-api` returns at `GET /v1/models`, parsed in `crates/codegen/cortex-shell/src/remote/client.rs` (`parse_remote_model_value`). Fields are accepted in snake_case or camelCase; several are also read from a `_meta` object.
- `cortex-api` writes that entry in `model_list` (`cortex-api/src/control.rs`). Today it sends only `id`, `model`, `name`, `context_window`, `api_backend` and `supports_backend_search`, so every lever below is unused.
- A user's own `[model.<id>]` table in `config.toml` overrides the server entry field by field (`ConfigModelOverride::apply`, `crates/codegen/cortex-shell/src/agent/config.rs`). Measured runs must not carry one.

## Levers in the model entry

| Field | Effect | Typical use for a small model |
|---|---|---|
| `agent_type` (alias `system_prompt_type`) | Selects the built-in agent: its prompt and tool set. Values: `cortex-build`, `cortex-build-concise`, `cortex-build-plan` (default, `DEFAULT_AGENT_TYPE`), `cortex-build-plan-no-subagents`, `cortex-build-ask-user`, `cortex-build-orchestrator`, `codex`, `opencode` (`BuiltinAgentName`, `crates/codegen/cortex-agent/src/config.rs`). Tool sets: `native_toolset_presets` in the same file. | Fewer tools, fewer distractions; no subagents for a model that cannot delegate well |
| `use_concise` | Replaces the system prompt with the short `COMPACT_SYSTEM_PROMPT` (`crates/codegen/cortex-agent/src/prompt/template.rs`). | Less prompt load in a small context window |
| `laziness_detector` | `enabled`, `max_nudges_per_session`, `idle_threshold_ms`, `min_confidence`, `include_reasoning` (`crates/codegen/cortex-config-types/src/flags.rs`). A classifier detects a premature stop and injects a `<system-reminder>` nudge. `enabled` with `max_nudges_per_session = 0` only observes. It acts only while the goal harness is active (`crates/codegen/cortex-shell/src/session/acp_session_impl/laziness.rs`). | A model that quits or claims done too early |
| `temperature`, `top_p` | Sampling sent with each request. | Loops, repetition, latching onto one token path |
| `reasoning_effort`, `supports_reasoning_effort`, `reasoning_efforts`, `reasoning_summary` | Thinking budget and its display. | Overthinking or underthinking |
| `max_completion_tokens` | Output cap per request. | Runaway generations |
| `context_window`, `auto_compact_threshold_percent`, `compaction_at_tokens` | When compaction fires. | Long tasks in a small window |
| `stream_tool_calls`, `max_retries`, `inference_idle_timeout_secs` | Tool-call wire shape, retry count, stream idle limit. | Malformed tool calls, slow generations |
| `system_prompt_label` | The name the prompt gives the assistant. | Identity only |

## Other server-side levers

- `GET /v1/subagents/bundle` (`cortex-api/src/control.rs`, type `SubagentBundle` in `prod/mc/cli-chat-proxy-types`) delivers agent definitions; it is empty today.
- `supports_backend_search` turns on server-side web search through Brave. It is the built-in way to supply knowledge a small model lacks, and is on only when the Brave token is configured.

## Last resort

The system prompt template (`crates/codegen/cortex-agent/templates/prompt.md`, regenerated with `crates/codegen/cortex-agent/scripts/encrypt_templates.py`) and the tool descriptions are client code. Editing them breaks the rename-only rule, so it needs an explicit decision from the owner and a measured win.
