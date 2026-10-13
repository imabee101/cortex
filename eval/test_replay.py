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

    def test_images_are_refused(self):
        item = {"type": "user", "content": [{"type": "image", "url": "data:"}]}
        with self.assertRaises(ValueError):
            replay.chat_messages([item], "S")


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
