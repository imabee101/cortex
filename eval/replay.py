"""Replays saved decision points against the served model with the original or a changed prompt.

Usage: replay.py <points.jsonl> <variants.json> <variant> <samples> [--server URL] [--max-seconds N] > results.jsonl

Each line of points.jsonl names a saved session directory (`session`, holding chat_history.jsonl and
tool_definitions.json) and `cut`, the index of the first history item the model generated at that point (its
reasoning, if any). Other fields are copied to the output. The request is rebuilt the way the client builds it
and sent through cortex-api like the client's, with no sampling overrides, so the deployed sampling applies:
- old tool results are trimmed as `prune_conversation` (cortex-chat-state request_builder.rs) trims them once the
  client's model-reported token total passes half the context window. A point where trimming would change the
  request must carry that total (`reported_tokens`, the `model_reported_total` the trial's cortex.log shows on
  the last history push before the cut) and the advertised `context_window`;
- each run of reasoning folds into the following assistant's `reasoning_content` (`conversation_to_chat_messages`
  in cortex-sampling-types).
Points must come from sessions run with memory off (the client then adds no memory reminder); images and
messages from another agent are refused. variants.json maps each variant name to exact-text substitutions for
the system prompt (`system`) and for tool descriptions (`tools`, keyed by tool name); each must match exactly once.
Every sample's first tool call and text are written as one JSON line; scoring is left to the experiment. A sample
still generating after --max-seconds is abandoned with finish "runaway", which frees the server's slot.
"""

import argparse
import http.client
import json
import os
import ssl
import sys
import time
import urllib.request
from pathlib import Path

DEFAULT_SERVER = "https://llm.imabee.com"
# `PruningConfig::default()` in cortex-chat-state, with its placeholder and separator.
KEEP_LAST_N_TURNS, SOFT_TRIM_THRESHOLD, SOFT_TRIM_HEAD, SOFT_TRIM_TAIL, HARD_CLEAR_AGE_TURNS = 3, 4000, 1500, 1500, 10
HARD_CLEAR_PLACEHOLDER = "[Tool result omitted \u2014 too old]"
SOFT_TRIM_SEPARATOR = "\n\n[\u2026trimmed\u2026]\n\n"


def substitute(text, pairs, what):
    for old, new in pairs:
        found = text.count(old)
        if found != 1:
            raise ValueError(f"{what}: expected one match for {old[:60]!r}, found {found}")
        text = text.replace(old, new)
    return text


def reasoning_text(item):
    parts = [p.get("text", "") for p in item.get("summary") or []]
    parts += [c.get("text", "") for c in item.get("content") or []]
    return "\n".join(parts)


def _reject_constant(name):
    raise ValueError(f"{name} is not JSON")


def tool_arguments(arguments):
    """The client replaces arguments that are not JSON with `{}` (`sanitize_tool_arguments`, serde: no NaN)."""
    try:
        json.loads(arguments, parse_constant=_reject_constant)
    except ValueError:
        return "{}"
    return arguments


def pruned(items):
    """`prune_conversation`: tool results more than KEEP_LAST_N_TURNS user items old are trimmed or cleared."""
    out, turn_from_end, seen_first_user = list(items), 0, False
    for i in range(len(out) - 1, -1, -1):
        item = out[i]
        if item["type"] == "user":
            if seen_first_user:
                turn_from_end += 1
            seen_first_user = True
            continue
        if item["type"] != "tool_result" or turn_from_end < KEEP_LAST_N_TURNS:
            continue
        content = item.get("content") or ""
        if turn_from_end >= HARD_CLEAR_AGE_TURNS:
            content = HARD_CLEAR_PLACEHOLDER
        elif len(content) > SOFT_TRIM_THRESHOLD:
            content = content[:SOFT_TRIM_HEAD] + SOFT_TRIM_SEPARATOR + content[-SOFT_TRIM_TAIL:]
        if content != (item.get("content") or ""):
            out[i] = dict(item, content=content)
    return out


def chat_messages(items, system):
    """Chat Completions messages for history items, with the system prompt replaced by `system`."""
    out, pending = [], []
    for item in items:
        kind = item["type"]
        if kind == "reasoning":
            text = reasoning_text(item)
            if text:
                pending.append(text)
            continue
        if kind == "system":
            message = {"role": "system", "content": system}
        elif kind == "user":
            if item.get("synthetic_reason") in ("agent_message", "parent_agent_message"):
                raise ValueError("messages from another agent are not replayed")
            content = item["content"]
            if isinstance(content, list):
                if any(part.get("type") != "text" for part in content):
                    raise ValueError("user content with images is not replayed")
                content = "\n".join(part["text"] for part in content)
            message = {"role": "user", "content": content}
        elif kind == "assistant":
            message = {"role": "assistant", "content": item.get("content") or ""}
            calls = item.get("tool_calls") or []
            if calls:
                message["tool_calls"] = [
                    {"id": c["id"], "type": "function", "function": {"name": c["name"], "arguments": tool_arguments(c["arguments"])}}
                    for c in calls
                ]
            if item.get("model_id"):
                message["model_id"] = item["model_id"]
            if pending:
                message["reasoning_content"] = "\n".join(pending)
        elif kind == "tool_result":
            if item.get("images"):
                raise ValueError("tool results with images are not replayed")
            message = {"role": "tool", "tool_call_id": item["tool_call_id"], "content": item.get("content") or ""}
        else:
            raise ValueError(f"history item type {kind!r} is not replayed")
        pending = []
        out.append(message)
    return out


