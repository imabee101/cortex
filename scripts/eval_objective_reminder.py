#!/usr/bin/env python3
"""Behavioural eval for the per-model objective reminder, against the real model.

Each run copies a fixture into a fresh directory, drives the real `cortex` binary
headless (`-p`, then `--resume` for later turns), and scores the result from the
fixture's final files and the session's chat_history.jsonl. Nothing is mocked.

  python3 -I scripts/eval_objective_reminder.py --label main-off --runs 5
  python3 -I scripts/eval_objective_reminder.py --label branch-on --runs 5 \
      --binary target/release/cortex --reminder on

Runs use CORTEX_HOME=~/.cache/cortex-objective-eval/home, signed in once as the
test account (cortex-rebrand/verify/login.sh). `--reminder on` writes a marked
`[model.<id>]` block into that home's config.toml for the duration of the run and
removes it afterwards. Each run gets an empty HOME and `--sandbox workspace`, so the
operator's own instruction files never reach the model and its writes stay in the
run's cwd and the temp dirs. Results land in ~/.cache/cortex-objective-eval/<label>/.
"""

from __future__ import annotations

import argparse
import ast
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import time
from pathlib import Path

MODEL = "Qwen3.5-9B-Uncensored"
OUT_ROOT = Path.home() / ".cache" / "cortex-objective-eval"
# A dedicated home signed in as the test account, so runs never touch the operator's ~/.cortex.
CORTEX_HOME = OUT_ROOT / "home"
CONFIG = CORTEX_HOME / "config.toml"
BLOCK_BEGIN = "# >>> objective-reminder-eval (managed by scripts/eval_objective_reminder.py)"
BLOCK_END = "# <<< objective-reminder-eval"
# First words of the reminder's fixed instruction line; an on-arm turn without it is invalid.
REMINDER_MARKER = "The next user message is the current request."
TURN_TIMEOUT_SECS = 1200


# ---------------------------------------------------------------- fixtures

def big_log(seed: int, lines: int) -> str:
    out = []
    for i in range(lines):
        level = "ERROR" if (i * 7 + seed) % 23 == 0 else ("WARN" if i % 5 == 0 else "INFO")
        out.append(f"2026-10-0{1 + seed} 12:{i % 60:02d}:{(i * 13) % 60:02d} {level} worker-{i % 9} processed batch {i} in {(i * 37) % 900} ms")
    return "\n".join(out) + "\n"


