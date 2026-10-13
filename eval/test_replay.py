import json
import tempfile
import unittest
from pathlib import Path

import replay

HISTORY = [
    {"type": "system", "content": "Run a server as a background command."},
    {"type": "user", "content": [{"type": "text", "text": "a"}, {"type": "text", "text": "b"}]},
    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "plan"}]},
    {"type": "reasoning", "summary": [], "content": [{"type": "text", "text": "more"}]},
    {"type": "assistant", "content": "", "model_id": "m1",
     "tool_calls": [{"id": "c1", "name": "bash", "arguments": "{\"command\":\"ls\"}"}]},
    {"type": "tool_result", "tool_call_id": "c1", "content": "exit: 0\n"},
    {"type": "reasoning", "summary": [{"type": "summary_text", "text": "next"}]},
    {"type": "assistant", "content": "done", "model_id": "m1"},
]
TOOLS = [{"type": "function", "function": {"name": "bash", "description": "Runs until it exits.", "parameters": {}}}]


class ChatMessages(unittest.TestCase):
    def test_reasoning_folds_into_the_following_assistant(self):
        messages = replay.chat_messages(HISTORY, "S")
        self.assertEqual([m["role"] for m in messages], ["system", "user", "assistant", "tool", "assistant"])
        self.assertEqual(messages[0]["content"], "S")
        self.assertEqual(messages[1]["content"], "a\nb")
        self.assertEqual(messages[2]["reasoning_content"], "plan\nmore")
        self.assertEqual(messages[2]["tool_calls"][0]["function"], {"name": "bash", "arguments": "{\"command\":\"ls\"}"})
        self.assertEqual(messages[4]["reasoning_content"], "next")

    def test_reasoning_with_no_following_assistant_is_dropped(self):
        messages = replay.chat_messages(HISTORY[:3] + [HISTORY[5]], "S")
        self.assertNotIn("reasoning_content", messages[-1])

    def test_assistant_messages_carry_the_model_and_valid_arguments(self):
        broken = dict(HISTORY[4], tool_calls=[{"id": "c1", "name": "bash", "arguments": "{\"command\":"}])
        message = replay.chat_messages([HISTORY[0], broken], "S")[1]
        self.assertEqual(message["model_id"], "m1")
        self.assertEqual(message["tool_calls"][0]["function"]["arguments"], "{}")

    def test_arguments_serde_rejects_are_replaced(self):
        self.assertEqual(replay.tool_arguments('{"x": NaN}'), "{}")

    def test_messages_from_another_agent_are_refused(self):
        with self.assertRaises(ValueError):
            replay.chat_messages([{"type": "user", "content": "hi", "synthetic_reason": "agent_message"}], "S")

    def test_images_are_refused(self):
        item = {"type": "user", "content": [{"type": "image", "url": "data:"}]}
        with self.assertRaises(ValueError):
            replay.chat_messages([item], "S")


class Pruned(unittest.TestCase):
    def history(self, users, result):
        items = [{"type": "system", "content": "S"}, {"type": "user", "content": "go"},
                 {"type": "assistant", "content": "", "tool_calls": [{"id": "c", "name": "bash", "arguments": "{}"}]},
                 {"type": "tool_result", "tool_call_id": "c", "content": result}]
        return items + [{"type": "user", "content": f"r{i}"} for i in range(users)]

    def test_recent_results_are_kept(self):
        items = self.history(3, "x" * 5000)
        self.assertEqual(replay.pruned(items), items)

    def test_old_large_results_keep_head_and_tail(self):
        content = "a" * 1500 + "b" * 2000 + "c" * 1500
        trimmed = replay.pruned(self.history(4, content))[3]["content"]
        self.assertEqual(trimmed, "a" * 1500 + "\n\n[\u2026trimmed\u2026]\n\n" + "c" * 1500)
        self.assertEqual(replay.pruned(self.history(4, "x" * 4000))[3]["content"], "x" * 4000)

    def test_very_old_results_are_cleared(self):
        self.assertEqual(replay.pruned(self.history(11, "short"))[3]["content"], "[Tool result omitted \u2014 too old]")


class Substitute(unittest.TestCase):
    def test_each_pair_must_match_once(self):
        self.assertEqual(replay.substitute("a b", [["a", "c"]], "x"), "c b")
        with self.assertRaises(ValueError):
            replay.substitute("a b", [["z", "c"]], "x")
        with self.assertRaises(ValueError):
            replay.substitute("a a", [["a", "c"]], "x")