def build_request(session, cut, variant, reported_tokens=None, context_window=None):
    items = [json.loads(line) for line in (session / "chat_history.jsonl").read_text().splitlines() if line.strip()]
    if not 0 < cut < len(items) or items[0]["type"] != "system":
        raise ValueError(f"cut {cut} is outside a history of {len(items)} items starting with a system prompt")
    if items[cut]["type"] not in ("reasoning", "assistant") or items[cut - 1]["type"] == "reasoning":
        raise ValueError(f"cut {cut} is not the first item of a model reply")
    history = items[:cut]
    if pruned(history) != history:
        if reported_tokens is None or context_window is None:
            raise ValueError("the client may have trimmed old tool results here: give reported_tokens and context_window")
        if reported_tokens > context_window // 2:
            history = pruned(history)
    model = next((i.get("model_id") for i in items[:cut] if i["type"] == "assistant" and i.get("model_id")), None)
    if model is None:
        raise ValueError("no assistant item before the cut records the model id")
    system = substitute(items[0]["content"], variant.get("system", []), "system prompt")
    tools = json.loads((session / "tool_definitions.json").read_text())
    names = {tool["function"]["name"] for tool in tools}
    for name, pairs in variant.get("tools", {}).items():
        if name not in names:
            raise ValueError(f"tool {name!r} is not in this session's tool definitions")
    for tool in tools:
        function = tool["function"]
        pairs = variant.get("tools", {}).get(function["name"], [])
        if pairs:
            function["description"] = substitute(function.get("description") or "", pairs, function["name"])
    return {
        "model": model,
        "messages": chat_messages(history, system),
        "tools": tools,
        "stream": True,
        "stream_options": {"include_usage": True},
    }


def collect(lines, deadline=None):
    """First tool call, text and finish reason from a Chat Completions SSE stream, cut at `deadline` (monotonic)."""
    calls, text, finish = {}, [], None
    for raw in lines:
        if deadline is not None and time.monotonic() > deadline:
            finish = "runaway"
            break
        line = raw.decode("utf-8", "replace").strip() if isinstance(raw, bytes) else raw.strip()
        if not line.startswith("data:") or line == "data: [DONE]":
            continue
        for choice in json.loads(line[5:]).get("choices") or []:
            delta = choice.get("delta") or {}
            text.append(delta.get("content") or "")
            for call in delta.get("tool_calls") or []:
                slot = calls.setdefault(call.get("index", 0), {"name": "", "arguments": ""})
                function = call.get("function") or {}
                slot["name"] += function.get("name") or ""
                slot["arguments"] += function.get("arguments") or ""
            finish = choice.get("finish_reason") or finish
    return (calls[min(calls)] if calls else None), "".join(text), finish


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("points")
    parser.add_argument("variants")
    parser.add_argument("variant")
    parser.add_argument("samples", type=int)
    parser.add_argument("--server", default=DEFAULT_SERVER)
    # The longest legitimate generation in the first Terminal-Bench baseline took about two minutes.
    parser.add_argument("--max-seconds", type=float, default=300)
    args = parser.parse_args()
    key = os.environ.get("CORTEX_API_KEY")
    if not key:
        sys.exit("CORTEX_API_KEY must be set")
    variant = json.loads(Path(args.variants).read_text())[args.variant]
    context = ssl.create_default_context()
    for line in Path(args.points).read_text().splitlines():
        if not line.strip():
            continue
        point = json.loads(line)
        body = json.dumps(build_request(
            Path(point["session"]), point["cut"], variant, point.get("reported_tokens"), point.get("context_window")
        )).encode()
        for sample in range(args.samples):
            request = urllib.request.Request(
                f"{args.server}/v1/chat/completions",
                data=body,
                headers={"Authorization": f"Bearer {key}", "Content-Type": "application/json"},
            )
            started = time.monotonic()
            try:
                with urllib.request.urlopen(request, timeout=900, context=context) as response:
                    call, text, finish = collect(response, started + args.max_seconds)
                error = None if finish else "the stream ended without a finish reason"
            except http.client.HTTPException as e:
                # cortex-api aborts the stream when its loop guard fires or the upstream fails mid-generation.
                call, text, finish, error = None, "", "aborted", repr(e)
            except (OSError, ValueError) as e:
                call, text, finish, error = None, "", None, repr(e)
            print(json.dumps({
                **point, "variant": args.variant, "sample": sample, "call": call, "text": text, "finish": finish,
                "error": error, "seconds": round(time.monotonic() - started, 1),
            }), flush=True)


if __name__ == "__main__":
    main()