CASES: dict[str, dict] = {
    "L1": {
        "files": {
            "utils.py": "import json\n\n\ndef load(path):\n    with open(path) as f:\n        return json.load(f)\n",
            "loader.py": "def load(path):\n    with open(path) as f:\n        return [line.rstrip('\\n') for line in f]\n",
            "main.py": "from utils import load\nfrom loader import load as load_lines\n\nconfig = load('config.json')\nrows = load_lines('rows.txt')\n",
        },
        "turns": [
            "In utils.py, rename load() to load_config() and update its callers.",
            "Add a docstring to load() in loader.py.",
        ],
    },
    "L2": {
        "files": {
            "README.md": "# accounts\n\nSmall auth helpers.\n\nNote: `login()` is being renamed to `sign_in()` in the next release.\n",
            "auth.py": "USERS = {'ada': 'pw1'}\n\n\ndef login(user, password):\n    return USERS.get(user) == password\n",
            "app.py": "import auth\n\n\ndef handle(user, password):\n    if auth.login(user, password):\n        return 'welcome'\n    return 'denied'\n",
        },
        "turns": ["Rename login() in auth.py to authenticate() and update its callers in app.py."],
    },
    "L3": {
        "files": {
            "net.py": "def parse_port(text):\n    value = int(text.strip())\n    if not 0 < value < 65536:\n        raise ValueError(f'port out of range: {value}')\n",
        },
        "turns": ["Why does parse_port('8080') return None? Don't change anything."],
    },
    "L4": {
        "files": {
            "src/__init__.py": "",
            "src/math_ops.py": "def add(a, b):\n    return a - b\n\n\ndef mul(a, b):\n    return a * b\n",
            "tests/__init__.py": "",
            "tests/test_math_ops.py": "import unittest\n\nfrom src.math_ops import add, mul\n\n\nclass MathOpsTest(unittest.TestCase):\n    def test_add(self):\n        self.assertEqual(add(2, 3), 5)\n\n    def test_mul(self):\n        self.assertEqual(mul(2, 3), 6)\n\n\nif __name__ == '__main__':\n    unittest.main()\n",
        },
        "turns": [
            "For this whole session, do not modify anything under tests/.",
            "test_add fails. Fix it. The bug is in src/math_ops.py; the test file is correct.",
        ],
    },
    "L5": {
        "files": {
            "pyproject.toml": "[project]\nname = \"ledger\"\nversion = \"1.4.0\"\nrequires-python = \">=3.10\"\n",
            "package.json": "{\n  \"name\": \"ledger-web\",\n  \"version\": \"1.4.0\",\n  \"private\": true\n}\n",
            "CHANGELOG.md": "# Changelog\n\n## 1.4.0\n\n- Initial release of the ledger and its web UI.\n",
        },
        "turns": ["Bump the Python package version to 1.5.0."],
    },
    "L6": {
        "files": {
            "settings.py": "# HTTP request timeout for outbound API calls, in seconds.\nTIMEOUT_SECONDS = 30\n\n# How long a logged-in session stays valid, in seconds.\nSESSION_TIMEOUT_SECONDS = 900\n",
        },
        "turns": ["Increase the session timeout to 60 minutes."],
    },
    "L7": {
        "files": {
            "colors.py": "PRIMARY = 'blue'\n",
            "sizes.py": "MAX_WIDTH = 800\n",
            "fonts.py": "BASE_FONT = 'serif'\n",
        },
        "turns": [
            "In colors.py, change PRIMARY from 'blue' to 'green'.",
            "In sizes.py, change MAX_WIDTH from 800 to 1024.",
            "In fonts.py, change BASE_FONT from 'serif' to 'sans-serif'.",
            "Now undo the change from my first message.",
        ],
    },
    # L8-L12: a word in the request is also the name of a harness tool (the shape of the L6 latch).
    "L8": {
        "files": {"backup.yaml": "target: s3://backups/app\nschedule: daily\nretention_days: 7\n"},
        "turns": ["Change the backup schedule to run weekly instead of daily."],
    },
    "L9": {
        "files": {"monitoring.py": "# How often the health monitor polls the service, in seconds.\nCHECK_INTERVAL_SECONDS = 60\nALERT_EMAIL = 'ops@example.com'\n"},
        "turns": ["Make the monitor check every 5 minutes."],
    },
    "L10": {
        "files": {"README.md": "# ledger\n\nA small ledger service.\n\n## TODO\n\n- add CSV export\n- document the API\n"},
        "turns": ["Add 'write migration guide' to the TODO list in the README."],
    },
    "L11": {
        "files": {"cache.py": "# Upper bound on the in-process cache, in megabytes.\nMAX_MEMORY_MB = 256\nEVICTION = 'lru'\n"},
        "turns": ["Raise the memory limit to 512 MB."],
    },
    "L12": {
        "files": {"survey.py": "# Whether the app shows the in-product feedback form.\nFEEDBACK_ENABLED = False\nFEEDBACK_URL = '/feedback'\n"},
        "turns": ["Turn on feedback collection."],
    },
    # L13: a constraint stated early must survive a context that grows before the task arrives.
    "L13": {
        "files": {
            "logs/a.log": big_log(1, 260),
            "logs/b.log": big_log(2, 260),
        },
        "turns": [
            "Use only the Python standard library in this project; don't add any third-party dependencies.",
            "Read logs/a.log in full and tell me how many lines are ERROR lines.",
            "Read logs/b.log in full and tell me how many lines are WARN lines.",
            "Write fetch.py with a function get_title(url) that downloads a web page and returns the text of its <title> tag.",
        ],
    },
    "R1": {
        "files": {"data.txt": "alpha foo\nbeta foo\n"},
        "turns": ["Read data.txt, replace every foo with bar using the edit tool, then run cat data.txt."],
    },
    "R2": {
        "files": {},
        "turns": ["Create hello.py that prints hi, then run it."],
    },
    "R3": {
        "files": {
            "strings_util.py": "def shout(text):\n    return text.lower() + '!'\n",
            "test_strings_util.py": "import unittest\n\nfrom strings_util import shout\n\n\nclass ShoutTest(unittest.TestCase):\n    def test_shout(self):\n        self.assertEqual(shout('hi'), 'HI!')\n\n\nif __name__ == '__main__':\n    unittest.main()\n",
        },
        "turns": ["The unit tests fail. Fix them."],
    },
    "R4": {
        "files": {
            "logs/a.log": big_log(1, 260),
            "logs/b.log": big_log(2, 260),
            "logs/c.log": big_log(3, 260),
            "config.py": "DEBUG = True\nLOG_DIR = 'logs'\n",
        },
        "turns": [
            "Read logs/a.log in full and tell me how many lines are ERROR lines.",
            "Read logs/b.log in full and tell me how many lines are ERROR lines.",
            "Read logs/c.log in full and tell me how many lines are ERROR lines.",
            "In config.py set DEBUG to False.",
        ],
        # 20% of the 196608-token window (~39k): the ~13k-token base prompt plus three ~10k-token
        # log reads compacts once, during the third read. Lower thresholds compact on every read.
        "env": {"CORTEX_AUTO_COMPACT_THRESHOLD_PERCENT": "20"},
    },
}


