#!/usr/bin/env python3
"""Unit tests for beehive (tools/beehive/beehive.py) that need no LLM or login:
the console wrapper, prompts, and the claude-code / grok CLI backends driven
by fake CLI executables (stream parsing, tool-use watchdog, errors, cancel),
and the network door for paired machines (pairing, the proof, sealed lines,
limits).

    python3 tests/test_beehive.py        (part of `make check`)
"""

import base64 as b64
import hashlib
import json
import socket
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


class Client:
    """The machine's side of the network link, as the OS does it (beenet/src/hive.rs)."""

    def __init__(self, port: int, name: str, key: str, auth=None, timeout: float = 5):
        self.sock = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        self.buf = b""
        self.sent = self.received = 0
        a = "00112233445566778899aabbccddeeff"
        self.put({"type": "hello", "name": name, "nonce": a, "version": "t", "build": "1"})
        self.challenge = self.get()
        nonce = self.challenge["nonce"]
        k = b.key_bytes(key)
        self.put({"type": "auth", "mac": b.proof(k, "os", a, nonce) if auth is None else auth})
        self.welcome = self.get()  # None: the door stayed shut
        self.proved = bool(self.welcome) and self.welcome.get("mac") == b.proof(k, "hive", nonce, a)
        self.session = b.session_key(k, a, nonce)

    def put(self, msg):
        self.raw(json.dumps(msg))

    def raw(self, line: str):
        self.sock.sendall(line.encode() + b"\n")

    def line(self):
        """The next line as text, or None when beehive closed the connection."""
        try:
            while b"\n" not in self.buf:
                chunk = self.sock.recv(4096)
                if not chunk:
                    return None
                self.buf += chunk
        except OSError:
            return None
        raw, self.buf = self.buf.split(b"\n", 1)
        return raw.decode()

    def get(self):
        line = self.line()
        return json.loads(line) if line else None

    def send(self, msg, n=None):
        self.sent += 1
        self.raw(b.seal(self.session, "o", n or self.sent, json.dumps(msg, ensure_ascii=False)))

    def recv(self):
        line = self.line()
        if line is None:
            return None
        self.received += 1
        text = b.unseal(self.session, "h", self.received, line)
        assert text is not None, f"beehive sent a line that is not genuine: {line}"
        return json.loads(text)

    def close(self):
        self.sock.close()