class BuildRequest(unittest.TestCase):
    def setUp(self):
        self.dir = tempfile.TemporaryDirectory()
        self.session = Path(self.dir.name)
        (self.session / "chat_history.jsonl").write_text("\n".join(json.dumps(i) for i in HISTORY) + "\n")
        (self.session / "tool_definitions.json").write_text(json.dumps(TOOLS))

    def tearDown(self):
        self.dir.cleanup()

    def test_only_the_named_text_changes(self):
        variant = {"system": [["a background command", "a detached process"]], "tools": {"bash": [["exits", "ends"]]}}
        body = replay.build_request(self.session, 6, variant)
        self.assertEqual(body["model"], "m1")
        self.assertEqual(body["messages"][0]["content"], "Run a server as a detached process.")
        self.assertEqual(body["tools"][0]["function"]["description"], "Runs until it ends.")
        self.assertEqual(body["messages"][1:], replay.chat_messages(HISTORY[1:6], ""))

    def test_the_original_variant_sends_the_session_unchanged(self):
        body = replay.build_request(self.session, 6, {})
        self.assertEqual(body["messages"][0]["content"], HISTORY[0]["content"])
        self.assertEqual(body["tools"], TOOLS)

    def test_a_cut_before_any_model_output_is_refused(self):
        with self.assertRaises(ValueError):
            replay.build_request(self.session, 2, {})
        with self.assertRaises(ValueError):
            replay.build_request(self.session, len(HISTORY), {})

    def test_a_cut_inside_a_reply_or_on_a_tool_result_is_refused(self):
        for cut in (5, 7):
            with self.assertRaises(ValueError):
                replay.build_request(self.session, cut, {})

    def test_trimming_needs_the_token_total_and_applies_past_half_the_window(self):
        history = [HISTORY[0], HISTORY[1], HISTORY[4], dict(HISTORY[5], content="x" * 5000)]
        history += [{"type": "user", "content": f"r{i}"} for i in range(4)] + [HISTORY[7]]
        (self.session / "chat_history.jsonl").write_text("\n".join(json.dumps(i) for i in history) + "\n")
        with self.assertRaises(ValueError):
            replay.build_request(self.session, 8, {})
        kept = replay.build_request(self.session, 8, {}, reported_tokens=500, context_window=1000)
        self.assertEqual(kept["messages"][3]["content"], "x" * 5000)
        trimmed = replay.build_request(self.session, 8, {}, reported_tokens=501, context_window=1000)
        self.assertEqual(len(trimmed["messages"][3]["content"]), 3000 + len(replay.SOFT_TRIM_SEPARATOR))

    def test_a_tool_without_a_description_is_sent_as_saved(self):
        (self.session / "tool_definitions.json").write_text(json.dumps([{"type": "function", "function": {"name": "bash"}}]))
        self.assertEqual(replay.build_request(self.session, 6, {})["tools"][0]["function"], {"name": "bash"})

    def test_an_unknown_tool_is_refused(self):
        with self.assertRaises(ValueError):
            replay.build_request(self.session, 6, {"tools": {"edit": [["a", "b"]]}})


class Collect(unittest.TestCase):
    def test_tool_call_arguments_are_joined_across_chunks(self):
        chunks = [
            {"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"name": "bash", "arguments": "{\"comm"}}]}}]},
            {"choices": [{"delta": {"tool_calls": [{"index": 0, "function": {"arguments": "and\":\"ls\"}"}}]}}]},
            {"choices": [{"delta": {"tool_calls": [{"index": 1, "function": {"name": "read", "arguments": "{}"}}]}}]},
            {"choices": [{"delta": {}, "finish_reason": "tool_calls"}]},
        ]
        lines = [f"data: {json.dumps(c)}".encode() for c in chunks] + [b"", b"data: [DONE]"]
        call, text, finish = replay.collect(lines)
        self.assertEqual(call, {"name": "bash", "arguments": "{\"command\":\"ls\"}"})
        self.assertEqual((text, finish), ("", "tool_calls"))

    def test_a_generation_past_the_deadline_is_abandoned(self):
        lines = [b'data: {"choices":[{"delta":{"content":"x"}}]}'] * 3
        call, text, finish = replay.collect(lines, deadline=0)
        self.assertEqual((call, text, finish), (None, "", "runaway"))


if __name__ == "__main__":
    unittest.main()