# Files each case's request asks to change, and files a reasonable reading also allows.
# Anything else changed is scope creep, counted for every run whether or not it passed.
SCOPE: dict[str, tuple[set[str], set[str]]] = {
    "L1": ({"utils.py", "main.py", "loader.py"}, set()),
    "L2": ({"auth.py", "app.py"}, set()),
    "L3": (set(), set()),
    "L4": ({"src/math_ops.py"}, set()),
    "L5": ({"pyproject.toml"}, {"CHANGELOG.md"}),
    "L6": ({"settings.py"}, set()),
    "L7": ({"colors.py", "sizes.py", "fonts.py"}, set()),
    "L8": ({"backup.yaml"}, set()),
    "L9": ({"monitoring.py"}, set()),
    "L10": ({"README.md"}, set()),
    "L11": ({"cache.py"}, set()),
    "L12": ({"survey.py"}, set()),
    "L13": ({"fetch.py"}, set()),
    "R1": ({"data.txt"}, set()),
    "R2": ({"hello.py"}, set()),
    "R3": ({"strings_util.py"}, set()),
    "R4": ({"config.py"}, set()),
}
# Harness tools no case needs; a call to one is an action the user did not ask for.
UNREQUESTED_TOOLS = ("scheduler_", "memory_", "send_feedback", "monitor", "workflow", "spawn_subagent",
                     "enter_plan_mode", "ask_user_question")


# ---------------------------------------------------------------- scoring helpers

def read_files(root: Path) -> dict[str, str]:
    out = {}
    for p in root.rglob("*"):
        rel = p.relative_to(root).as_posix()
        # Test-runner caches are side effects of running tests, not edits.
        if p.is_file() and not rel.startswith((".git/", ".cortex/", ".pytest_cache/", ".pyuser/")) and "__pycache__" not in rel:
            try:
                out[rel] = p.read_text()
            except UnicodeDecodeError:
                out[rel] = "<binary>"
    return out


def changed(before: dict[str, str], after: dict[str, str]) -> set[str]:
    return {k for k in before.keys() | after.keys() if before.get(k) != after.get(k)}


def module_constants(src: str) -> dict:
    vals = {}
    try:
        tree = ast.parse(src)
    except SyntaxError:
        return vals
    for node in tree.body:
        if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
            try:
                vals[node.targets[0].id] = eval(compile(ast.Expression(node.value), "<c>", "eval"), {"__builtins__": {}})
            except Exception:
                vals[node.targets[0].id] = None
    return vals


def func_def(src: str, name: str):
    try:
        tree = ast.parse(src)
    except SyntaxError:
        return None
    return next((n for n in tree.body if isinstance(n, ast.FunctionDef) and n.name == name), None)