class Network(unittest.TestCase):
    """beehive --listen: only paired machines get in, and only genuine lines are served."""

    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.state = b.State(Path(self.tmp.name), 100)
        self.assertEqual(b.manage_machines(self.state, ["pair", "hp"]), 0)
        self.key = self.state.machines()["hp"]["key"]
        self.hive = b.Hive(b.FakeBackend(), self.state, "README", 1)
        self.listener = b.Listener("127.0.0.1:0", self.hive)
        self.listener.start()
        self.port = self.listener.port
        self.clients = []

    def tearDown(self):
        for c in self.clients:
            c.close()
        self.listener.close()
        self.tmp.cleanup()

    def client(self, name="hp", key=None, **kw):
        c = Client(self.port, name, key or self.key, **kw)
        self.clients.append(c)
        return c

    def signals(self):
        f = Path(self.tmp.name) / "signals.jsonl"
        return [json.loads(l) for l in f.read_text().splitlines()] if f.exists() else []

    def test_the_two_sides_agree_on_fixed_values(self):
        # The same values are in beenet/src/hive.rs: a change on one side fails a test.
        k = b.key_bytes("ABCDE-fghij-kmnpq-rstuv")
        self.assertEqual(k, b"abcdefghijkmnpqrstuv")
        a, n = "00112233445566778899aabbccddeeff", "ffeeddccbbaa99887766554433221100"
        self.assertEqual(b.proof(k, "os", a, n), "c6a12e0fb9bfc1849ceef735bb0e27218e7150f4366bd88dab638b76d0ccb894")
        self.assertEqual(b.proof(k, "hive", n, a), "d97922ca4948a5f094f7f3b6da1f0061ef6ce2b77e4e1b5ede14b0946df87b85")
        session = b.session_key(k, a, n)
        self.assertEqual(session.hex(), "226f0e56f7988da09c521f72d137c85112110a73b3671ed26bdf2f0127f8bb3b")
        self.assertEqual(b.seal(session, "o", 1, '{"type":"ping"}'), '69b8381eba22110a9a2cb3cef051f20d 1 {"type":"ping"}')
        line = b.seal(session, "h", 7, '{"type":"line","id":3,"text":"héllo"}')
        self.assertEqual(line, '8c430b21a0d327d623ac6ef6d8369495 7 {"type":"line","id":3,"text":"héllo"}')
        self.assertEqual(b.unseal(session, "h", 7, line), '{"type":"line","id":3,"text":"héllo"}')
        for wrong in (line.replace("héllo", "hello"), line.replace(" 7 ", " 8 "), "0" + line[1:], line[33:], ""):
            self.assertIsNone(b.unseal(session, "h", 7, wrong))
        self.assertIsNone(b.unseal(session, "o", 7, line))  # the other direction
        self.assertIsNone(b.unseal(session, "h", 6, line))  # another place in the row
        for bad in ("", "abcde", "abcde-fghij-klmno-pqrst", "abcde-fghij-kmnpq-rstu", "abcde-fghij-kmnpq-rstuvw"):
            self.assertIsNone(b.key_bytes(bad))

    def test_pairing(self):
        key = self.key
        self.assertRegex(key, r"^[a-z2-9]{5}(-[a-z2-9]{5}){3}$")
        self.assertIsNotNone(b.key_bytes(key))
        mode = (Path(self.tmp.name) / "machines.json").stat().st_mode & 0o777
        self.assertEqual(mode, 0o600)
        self.assertEqual(b.manage_machines(self.state, ["pair", "Bad Name"]), 2)
        self.assertEqual(b.manage_machines(self.state, ["pair", "x" * 17]), 2)
        self.assertEqual(b.manage_machines(self.state, ["nonsense"]), 2)
        self.assertEqual(b.manage_machines(self.state, ["unpair", "nobody"]), 1)
        self.assertEqual(b.manage_machines(self.state, ["pair", "hp"]), 0)  # again: a new key
        self.assertNotEqual(self.state.machines()["hp"]["key"], key)
        self.assertFalse(self.client(key=key).welcome)  # the old key is worth nothing now
        self.assertEqual(b.manage_machines(self.state, ["unpair", "hp"]), 0)
        self.assertEqual(self.state.machines(), {})
        self.assertEqual(b.manage_machines(self.state, ["machines"]), 0)

    def test_a_paired_machine_is_served(self):
        c = self.client()
        self.assertTrue(c.proved)  # beehive proved itself too
        self.assertEqual(c.welcome["backend"], "fake")
        self.assertNotIn(self.key, json.dumps(c.challenge) + json.dumps(c.welcome))
        c.send({"type": "ping"})
        self.assertEqual(c.recv(), {"type": "pong"})
        c.send({"type": "ask", "id": 4, "text": "what is this?", "cwd": "/"})
        self.assertEqual(c.recv(), {"type": "ack", "id": 4})
        lines = []
        while (m := c.recv())["type"] == "line":
            lines.append(m["text"])
        self.assertEqual(m, {"type": "done", "id": 4})
        self.assertIn("Question: what is this?", lines)
        c.send({"type": "signal", "kind": "wish", "text": "a banner", "build": "33"})
        c.send({"type": "ping"})
        self.assertEqual(c.recv(), {"type": "pong"})  # the signal before it was handled
        self.assertEqual([(s["target"], s["kind"], s["text"]) for s in self.signals()], [("hp", "wish", "a banner")])
        self.assertIsNotNone(self.state.machines()["hp"]["seen"])
        exchange = json.loads((Path(self.tmp.name) / "exchanges.jsonl").read_text().splitlines()[0])
        self.assertEqual((exchange["target"], exchange["outcome"]), ("hp", "done"))

    def test_the_door_stays_shut_without_the_key(self):
        self.assertFalse(self.client(key="abcde-fghij-kmnpq-rstuv").welcome)
        self.assertFalse(self.client(auth="héllo").welcome)
        self.assertFalse(self.client(auth=17).welcome)
        # An unknown machine gets a challenge too, and then the same silence.
        stranger = self.client(name="nobody")
        self.assertEqual(stranger.challenge["type"], "challenge")
        self.assertFalse(stranger.welcome)
        self.assertEqual(self.signals(), [])

    def test_nothing_is_served_before_the_proof(self):
        for first in ('{"type":"ask","id":1,"text":"hi"}', '{"type":"signal","kind":"wish","text":"x"}', "garbage",
                      '{"type":"hello","name":"hp","nonce":"short"}', '{"type":"hello","name":7,"nonce":"' + "a" * 32 + '"}',
                      '{"type":"hello","name":"hp","nonce":"' + "x" * 600 + '"}', "[1,2,3]"):
            sock = socket.create_connection(("127.0.0.1", self.port), timeout=5)
            sock.sendall(first.encode() + b"\n")
            try:
                self.assertEqual(sock.recv(4096), b"", first)  # closed, not a word
            except ConnectionResetError:
                pass  # closed with our line still unread: just as good
            sock.close()
        self.assertEqual(self.signals(), [])
        self.assertFalse((Path(self.tmp.name) / "exchanges.jsonl").exists())

    def test_lines_that_are_not_genuine_end_the_connection(self):
        signal = {"type": "signal", "kind": "wish", "text": "x"}
        tampered = lambda c: c.raw(b.seal(c.session, "o", 1, json.dumps(signal)).replace('"x"', '"y"'))
        unsealed = lambda c: c.raw(json.dumps(signal))
        skipped = lambda c: c.send(signal, n=2)
        wrong_way = lambda c: c.raw(b.seal(c.session, "h", 1, json.dumps(signal)))
        too_long = lambda c: c.raw("x" * (b.MAX_LINE + 100))
        for attack in (tampered, unsealed, skipped, wrong_way, too_long):
            c = self.client()
            attack(c)
            self.assertIsNone(c.recv())
        # A line sent twice: the first is taken, the copy is not.
        c = self.client()
        line = b.seal(c.session, "o", 1, json.dumps(signal))
        c.raw(line)
        c.raw(line)
        self.assertIsNone(c.recv())
        self.assertEqual(len(self.signals()), 1)

    def test_wrong_tries_lock_the_address_out(self):
        for _ in range(b.WRONG_TRIES):
            self.assertFalse(self.client(auth="00" * 32).welcome)
        sock = socket.create_connection(("127.0.0.1", self.port), timeout=5)
        self.clients.append(sock)
        self.assertEqual(sock.recv(4096), b"")  # not even a challenge, also with the right key
        with self.assertRaises((OSError, TypeError, ValueError)):
            self.client()

    def test_each_machine_has_its_own_share_of_the_budget(self):
        self.state.max_per_machine = 2
        b.manage_machines(self.state, ["pair", "nuc"])
        hp, nuc = self.client(), self.client("nuc", self.state.machines()["nuc"]["key"])

        def ask(c, rid):
            c.send({"type": "ask", "id": rid, "text": "hi"})
            while (m := c.recv())["type"] in ("ack", "line"):
                pass
            return m["type"]

        self.assertEqual([ask(hp, 1), ask(hp, 2), ask(hp, 3)], ["done", "done", "error"])
        self.assertEqual(ask(nuc, 1), "done")  # the other machine still has its share
        self.assertTrue(self.state.take_request())  # and so have the serial links
        usage = json.loads((Path(self.tmp.name) / "usage.json").read_text())
        self.assertEqual((usage["requests"], usage["machines"]), (4, {"hp": 2, "nuc": 1}))
        self.state.max_per_day = 4
        self.assertEqual(ask(nuc, 2), "error")  # the total for the day counts for everyone

    def test_a_new_connection_replaces_the_old_one(self):
        first = self.client()
        second = self.client()
        self.assertIsNone(first.recv())
        second.send({"type": "ping"})
        self.assertEqual(second.recv(), {"type": "pong"})

    def test_a_kernel_on_offer_is_handed_out_in_pieces(self):
        kernel = Path(self.tmp.name) / "k.elf"
        data = b"\x7fELF" + bytes(range(256)) * 60  # 15 KiB: several pieces
        kernel.write_bytes(data)
        c = self.client()
        c.send({"type": "update-check", "id": 1, "build": "7"})
        self.assertEqual(c.recv(), {"type": "update", "id": 1, "none": True})  # nothing on offer
        self.assertEqual(b.offer_kernel(self.state, ["offer", str(kernel)], 8), 0)
        c.send({"type": "update-check", "id": 2, "build": "8"})
        self.assertEqual(c.recv(), {"type": "update", "id": 2, "none": True})  # it has that build
        c.send({"type": "update-check", "id": 3, "build": "7"})
        offer = c.recv()
        self.assertEqual((offer["build"], offer["size"]), (8, len(data)))
        got = b""
        while len(got) < offer["size"]:
            c.send({"type": "update-chunk", "id": 4, "offset": len(got)})
            piece = c.recv()
            self.assertEqual((piece["type"], piece["offset"]), ("update-data", len(got)))
            got += b64.b64decode(piece["data"])
        self.assertEqual(got, data)
        self.assertEqual(hashlib.sha256(got).hexdigest(), offer["sha256"])
        # Past the end, a negative offset, a missing offset: empty or no answer, never a crash.
        c.send({"type": "update-chunk", "id": 5, "offset": 10 ** 9})
        self.assertEqual(c.recv()["data"], "")
        c.send({"type": "update-chunk", "id": 6, "offset": -5})
        self.assertEqual(b64.b64decode(c.recv()["data"])[:4], b"\x7fELF")
        c.send({"type": "update-chunk", "id": 7})
        c.send({"type": "ping"})
        self.assertEqual(c.recv(), {"type": "pong"})
        # Not a kernel, a missing file, taking the offer away.
        junk = Path(self.tmp.name) / "junk"
        junk.write_bytes(b"hello")
        self.assertEqual(b.offer_kernel(self.state, ["offer", str(junk)], 9), 2)
        self.assertEqual(b.offer_kernel(self.state, ["offer", "/nonexistent"], 9), 2)
        self.assertEqual(self.state.offer()["build"], 8)
        self.assertEqual(b.offer_kernel(self.state, ["offer", "none"], None), 0)
        self.assertIsNone(self.state.offer())

    def test_control_requests_reach_the_machine_and_answers_come_back(self):
        control = b.Control(self.hive)
        control.start()
        inbox, out = Path(self.tmp.name) / "control" / "in", Path(self.tmp.name) / "control" / "out"

        def ask(rid, machine, **fields):
            (inbox / f"{rid}.json").write_text(json.dumps({"rid": rid, "machine": machine, **fields}))

        def answer(rid):
            deadline = time.monotonic() + 3
            while not (out / f"{rid}.done").exists():
                self.assertLess(time.monotonic(), deadline, f"no answer to {rid}")
                time.sleep(0.02)
            pieces = [json.loads(l) for l in (out / f"{rid}.jsonl").read_text().splitlines()]
            for f in (out / f"{rid}.jsonl", out / f"{rid}.done"):
                f.unlink()
            return pieces

        # Nobody connected under that name: said at once.
        ask("r1", "hp", type="run", cmd="ls")
        self.assertIn("not connected", answer("r1")[0]["error"])
        c = self.client()
        ask("r2", "hp", type="run", cmd="echo hi", timeout=5)
        self.assertEqual(c.recv(), {"rid": "r2", "type": "run", "cmd": "echo hi", "timeout": 5})
        c.send({"type": "ran", "rid": "r2", "text": "hi\n"})
        c.send({"type": "ran", "rid": "r2", "done": True})
        self.assertEqual([p.get("text") for p in answer("r2")], ["hi\n", None])
        # Keys for the window that has the focus go the same way.
        ask("r4", "hp", type="keys", keys="ls{enter}")
        self.assertEqual(c.recv(), {"rid": "r4", "type": "keys", "keys": "ls{enter}"})
        c.send({"type": "ran", "rid": "r4", "text": "3 keys typed\n", "done": True})
        self.assertEqual([p.get("text") for p in answer("r4")], ["3 keys typed\n"])
        # Requests that are not ours to send, and answers with a name that is no file name.
        ask("r3", "hp", type="shutdown-the-host")
        (inbox / "bad.json").write_text("{not json")
        c.send({"type": "ran", "rid": "../../escape", "done": True})
        c.send({"type": "ping"})
        self.assertEqual(c.recv(), {"type": "pong"})
        self.assertFalse(list(Path(self.tmp.name).parent.glob("escape*")))
        self.assertEqual(sorted(p.name for p in out.iterdir()), [])  # nothing was written for any of those

    def test_a_slow_stranger_is_dropped(self):
        old, b.PROOF_SECONDS = b.PROOF_SECONDS, 0.3
        try:
            sock = socket.create_connection(("127.0.0.1", self.port), timeout=5)
            self.clients.append(sock)
            started = time.monotonic()
            self.assertEqual(sock.recv(4096), b"")
            self.assertLess(time.monotonic() - started, 3)
        finally:
            b.PROOF_SECONDS = old


if __name__ == "__main__":
    unittest.main(verbosity=1)
