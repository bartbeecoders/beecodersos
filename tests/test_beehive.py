#!/usr/bin/env python3
"""Unit tests for beehive (tools/beehive/beehive.py) that need no LLM or login:
the console wrapper, prompts, and the claude-code / grok CLI backends driven
by fake CLI executables (stream parsing, tool-use watchdog, errors, cancel).

    python3 tests/test_beehive.py        (part of `make check`)
"""

import json
import stat
import sys
import tempfile
import threading
import time
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools" / "beehive"))
import beehive as b  # noqa: E402


def fake_cli(dir: Path, name: str, events: list, exit_code: int = 0, stderr: str = "", sleep: float = 0) -> str:
    """Write an executable that prints `events` as JSON lines (and its argv to argv.json)."""
    path = dir / name
    path.write_text(f"""#!{sys.executable}
import json, sys, time
json.dump(sys.argv[1:], open({str(dir / (name + '.argv.json'))!r}, "w"))
sys.stdin.read()
time.sleep({sleep})
for ev in {events!r}:
    print(json.dumps(ev), flush=True)
sys.stderr.write({stderr!r})
sys.exit({exit_code})
""")
    path.chmod(path.stat().st_mode | stat.S_IEXEC)
    return str(path)


def delta(text):
    return {"type": "stream_event", "event": {"type": "content_block_delta", "index": 0,
                                              "delta": {"type": "text_delta", "text": text}}}


class CliBackends(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.dir = Path(self.tmp.name)

    def tearDown(self):
        self.tmp.cleanup()

    def run_backend(self, backend, cancel=None):
        return "".join(backend.stream("SYSTEM", "PROMPT", cancel or threading.Event()))

    def argv(self, name):
        return json.loads((self.dir / f"{name}.argv.json").read_text())

    def test_claude_code_streams_text_without_tools(self):
        cmd = fake_cli(self.dir, "claude", [
            {"type": "system", "subtype": "init", "model": "claude-opus-5-5", "tools": []},
            delta("Hello "), delta("bee"),
            {"type": "result", "subtype": "success", "is_error": False, "result": "Hello bee"}])
        be = b.ClaudeCodeBackend(cmd, None, "low", self.dir / "work")
        self.assertEqual(self.run_backend(be), "Hello bee")
        self.assertEqual(be.model, "claude-opus-5-5")
        argv = self.argv("claude")
        # Tools, MCP servers and user settings must be switched off.
        self.assertEqual(argv[argv.index("--tools") + 1], "")
        self.assertEqual(argv[argv.index("--setting-sources") + 1], "")
        self.assertIn("--strict-mcp-config", argv)
        self.assertEqual(argv[argv.index("--system-prompt") + 1], "SYSTEM")
        self.assertEqual(argv[argv.index("--effort") + 1], "low")

    def test_claude_code_tool_use_is_stopped(self):
        cmd = fake_cli(self.dir, "claude", [delta("Sure, running it"), {
            "type": "stream_event", "event": {"type": "content_block_start", "index": 1,
                                              "content_block": {"type": "tool_use", "name": "Bash"}}},
            delta("SHOULD NOT ARRIVE")])
        be = b.ClaudeCodeBackend(cmd, None, None, self.dir / "work")
        with self.assertRaisesRegex(b.BackendError, r"tried to use a tool \(Bash\)"):
            self.run_backend(be)

    def test_claude_code_error_result(self):
        cmd = fake_cli(self.dir, "claude", [{"type": "result", "is_error": True, "result": "Not logged in"}], 1)
        be = b.ClaudeCodeBackend(cmd, None, None, self.dir / "work")
        with self.assertRaisesRegex(b.BackendError, "Not logged in"):
            self.run_backend(be)

    def test_grok_streams_text_and_blocks_tools(self):
        cmd = fake_cli(self.dir, "grok", [
            {"type": "available_commands", "tools": []},
            {"type": "tool_call", "toolName": "update_goal"},  # harmless, allowed
            {"type": "text", "data": "Use "}, {"type": "text", "data": "mkdir."},
            {"type": "end", "stopReason": "end_turn"}])
        be = b.GrokBackend(cmd, "grok-x", None, self.dir / "work")
        self.assertEqual(self.run_backend(be), "Use mkdir.")
        argv = self.argv("grok")
        self.assertIn("run_terminal_command", argv[argv.index("--disallowed-tools") + 1])
        self.assertIn("use_tool", argv[argv.index("--disallowed-tools") + 1])
        self.assertEqual(argv[argv.index("--deny") + 1], "MCPTool(*)")
        self.assertEqual(argv[argv.index("-m") + 1], "grok-x")

        cmd = fake_cli(self.dir, "grok2", [{"type": "tool_call", "toolName": "run_terminal_command"},
                                           {"type": "text", "data": "pwned"}])
        with self.assertRaisesRegex(b.BackendError, r"run_terminal_command"):
            self.run_backend(b.GrokBackend(cmd, None, None, self.dir / "work"))

    def test_grok_no_answer(self):
        cmd = fake_cli(self.dir, "grok", [{"type": "end", "stopReason": "max_turn_requests"}], 1,
                       "Error: max turns reached\n")
        with self.assertRaisesRegex(b.BackendError, "max turns reached|without an answer"):
            self.run_backend(b.GrokBackend(cmd, None, None, self.dir / "work"))

    def test_cancel_kills_the_cli(self):
        cmd = fake_cli(self.dir, "claude", [delta("late")], sleep=30)
        be = b.ClaudeCodeBackend(cmd, None, None, self.dir / "work")
        cancel = threading.Event()
        threading.Timer(0.3, cancel.set).start()
        t = time.monotonic()
        self.assertEqual(self.run_backend(be, cancel), "")
        self.assertLess(time.monotonic() - t, 5)


class Formatting(unittest.TestCase):
    def test_wrapper_strips_markdown_and_wraps(self):
        w = b.Wrapper()
        lines = w.feed("## Title\n**Bold** and `code`\n- " + "word " * 30 + "\n") + w.flush()
        self.assertEqual(lines[:2], ["Title", "Bold and code"])
        self.assertTrue(all(len(l) <= b.WIDTH for l in lines))
        self.assertTrue(lines[3].startswith("  "))  # list continuation is indented

    def test_prompts(self):
        hive = b.Hive(b.FakeBackend(), None, "README", 1)
        self.assertIn("README", hive.system)
        self.assertIn("Question: hi", hive.prompt({"type": "ask", "text": "hi", "cwd": "/"}))
        p = hive.prompt({"type": "explain", "cmd": "cat x", "error": "x: not found", "cwd": "/a"})
        self.assertIn("cat x", p)
        self.assertIn("x: not found", p)
        p = hive.prompt({"type": "explain", "crash": "panicked at shell.rs", "kind": "panic", "build": "9"})
        self.assertIn("crashed", p)


if __name__ == "__main__":
    unittest.main(verbosity=1)