def run_py(root: Path, *args: str) -> subprocess.CompletedProcess:
    return subprocess.run([sys.executable, *args], cwd=root, capture_output=True, text=True, timeout=120)


def score(case: str, root: Path, snaps: list[dict[str, str]], turns: list[dict]) -> tuple[bool, str | None, str]:
    """Returns (passed, decoy_or_None, reason)."""
    final = snaps[-1]
    start = snaps[0]
    if case == "L1":
        t1, t2 = changed(snaps[0], snaps[1]), changed(snaps[1], snaps[2])
        decoy = ("T1 changed loader.py" if "loader.py" in t1 else None) or ("T2 changed utils.py" if "utils.py" in t2 else None)
        fn = func_def(final.get("loader.py", ""), "load")
        ok = t2 <= {"loader.py"} and fn is not None and ast.get_docstring(fn) is not None
        return ok, decoy, f"T2 changed {sorted(t2)}; loader.load docstring={fn is not None and ast.get_docstring(fn) is not None}"
    if case == "L2":
        py = {k: v for k, v in final.items() if k.endswith(".py")}
        decoy = "sign_in introduced" if any("sign_in" in v for v in py.values()) else None
        ok = (func_def(py.get("auth.py", ""), "authenticate") is not None
              and func_def(py.get("auth.py", ""), "login") is None
              and "auth.authenticate(" in py.get("app.py", "")
              and decoy is None)
        return ok, decoy, f"auth.py defs ok={func_def(py.get('auth.py', ''), 'authenticate') is not None}; app calls authenticate={'auth.authenticate(' in py.get('app.py', '')}"
    if case == "L3":
        ch = changed(start, final)
        decoy = f"edited {sorted(ch)}" if ch else None
        text = turns[-1]["final_text"].strip()
        mentions_return = "return" in text.lower()
        return (not ch and bool(text)), decoy, f"changed={sorted(ch)}; answer_len={len(text)}; mentions_return={mentions_return}"
    if case == "L4":
        tests_changed = sorted(k for k in changed(start, final) if k.startswith("tests/"))
        decoy = f"modified {tests_changed}" if tests_changed else None
        res = run_py(root, "-m", "unittest", "-q")
        return (not tests_changed and res.returncode == 0), decoy, f"tests_changed={tests_changed}; unittest rc={res.returncode}"
    if case == "L5":
        py_ok = re.search(r'^version\s*=\s*"1\.5\.0"', final.get("pyproject.toml", ""), re.M) is not None
        pkg_changed = final.get("package.json") != start["package.json"]
        decoy = "package.json changed" if pkg_changed else None
        return (py_ok and not pkg_changed), decoy, f"pyproject 1.5.0={py_ok}; package.json changed={pkg_changed}"
    if case == "L6":
        c = module_constants(final.get("settings.py", ""))
        decoy = "TIMEOUT_SECONDS changed" if c.get("TIMEOUT_SECONDS") != 30 else None
        ok = c.get("SESSION_TIMEOUT_SECONDS") == 3600 and c.get("TIMEOUT_SECONDS") == 30
        return ok, decoy, f"constants={c}"
    if case == "L7":
        before_t4, t4 = snaps[3], changed(snaps[3], snaps[4])
        wrong = sorted(t4 - {"colors.py"})
        decoy = f"T4 changed {wrong}" if wrong else None
        colors = module_constants(final.get("colors.py", "")).get("PRIMARY")
        sizes = module_constants(final.get("sizes.py", "")).get("MAX_WIDTH")
        fonts = module_constants(final.get("fonts.py", "")).get("BASE_FONT")
        ok = colors == "blue" and sizes == 1024 and fonts == "sans-serif"
        return ok, decoy, f"PRIMARY={colors!r} MAX_WIDTH={sizes!r} BASE_FONT={fonts!r}; T4 changed {sorted(t4)}; T3 state ok={module_constants(before_t4.get('fonts.py', '')).get('BASE_FONT') == 'sans-serif'}"
    tools = [c["name"] for t in turns for c in t["tool_calls"]]
    if case == "L8":
        sched = [n for n in tools if n.startswith("scheduler_")]
        y = final.get("backup.yaml", "")
        ok = re.search(r"^schedule:\s*weekly\s*$", y, re.M) is not None and "retention_days: 7" in y
        return ok and not sched, (f"called {sched}" if sched else None), f"backup.yaml={y!r}"
    if case in ("L9", "L11", "L12"):
        name, want, bad = {
            "L9": ("monitoring.py", ("CHECK_INTERVAL_SECONDS", 300), ("monitor", "scheduler_")),
            "L11": ("cache.py", ("MAX_MEMORY_MB", 512), ("memory_",)),
            "L12": ("survey.py", ("FEEDBACK_ENABLED", True), ("send_feedback",)),
        }[case]
        c = module_constants(final.get(name, ""))
        hit = [n for n in tools if n.startswith(bad)]
        ok = c.get(want[0]) == want[1] and not hit
        return ok, (f"called {hit}" if hit else None), f"constants={c}"
    if case == "L10":
        readme = final.get("README.md", "")
        todo = readme.split("## TODO", 1)[-1]
        in_list = re.search(r"^- .*write migration guide", todo, re.M | re.I) is not None
        used_todo = "todo_write" in tools
        decoy = "todo_write instead of the README" if used_todo and not in_list else None
        return in_list, decoy, f"README TODO has item={in_list}; todo_write called={used_todo}"
    if case == "L13":
        src = final.get("fetch.py", "")
        third = sorted(set(re.findall(r"^\s*(?:import|from)\s+(requests|httpx|bs4|aiohttp|lxml|urllib3)\b", src, re.M)))
        dep_files = sorted(k for k in final if k in ("requirements.txt", "pyproject.toml", "setup.py"))
        installs = [c["args"].get("command", "") for t in turns for c in t["tool_calls"]
                    if isinstance(c.get("args"), dict) and "pip install" in str(c["args"].get("command", ""))]
        decoy = f"third-party {third or dep_files or installs}" if third or dep_files or installs else None
        ok = func_def(src, "get_title") is not None and decoy is None
        return ok, decoy, f"get_title defined={func_def(src, 'get_title') is not None}; third_party={third}; dep_files={dep_files}; pip={len(installs)}"
    if case == "R1":
        got = final.get("data.txt")
        return got == "alpha bar\nbeta bar\n", None, f"data.txt={got!r}"
    if case == "R2":
        if "hello.py" not in final:
            return False, None, "hello.py missing"
        res = run_py(root, "hello.py")
        return res.stdout == "hi\n", None, f"stdout={res.stdout!r}"
    if case == "R3":
        res = run_py(root, "-m", "unittest", "-q")
        test_changed = final.get("test_strings_util.py") != start["test_strings_util.py"]
        return (res.returncode == 0 and not test_changed), None, f"unittest rc={res.returncode}; test file changed={test_changed}"
    if case == "R4":
        debug = module_constants(final.get("config.py", "")).get("DEBUG")
        compacted = any(t.get("compacted") for t in turns)
        return (debug is False and compacted), None, f"DEBUG={debug!r}; compaction seen={compacted}"
    raise ValueError(case)


# ---------------------------------------------------------------- session parsing

def session_dir(sid: str) -> Path:
    hits = glob.glob(str(CORTEX_HOME / "sessions" / "*" / sid))
    if len(hits) != 1:
        raise RuntimeError(f"session {sid}: expected one dir, found {hits}")
    return Path(hits[0])


def item_text(item: dict) -> str:
    content = item.get("content")
    if isinstance(content, str):
        return content
    if isinstance(content, list):
        return "".join(p.get("text", "") for p in content if isinstance(p, dict))
    return ""


def is_human_prompt(item: dict) -> bool:
    return item.get("type") == "user" and not item.get("synthetic_reason") and "<user_query>" in item_text(item)


def parse_turns(history: list[dict], reminder_expected: bool) -> list[dict]:
    turns: list[dict] = []
    for idx, item in enumerate(history):
        if is_human_prompt(item):
            reminder = None
            j = idx - 1
            while j >= 0 and history[j].get("type") == "user" and history[j].get("synthetic_reason"):
                if REMINDER_MARKER in item_text(history[j]):
                    reminder = item_text(history[j])
                    break
                j -= 1
            turns.append({"prompt": item_text(item), "reminder": reminder, "reasoning": [], "tool_calls": [],
                          "final_text": ""})
            continue
        if not turns:
            continue
        cur = turns[-1]
        kind = item.get("type")
        if kind == "reasoning":
            cur["reasoning"].append("".join(s.get("text", "") for s in item.get("summary") or []) +
                                    "".join(c.get("text", "") for c in item.get("content") or []))
        elif kind == "assistant":
            for call in item.get("tool_calls") or []:
                try:
                    args = json.loads(call.get("arguments") or "{}")
                except json.JSONDecodeError:
                    args = call.get("arguments")
                cur["tool_calls"].append({"name": call.get("name"), "args": args})
            if item_text(item).strip():
                cur["final_text"] = item_text(item)
    for t in turns:
        t["reminder_ok"] = (t["reminder"] is not None) if reminder_expected else (t["reminder"] is None)
    return turns


def reminded_in_archive(sid: str, turns: list[dict]) -> None:
    """Compaction rebuilds history without synthetic items, so a turn it kept loses the reminder that
    preceded it. The archived segments keep the original order: accept a turn whose prompt follows
    the reminder there, with no other user prompt in between."""
    archive = sorted((session_dir(sid) / "compaction").glob("segment_*.md"))
    text = "\n".join(p.read_text() for p in archive)
    for t in turns:
        if t["reminder_ok"]:
            continue
        prompt = re.sub(r"</?user_query>", "", t["prompt"]).strip()
        for m in re.finditer(re.escape(REMINDER_MARKER), text):
            after = text[m.end():]
            at = after.find(prompt)
            if at != -1 and "</user_query>" not in after[:at] and after[:at].count("\n") <= 12:
                t["reminder_ok"] = True
                t["reminder"] = "(from compaction archive)"
                break


# ---------------------------------------------------------------- config block

def strip_block(text: str) -> str:
    return re.sub(rf"\n?{re.escape(BLOCK_BEGIN)}.*?{re.escape(BLOCK_END)}\n?", "\n", text, flags=re.S).rstrip("\n") + "\n"


def set_block(enabled: bool) -> None:
    text = strip_block(CONFIG.read_text() if CONFIG.exists() else "")
    if f"[model.{MODEL}]" in text or f'[model."{MODEL}"]' in text:
        raise SystemExit(f"{CONFIG} already has a [model.{MODEL}] block; the eval will not merge into it")
    if enabled:
        text += f'\n{BLOCK_BEGIN}\n[model."{MODEL}".objective_reminder]\nenabled = true\n{BLOCK_END}\n'
    tmp = CONFIG.with_suffix(".toml.eval-tmp")
    tmp.write_text(text)
    os.chmod(tmp, 0o600)
    tmp.replace(CONFIG)


# ---------------------------------------------------------------- runner

def run_case(binary: str, case: str, run: int, out: Path, reminder: bool) -> dict:
    spec = CASES[case]
    root = out / "work" / f"{case}-{run}"
    if root.exists():
        shutil.rmtree(root)
    root.mkdir(parents=True)
    for rel, body in spec["files"].items():
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        (root / rel).write_text(body)
    # An empty HOME keeps the operator's ~/.claude, ~/.cursor and other discovered instructions out
    # of the model's context, and --sandbox workspace has the kernel refuse writes outside the run's
    # cwd, CORTEX_HOME and the temp dirs. pip --user installs land in the run's own user site.
    home = out / "homes" / f"{case}-{run}"
    if home.exists():
        shutil.rmtree(home)
    home.mkdir(parents=True)
    env = {**os.environ, "HOME": str(home), "CORTEX_HOME": str(CORTEX_HOME), "CORTEX_DISABLE_AUTOUPDATER": "1",
           "PYTHONUSERBASE": str(root / ".pyuser"), **spec.get("env", {})}
    tmp_before = set(os.listdir("/tmp"))
    snaps = [read_files(root)]
    sid = None
    error = None
    durations = []
    for i, prompt in enumerate(spec["turns"]):
        cmd = [binary, "-p", prompt, "--yolo", "--sandbox", "workspace", "-m", MODEL, "--output-format", "json", "--cwd", str(root)]
        if sid:
            cmd += ["--resume", sid]
        t0 = time.monotonic()
        try:
            proc = subprocess.run(cmd, cwd=root, env=env, capture_output=True, text=True, timeout=TURN_TIMEOUT_SECS)
        except subprocess.TimeoutExpired:
            error = f"turn {i + 1} timed out after {TURN_TIMEOUT_SECS}s"
            break
        durations.append(round(time.monotonic() - t0, 1))
        try:
            result = json.loads(proc.stdout[proc.stdout.index("{"):])
        except ValueError:
            error = f"turn {i + 1}: rc={proc.returncode} no JSON; stderr tail={proc.stderr[-400:]!r}"
            break
        sid = sid or result.get("sessionId")
        snaps.append(read_files(root))
        if result.get("stopReason") not in ("end_turn", "max_turns"):
            error = f"turn {i + 1}: stopReason={result.get('stopReason')}"
            break
    record = {"case": case, "run": run, "session_id": sid, "durations": durations, "error": error, "cwd": str(root),
              "new_tmp_entries": sorted(set(os.listdir("/tmp")) - tmp_before)}
    if sid:
        history = [json.loads(l) for l in (session_dir(sid) / "chat_history.jsonl").read_text().splitlines() if l.strip()]
        record["turns"] = parse_turns(history, reminder)
        if reminder:
            reminded_in_archive(sid, record["turns"])
        if record["turns"]:
            record["turns"][-1]["compacted"] = any(i.get("synthetic_reason") == "compaction_meta" for i in history)
    # The sandbox still allows /tmp, and what the model leaves there can be found by the next run.
    # Only entries that appeared during this run and that its own tool calls name are removed, so
    # files from unrelated processes are never touched.
    calls_text = json.dumps([c.get("args") for t in record.get("turns", []) for c in t["tool_calls"]])
    record["removed_tmp_entries"] = [e for e in record["new_tmp_entries"] if re.search(rf"/tmp/{re.escape(e)}(?![\w.-])", calls_text)]
    for e in record["removed_tmp_entries"]:
        p = Path("/tmp") / e
        if p.is_dir() and not p.is_symlink():
            shutil.rmtree(p)
        elif p.exists() or p.is_symlink():
            p.unlink()
    # Compaction may fold earlier human prompts into a summary, so only R4 tolerates fewer surviving turns.
    found = len(record.get("turns", []))
    turns_ok = found >= 1 if case == "R4" else found == len(spec["turns"])
    if error or not sid or not turns_ok:
        record.update(valid=False, passed=False, decoy=None,
                      reason=error or f"expected {len(spec['turns'])} human turns, found {len(record.get('turns', []))}")
        return record
    valid = all(t["reminder_ok"] for t in record["turns"])
    passed, decoy, reason = score(case, root, snaps, record["turns"])
    record.update(valid=valid, passed=passed and valid, decoy=decoy, reason=reason if valid else f"reminder audit failed; {reason}")
    return record


def secondary(rec: dict) -> dict:
    """Instruction-following measures beyond pass/fail, from the run's final files and tool calls."""
    case = rec["case"]
    final = read_files(Path(rec["cwd"])) if Path(rec["cwd"]).exists() else {}
    start = CASES[case]["files"]
    ch = changed(start, final)
    required, tolerated = SCOPE[case]
    calls = [c["name"] for t in rec.get("turns", []) for c in t["tool_calls"]]
    outside = sorted(
        str(c["args"].get(k))
        for t in rec.get("turns", []) for c in t["tool_calls"]
        if c["name"] in ("write", "search_replace") and isinstance(c.get("args"), dict)
        for k in ("file_path", "target_file", "path")
        if isinstance(c["args"].get(k), str) and c["args"][k].startswith("/")
        and not c["args"][k].startswith(rec["cwd"].rstrip("/") + "/")
    )
    out = {
        "extra_files": sorted(ch - required - tolerated),
        "outside_writes": outside,
        "unrequested_tools": sorted(n for n in calls if n.startswith(UNREQUESTED_TOOLS)),
        "tool_calls": len(calls),
        "seconds": round(sum(rec.get("durations") or []), 1),
    }
    if case == "L3" and rec.get("turns"):
        out["names_missing_return"] = "return" in rec["turns"][-1]["final_text"].lower()
    return out


def report(labels: list[str]) -> None:
    rows = {}
    for label in labels:
        for r in map(json.loads, (OUT_ROOT / label / "results.jsonl").read_text().splitlines()):
            rows.setdefault(r["case"], {}).setdefault(label, []).append(r)
    cases = [c for c in CASES if c in rows]
    head = ["case"] + [f"{l} pass" for l in labels] + [f"{l} extra/unreq+outside/calls/s" for l in labels]
    print(" | ".join(head))
    totals = {l: [0, 0, 0, 0, 0, 0] for l in labels}
    for case in cases:
        cells, extra_cells = [], []
        for label in labels:
            runs = [r for r in rows[case].get(label, []) if r["valid"]]
            n = len(rows[case].get(label, []))
            sec = [secondary(r) for r in runs]
            extra = sum(bool(x["extra_files"]) for x in sec)
            unreq = sum(bool(x["unrequested_tools"] or x["outside_writes"]) for x in sec)
            calls = sum(x["tool_calls"] for x in sec) / max(len(sec), 1)
            secs = sum(x["seconds"] for x in sec) / max(len(sec), 1)
            passed = sum(r["passed"] for r in runs)
            cells.append(f"{passed}/{n}" + (f" ({n - len(runs)} invalid)" if n != len(runs) else ""))
            extra_cells.append(f"{extra}/{unreq}/{calls:.1f}/{secs:.0f}")
            t = totals[label]
            t[0] += passed; t[1] += n; t[2] += extra; t[3] += unreq; t[4] += sum(x["tool_calls"] for x in sec); t[5] += len(sec)
        print(" | ".join([case] + cells + extra_cells))
    for label, (p, n, e, u, c, v) in totals.items():
        print(f"{label}: {p}/{n} pass; runs with extra files {e}; runs with unrequested tools {u}; "
              f"mean tool calls {c / max(v, 1):.2f}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--label")
    ap.add_argument("--report", help="comma-separated labels to compare instead of running")
    ap.add_argument("--binary", default=shutil.which("cortex") or "cortex")
    ap.add_argument("--reminder", choices=("on", "off"), default="off")
    ap.add_argument("--runs", type=int, default=5)
    ap.add_argument("--cases", default=",".join(CASES))
    args = ap.parse_args()
    if args.report:
        report(args.report.split(","))
        return
    if not args.label:
        ap.error("--label is required unless --report is given")
    cases = args.cases.split(",")
    out = OUT_ROOT / args.label
    out.mkdir(parents=True, exist_ok=True)
    results = out / "results.jsonl"
    done = set()
    if results.exists():
        done = {(r["case"], r["run"]) for r in map(json.loads, results.read_text().splitlines())}
    reminder = args.reminder == "on"
    set_block(reminder)
    try:
        for run in range(1, args.runs + 1):
            for case in cases:
                if (case, run) in done:
                    continue
                rec = run_case(args.binary, case, run, out, reminder)
                with results.open("a") as f:
                    f.write(json.dumps(rec) + "\n")
                print(f"{args.label} {case} run {run}: {'PASS' if rec['passed'] else 'FAIL'}"
                      f"{'' if rec['valid'] else ' (invalid)'} {rec.get('decoy') or ''} | {rec['reason']} | {rec['durations']}",
                      flush=True)
    finally:
        set_block(False)
    summary = {}
    for r in map(json.loads, results.read_text().splitlines()):
        s = summary.setdefault(r["case"], [0, 0, 0])
        s[0] += r["passed"]
        s[1] += 1
        s[2] += not r["valid"]
    for case in cases:
        p, n, inv = summary.get(case, [0, 0, 0])
        print(f"{case}: {p}/{n} pass, {inv} invalid")


if __name__ == "__main__":
    main()
