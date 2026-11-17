#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["anthropic"]
# ///
"""beehive: the host side of the BeeCodersOs AI bridge.

BeeCodersOs talks JSON lines over its second serial port (COM2). QEMU/libvirt
expose that port as a TCP server on localhost; beehive connects to it, answers
`ask` / `explain` requests with an LLM and stores the signals the OS sends
(wishes, feedback, missed commands, crashes) for the improvement loop.

The API key lives here, never in the guest.

Machines without such a serial port (real hardware) reach beehive over the
local network instead, when it is started with --listen. Only machines that
were paired may talk to it (see "machines on the network" in this file and
Docs/beehive-network-plan.html):

    beehive.py pair hp                 make a key for a machine called "hp"; prints what
                                       to type on that machine (`ai connect ...`)
    beehive.py unpair hp               take its key away
    beehive.py machines                paired machines and when each was last seen
    beehive.py --listen 192.168.1.10:4560 ...    also serve paired machines on that address

New kernels and remote control (for machines that allow it, see `update` and
`ai control` in the OS):

    beehive.py offer [KERNEL]          put a kernel ready (default: build/beecoders.elf); a machine
                                       fetches it with `update`, or by itself with `update auto on`
    beehive.py offer none              take the offer away
    tools/beectl.py run hp "ls /"      run a command on a machine and print what it shows
    tools/beectl.py screenshot hp out.ppm

The network link is not encrypted: people on the same network can read the
questions and answers, but cannot use beehive, pretend to be it, or change
what is sent.

    uv run tools/beehive/beehive.py                       # Claude (ANTHROPIC_API_KEY or `ant auth login`)
    python3 tools/beehive/beehive.py --backend openai \\
        --base-url http://localhost:11434/v1 --model llama3.2   # Ollama / LM Studio / OpenRouter
    python3 tools/beehive/beehive.py --backend claude-code    # the Claude Code CLI (your login)
    python3 tools/beehive/beehive.py --backend grok           # the grok CLI (your login)
    python3 tools/beehive/beehive.py --backend fake       # no LLM, for tests

The CLI backends run the coding agents as plain chat models: every tool, MCP
server and plugin is switched off, they run in an empty folder, and beehive
stops them if they try to use a tool anyway. Text typed in the VM must never be
able to run commands on the host.

Settings can also go in ~/.config/beehive/config.toml (same names as the
options, with underscores: backend = "openai", base_url = "...", ...).

State (default ~/.local/state/beehive/):
    signals.jsonl     wishes, feedback, missed commands and crashes from the OS
    exchanges.jsonl   every AI request with its answer and timing
    usage.json        requests per day (for --max-requests-per-day), also per machine
    offer.json, offer.elf   the kernel on offer to machines that ask for an update
    control/in, control/out   requests of tools/beectl.py to a machine, and the answers
    machines.json     the keys of paired machines (readable by you only)
"""

import argparse
import base64
import datetime as dt
import hashlib
import hmac
import json
import os
import re
import secrets
import shutil
import socket
import subprocess
import sys
import tempfile
import textwrap
import threading
import time
import tomllib
import urllib.error
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent.parent
DEFAULT_TARGETS = ["127.0.0.1:4555", "127.0.0.1:4556"]  # libvirt vm1, make run
WIDTH = 78  # the OS console is 80 columns

SYSTEM_PROMPT = """You are the built-in assistant of BeeCodersOs, a small hobby operating system \
written from scratch in Rust. People talk to you from its shell with the `ask` and `explain` commands.

Your answers appear on an 80-column text console:
- Plain text only. No Markdown: no **bold**, # headings, `backticks`, tables or code fences.
- Short: at most about 15 lines unless the user asks for more.
- Commands the user can type go on their own line, indented by two spaces.
- Never break lines yourself inside a sentence: write each paragraph as one line;
  the console wraps it to the screen width.

Base what you say about the OS on the reference below; it is the OS's own README. \
If BeeCodersOs cannot do something, say so plainly and suggest the user type \
`wish <what they want>` so the developers see the request. Do not invent commands.

<reference>
{readme}
</reference>"""

EXPLAIN_FAILURE = """The user ran this BeeShell command line:
  {cmd}
It failed with:
  {error}
Current directory: {cwd}
Explain briefly what went wrong and how to do it correctly."""

EXPLAIN_CRASH = """The BeeCodersOs kernel crashed during the previous boot (build b{build}, \
while running the command '{cmd}').
Crash report ({kind}): {crash}
Explain in plain words what probably happened, what it means for the user, \
and what a developer should look at in the kernel source."""


def log(msg: str):
    print(f"{dt.datetime.now():%H:%M:%S} beehive: {msg}", flush=True)


def now_iso() -> str:
    return dt.datetime.now(dt.UTC).strftime("%Y-%m-%dT%H:%M:%SZ")


# --- backends -----------------------------------------------------------------------

class BackendError(Exception):
    pass


class FakeBackend:
    """Deterministic answers without an LLM, for tests and demos."""
    name, model = "fake", "fake"

    def stream(self, system: str, prompt: str, cancel: threading.Event):
        if "slow" in prompt:
            for _ in range(200):  # 20 s, unless cancelled
                if cancel.is_set():
                    return
                time.sleep(0.1)
        text = ("BeeHive fake backend (no LLM).\n" + prompt + "\n"
                "This last line is deliberately long, so that it has to be wrapped onto more than "
                "one line on the 80-column console of BeeCodersOs.")
        for i in range(0, len(text), 20):
            yield text[i:i + 20]


class AnthropicBackend:
    """Claude through the official Anthropic SDK."""
    name = "anthropic"

    def __init__(self, model: str, effort: str):
        try:
            import anthropic
        except ImportError:
            sys.exit("the anthropic package is missing: run `uv run tools/beehive/beehive.py` "
                     "(installs it automatically) or `pip install anthropic`")
        self.anthropic = anthropic
        self.client = anthropic.Anthropic()
        self.model, self.effort = model, effort
        if not (os.environ.get("ANTHROPIC_API_KEY") or os.environ.get("ANTHROPIC_AUTH_TOKEN")
                or any((Path.home() / ".config/anthropic").glob("*"))):
            log("warning: no Anthropic credentials found (ANTHROPIC_API_KEY or `ant auth login`); "
                "requests will fail")

    def stream(self, system: str, prompt: str, cancel: threading.Event):
        a = self.anthropic
        try:
            with self.client.beta.messages.stream(
                model=self.model,
                max_tokens=8000,
                # Stable system prompt (no timestamps) so the README part is cached.
                system=[{"type": "text", "text": system, "cache_control": {"type": "ephemeral"}}],
                messages=[{"role": "user", "content": prompt}],
                output_config={"effort": self.effort},
                # If a safety classifier declines, retry on a fallback model.
                betas=["server-side-fallback-2026-07-01"],
                fallbacks="default",
            ) as stream:
                for text in stream.text_stream:
                    if cancel.is_set():
                        return
                    yield text
                final = stream.get_final_message()
        except a.AuthenticationError:
            raise BackendError("API key missing or invalid on the host "
                               "(set ANTHROPIC_API_KEY or run `ant auth login`)")
        except a.RateLimitError:
            raise BackendError("rate limited by the API, try again in a minute")
        except a.APIStatusError as e:
            raise BackendError(f"API error {e.status_code}: {e.message}"[:200])
        except a.APIConnectionError:
            raise BackendError("the host cannot reach the Anthropic API")
        except TypeError as e:
            if "authentication" not in str(e):
                raise
            raise BackendError("no Anthropic credentials on the host "
                               "(set ANTHROPIC_API_KEY or run `ant auth login`)")
        if final.stop_reason == "refusal":
            yield "\n(The model declined to answer this.)"
        elif final.stop_reason == "max_tokens":
            yield "\n(answer cut off)"


class OpenAIBackend:
    """Any OpenAI-compatible chat endpoint: Ollama, LM Studio, OpenRouter, ..."""
    name = "openai"

    def __init__(self, model: str, base_url: str, api_key: str | None):
        self.model, self.base_url, self.api_key = model, base_url.rstrip("/"), api_key

    def stream(self, system: str, prompt: str, cancel: threading.Event):
        body = json.dumps({
            "model": self.model, "stream": True,
            "messages": [{"role": "system", "content": system}, {"role": "user", "content": prompt}],
        }).encode()
        headers = {"Content-Type": "application/json"}
        if self.api_key:
            headers["Authorization"] = f"Bearer {self.api_key}"
        req = urllib.request.Request(f"{self.base_url}/chat/completions", body, headers)
        try:
            resp = urllib.request.urlopen(req, timeout=120)
        except urllib.error.HTTPError as e:
            raise BackendError(f"HTTP {e.code} from {self.base_url}: {e.read()[:150].decode(errors='replace')}")
        except (urllib.error.URLError, OSError) as e:
            raise BackendError(f"cannot reach {self.base_url}: {getattr(e, 'reason', e)}")
        with resp:
            for raw in resp:
                if cancel.is_set():
                    return
                line = raw.decode(errors="replace").strip()
                if not line.startswith("data:") or line == "data: [DONE]":
                    continue
                try:
                    delta = json.loads(line[5:])["choices"][0].get("delta", {})
                except (ValueError, KeyError, IndexError):
                    continue
                if delta.get("content"):
                    yield delta["content"]


class CliBackend:
    """Base for coding-agent CLIs run in non-interactive, streaming-JSON mode."""
    name = exe = ""
    timeout = 180  # seconds per answer

    def __init__(self, command: str | None, model: str | None, effort: str | None, workdir: Path):
        self.command = command or shutil.which(self.exe)
        if not self.command:
            sys.exit(f"`{self.exe}` not found on PATH (or pass --command /path/to/{self.exe})")
        self.model = model or "default"
        self.model_arg, self.effort = model, effort
        # An empty folder: no project instructions, configs or files to read.
        self.workdir = workdir
        workdir.mkdir(parents=True, exist_ok=True)

    def events(self, argv: list[str], stdin: str, cancel: threading.Event, env: dict | None = None):
        """Run the CLI and yield its JSON events. Killed on cancel or timeout."""
        with tempfile.TemporaryFile("w+") as err:
            proc = subprocess.Popen(argv, cwd=self.workdir, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                    stderr=err, text=True, env={**os.environ, **(env or {})})
            self.proc = proc
            deadline = time.monotonic() + self.timeout

            def watchdog():
                while proc.poll() is None:
                    if cancel.wait(0.2) or time.monotonic() > deadline:
                        proc.kill()
                        return
            threading.Thread(target=watchdog, daemon=True).start()
            try:
                proc.stdin.write(stdin)
                proc.stdin.close()
                for line in proc.stdout:
                    try:
                        obj = json.loads(line)
                    except ValueError:
                        continue
                    if isinstance(obj, dict):
                        yield obj
            finally:
                if proc.poll() is None:
                    proc.kill()
                proc.wait()
                proc.stdout.close()
            if proc.returncode != 0 and not cancel.is_set():
                err.seek(0)
                detail = err.read().strip().splitlines()[-1:] or [f"exit status {proc.returncode}"]
                if time.monotonic() > deadline:
                    detail = [f"no answer within {self.timeout}s"]
                raise BackendError(f"{self.exe}: {detail[0]}"[:200])

    def blocked(self, tool: str):
        self.proc.kill()
        raise BackendError(f"{self.exe} tried to use a tool ({tool}); beehive stopped it")


class ClaudeCodeBackend(CliBackend):
    """The Claude Code CLI (`claude -p`), using your Claude Code login."""
    name, exe = "claude-code", "claude"

    def stream(self, system: str, prompt: str, cancel: threading.Event):
        argv = [self.command, "-p", "--output-format", "stream-json", "--verbose",
                "--include-partial-messages",
                "--system-prompt", system,
                "--tools", "",                 # no built-in tools at all
                "--strict-mcp-config",         # no MCP servers
                "--setting-sources", "",       # no user/project settings, hooks or plugins
                "--disable-slash-commands", "--no-session-persistence"]
        if self.model_arg:
            argv += ["--model", self.model_arg]
        if self.effort:
            argv += ["--effort", self.effort]
        for ev in self.events(argv, prompt, cancel):
            kind = ev.get("type")
            if kind == "stream_event":
                e = ev.get("event", {})
                block = e.get("content_block") or {}
                if e.get("type") == "content_block_start" and block.get("type") in ("tool_use", "server_tool_use"):
                    self.blocked(block.get("name", "?"))
                delta = e.get("delta") or {}
                if delta.get("type") == "text_delta":
                    yield delta.get("text", "")
                if delta.get("stop_reason") == "refusal":
                    yield "\n(The model declined to answer this.)"
            elif kind == "system" and ev.get("subtype") == "init" and ev.get("model"):
                self.model = ev["model"]
            elif kind == "result" and ev.get("is_error"):
                raise BackendError(f"claude: {ev.get('result') or ev.get('subtype')}"[:200])


class GrokBackend(CliBackend):
    """The grok CLI (`grok -p`), using your grok login."""
    name, exe = "grok-cli", "grok"
    # Built-in tools of grok 1.0.x; use_tool/search_tool are also how it reaches
    # MCP servers. Tools added by newer versions are caught by the watchdog.
    BUILTIN_TOOLS = ("run_terminal_command,read_file,search_replace,list_dir,grep,kill_command_or_subagent,"
                     "todo_write,get_command_or_subagent_output,spawn_subagent,scheduler_create,scheduler_delete,"
                     "scheduler_list,monitor,search_tool,use_tool,workflow,enter_plan_mode,exit_plan_mode,"
                     "ask_user_question,send_feedback,image_gen,image_edit,image_to_video,reference_to_video,write")
    HARMLESS = {"update_goal"}  # grok's internal progress note

    def stream(self, system: str, prompt: str, cancel: threading.Event):
        argv = [self.command, "-p", prompt, "--output-format", "streaming-json",
                "--system-prompt-override", system,
                "--disallowed-tools", self.BUILTIN_TOOLS,
                "--deny", "MCPTool(*)",             # deny always wins
                "--permission-mode", "dontAsk",     # anything not allowed is refused
                "--no-subagents", "--disable-web-search", "--max-turns", "3"]
        if self.model_arg:
            argv += ["-m", self.model_arg]
        if self.effort:
            argv += ["--reasoning-effort", self.effort]
        env = {"GROK_CLAUDE_MCPS_ENABLED": "0", "GROK_CURSOR_MCPS_ENABLED": "0", "GROK_WORKFLOWS": "0"}
        answered = False
        for ev in self.events(argv, "", cancel, env):
            kind = ev.get("type")
            if kind == "text":
                answered = True
                yield ev.get("data", "")
            elif kind == "tool_call" and ev.get("toolName") not in self.HARMLESS:
                self.blocked(ev.get("toolName", "?"))
            elif kind == "error":
                raise BackendError(f"grok: {ev.get('message') or ev}"[:200])
            elif kind == "end" and ev.get("stopReason") not in ("end_turn", None) and not answered:
                raise BackendError(f"grok stopped without an answer ({ev.get('stopReason')})")


# --- text formatting for the 80-column console ---------------------------------------

def plain(line: str) -> str:
    """Strip the Markdown the model may still produce."""
    line = re.sub(r"^\s{0,3}#{1,6}\s+", "", line)
    line = line.replace("**", "").replace("__", "")
    line = re.sub(r"`{1,3}", "", line)
    return line


class Wrapper:
    """Turns streamed text into complete, wrapped console lines."""

    def __init__(self):
        self.buf = ""

    def _wrap(self, line: str) -> list[str]:
        line = plain(line.rstrip())
        if not line.strip():
            return [""]
        indent = re.match(r"\s*(?:[-*]\s+|\d+[.)]\s+)?", line).group(0)
        return textwrap.wrap(line, WIDTH, subsequent_indent=" " * len(indent),
                             break_long_words=True, break_on_hyphens=False)

    def feed(self, text: str) -> list[str]:
        self.buf += text
        out = []
        while "\n" in self.buf:
            line, self.buf = self.buf.split("\n", 1)
            out += self._wrap(line)
        if len(self.buf) > 4 * WIDTH:  # a very long line: emit all but the last part
            parts = self._wrap(self.buf)
            out += parts[:-1]
            self.buf = parts[-1]
        return out

    def flush(self) -> list[str]:
        rest, self.buf = self.buf, ""
        return self._wrap(rest) if rest.strip() else []


# --- state ---------------------------------------------------------------------------

class State:
    def __init__(self, path: Path, max_per_day: int, max_per_machine: int | None = None):
        self.path, self.max_per_day = path, max_per_day
        # One machine on the network may not use up the day for all the others.
        self.max_per_machine = max_per_day if max_per_machine is None else max_per_machine
        path.mkdir(parents=True, exist_ok=True)
        self.lock = threading.Lock()

    def append(self, name: str, record: dict):
        with self.lock, open(self.path / name, "a") as f:
            f.write(json.dumps(record, ensure_ascii=False) + "\n")

    def take_request(self, machine: str | None = None) -> bool:
        """Count one request against today's budget (and, for a machine on the
        network, against that machine's share). False when over budget."""
        with self.lock:
            f = self.path / "usage.json"
            today = dt.date.today().isoformat()
            try:
                usage = json.loads(f.read_text())
            except (OSError, ValueError):
                usage = {}
            if not isinstance(usage, dict) or usage.get("date") != today:
                usage = {"date": today, "requests": 0}
            machines = usage.get("machines") if isinstance(usage.get("machines"), dict) else {}
            if usage["requests"] >= self.max_per_day:
                return False
            if machine is not None and machines.get(machine, 0) >= self.max_per_machine:
                return False
            usage["requests"] += 1
            if machine is not None:
                machines[machine] = machines.get(machine, 0) + 1
                usage["machines"] = machines
            f.write_text(json.dumps(usage))
            return True

    # Paired machines: {name: {"key": ..., "paired": time, "seen": time or None}}.

    def machines(self) -> dict:
        try:
            data = json.loads((self.path / "machines.json").read_text())
        except (OSError, ValueError):
            return {}
        return data if isinstance(data, dict) else {}

    def save_machines(self, machines: dict):
        f = self.path / "machines.json"
        tmp = f.with_suffix(".tmp")
        fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)  # the keys: for the owner only
        with os.fdopen(fd, "w") as out:
            out.write(json.dumps(machines, indent=1))
        os.replace(tmp, f)

    # The kernel on offer: {"build": N, "size": bytes, "sha256": hex}; the file is offer.elf.

    def offer(self) -> dict | None:
        try:
            offer = json.loads((self.path / "offer.json").read_text())
        except (OSError, ValueError):
            return None
        return offer if isinstance(offer, dict) and isinstance(offer.get("build"), int) else None

    def offer_chunk(self, offset: int, size: int) -> bytes:
        try:
            with open(self.path / "offer.elf", "rb") as f:
                f.seek(offset)
                return f.read(size)
        except OSError:
            return b""

    def machine_seen(self, name: str):
        with self.lock:
            machines = self.machines()
            if name in machines:
                machines[name]["seen"] = now_iso()
                self.save_machines(machines)


# --- machines on the network ---------------------------------------------------------------
#
# A machine and beehive share a key, made by `beehive.py pair NAME` and typed in on the
# machine once. It is never sent. On each connection both sides prove they know it:
#
#   machine: {"type":"hello","name":NAME,"nonce":A,...}       A, B: 32 hex digits, random
#   beehive: {"type":"challenge","nonce":B}
#   machine: {"type":"auth","mac":proof(key, "os", A, B)}
#   beehive: {"type":"welcome",...,"mac":proof(key, "hive", B, A)}
#
# After that every line is "MAC N JSON": N counts the lines of one direction from 1, and
# MAC is made from the direction, N and the JSON text with a key for this connection
# only. A line that was changed, repeated, left out or made up does not pass.
# The same code exists in the OS: beenet/src/hive.rs. Keep the two in step.

KEY_ALPHABET = "abcdefghijkmnpqrstuvwxyz23456789"  # no l, o, 0, 1: easy to read and type
DEFAULT_PORT = 4560
NONCE = re.compile(r"[0-9a-f]{32}")
MACHINE_NAME = re.compile(r"[a-z0-9][a-z0-9-]{0,15}")
MAX_LINE = 8192          # as in the OS (aibridge.rs)
MAX_HELLO_LINE = 512     # before the proof: short lines only
PROOF_SECONDS = 10       # a connection that has not proved itself by then is closed
MAX_CONNECTIONS = 8
WRONG_TRIES = 5          # from one address; then it is ignored for LOCKOUT_SECONDS
LOCKOUT_SECONDS = 60


def new_key() -> str:
    """100 random bits as four groups of five characters."""
    return "-".join("".join(secrets.choice(KEY_ALPHABET) for _ in range(5)) for _ in range(4))


def key_bytes(code: str) -> bytes | None:
    """The key as typed (dashes and capitals do not matter), or None if it cannot be one."""
    text = code.replace("-", "").lower()
    return text.encode() if len(text) == 20 and all(c in KEY_ALPHABET for c in text) else None


def proof(key: bytes, who: str, first: str, second: str) -> str:
    return hmac.new(key, f"{who} {first} {second}".encode(), "sha256").hexdigest()


def session_key(key: bytes, a: str, b: str) -> bytes:
    return hmac.new(key, f"session {a} {b}".encode(), "sha256").digest()


def seal(session: bytes, direction: str, n: int, text: str) -> str:
    """One line of a proved connection. direction: "o" from the OS, "h" from beehive."""
    mac = hmac.new(session, f"{direction} {n} {text}".encode(), "sha256").hexdigest()[:32]
    return f"{mac} {n} {text}"


def unseal(session: bytes, direction: str, n: int, line: str) -> str | None:
    """The JSON text of line number `n`, or None when the line is not genuine."""
    mac, _, rest = line.partition(" ")
    number, _, text = rest.partition(" ")
    if number != str(n) or not hmac.compare_digest(seal(session, direction, n, text).encode(), line.encode()):
        return None
    return text

# --- one connection to a VM serial port ---------------------------------------------------

class Link(threading.Thread):
    machine = None  # the name of a paired machine on the network (see Peer); None: a serial port

    def __init__(self, target: str, hive: "Hive"):
        super().__init__(daemon=True)
        self.target, self.hive = target, hive
        self.sock = None
        self.send_lock = threading.Lock()
        self.cancels: dict[int, threading.Event] = {}

    def run(self):
        host, port = self.target.rsplit(":", 1)
        announced = False
        while True:
            try:
                self.sock = socket.create_connection((host, int(port)), timeout=5)
            except OSError:
                if not announced:
                    log(f"waiting for a VM on {self.target}")
                    announced = True
                time.sleep(self.hive.retry)
                continue
            self.sock.settimeout(None)
            log(f"connected to {self.target}")
            self.hive.links[self.target] = self
            announced = False
            self.send({"type": "welcome", "backend": self.hive.backend.name, "model": self.hive.backend.model})
            try:
                self.serve()
            except OSError:
                pass
            log(f"disconnected from {self.target}")
            self.hive.links.pop(self.target, None)
            for ev in self.cancels.values():
                ev.set()
            time.sleep(self.hive.retry)

    def send(self, msg: dict):
        data = (json.dumps(msg, ensure_ascii=False) + "\n").encode()
        with self.send_lock:
            try:
                self.sock.sendall(data)
            except OSError:
                pass

    def serve(self):
        buf = b""
        while chunk := self.sock.recv(4096):
            buf += chunk
            while b"\n" in buf:
                raw, buf = buf.split(b"\n", 1)
                try:
                    msg = json.loads(raw.decode("utf-8", errors="replace"))
                except ValueError:
                    continue  # boot noise or a partial line
                if isinstance(msg, dict):
                    self.dispatch(msg)

    def dispatch(self, msg: dict):
        kind = msg.get("type")
        if kind == "hello":
            log(f"{self.target}: BeeCodersOs {msg.get('version')} b{msg.get('build')} says hello")
            self.send({"type": "welcome", "backend": self.hive.backend.name, "model": self.hive.backend.model})
        elif kind == "ping":  # "still there?" from a machine on the network
            self.send({"type": "pong"})
        elif kind == "update-check" and isinstance(msg.get("id"), int):
            # A machine asks whether there is a newer kernel for it.
            offer = self.hive.state.offer()
            answer = {"type": "update", "id": msg["id"]}
            if offer and str(offer["build"]) != str(msg.get("build")):
                answer.update(build=offer["build"], size=offer["size"], sha256=offer["sha256"])
                log(f"{self.target}: asks for an update, has b{msg.get('build')}: offering b{offer['build']}")
            else:
                answer["none"] = True
            self.send(answer)
        elif kind == "update-chunk" and isinstance(msg.get("id"), int) and isinstance(msg.get("offset"), int):
            data = self.hive.state.offer_chunk(max(0, msg["offset"]), UPDATE_CHUNK)
            self.send({"type": "update-data", "id": msg["id"], "offset": msg["offset"],
                       "data": base64.b64encode(data).decode()})
        elif kind in ("ran", "shot") and isinstance(msg.get("rid"), str):
            self.hive.control_answer(msg)  # the answer to a request of tools/beectl.py
        elif kind == "signal":
            record = {"received": now_iso(), "target": self.target,
                      **{k: msg.get(k) for k in ("kind", "text", "build", "t")}}
            self.hive.state.append("signals.jsonl", record)
            log(f"{self.target}: signal {msg.get('kind')}: {str(msg.get('text'))[:60]}")
        elif kind in ("ask", "explain") and isinstance(msg.get("id"), int):
            ev = threading.Event()
            self.cancels[msg["id"]] = ev
            threading.Thread(target=self.answer, args=(msg, ev), daemon=True).start()
        elif kind == "cancel":
            if ev := self.cancels.get(msg.get("id")):
                ev.set()

    def answer(self, msg: dict, cancel: threading.Event):
        rid, kind = msg["id"], msg["type"]
        self.send({"type": "ack", "id": rid})
        started = time.monotonic()
        lines, outcome = [], "done"
        try:
            if not self.hive.state.take_request(self.machine):
                raise BackendError(f"daily request budget reached ({self.hive.state.max_per_day}); "
                                   "raise --max-requests-per-day on the host")
            prompt = self.hive.prompt(msg)
            wrapper = Wrapper()
            for chunk in self.hive.backend.stream(self.hive.system, prompt, cancel):
                for line in wrapper.feed(chunk):
                    lines.append(line)
                    self.send({"type": "line", "id": rid, "text": line})
            if cancel.is_set():
                outcome = "cancelled"
            else:
                for line in wrapper.flush():
                    lines.append(line)
                    self.send({"type": "line", "id": rid, "text": line})
                self.send({"type": "done", "id": rid})
        except BackendError as e:
            outcome = f"error: {e}"
            self.send({"type": "error", "id": rid, "text": str(e)})
        except Exception as e:  # never let one request kill the link
            outcome = f"error: {e!r}"
            self.send({"type": "error", "id": rid, "text": f"beehive: {type(e).__name__}: {e}"[:200]})
        finally:
            self.cancels.pop(rid, None)
        secs = time.monotonic() - started
        log(f"{self.target}: {kind} #{rid}: {outcome} ({len(lines)} lines, {secs:.1f}s)")
        self.hive.state.append("exchanges.jsonl", {
            "t": now_iso(), "target": self.target, "backend": self.hive.backend.name,
            "model": self.hive.backend.model, "request": msg, "answer": "\n".join(lines),
            "outcome": outcome, "seconds": round(secs, 2)})


class Peer(Link):
    """One paired machine connected over the network. After the proof it is served
    exactly like a serial link (`dispatch`), with every line sealed."""

    def __init__(self, sock, name: str, session: bytes, hive: "Hive", on_close):
        super().__init__(name, hive)
        self.sock, self.machine, self.session, self.on_close = sock, name, session, on_close
        self.sent = self.received = 0

    def send(self, msg: dict):
        with self.send_lock:
            self.sent += 1
            line = seal(self.session, "h", self.sent, json.dumps(msg, ensure_ascii=False))
            try:
                self.sock.sendall(line.encode() + b"\n")
            except OSError:
                pass

    def run(self):
        log(f"{self.target}: connected over the network")
        self.hive.links[self.target] = self
        try:
            buf = b""
            while chunk := self.sock.recv(4096):
                buf += chunk
                while b"\n" in buf:
                    raw, buf = buf.split(b"\n", 1)
                    self.received += 1
                    text = unseal(self.session, "o", self.received, raw.decode("utf-8", errors="replace"))
                    if text is None:
                        raise ValueError("a line that is not genuine")
                    msg = json.loads(text)
                    if isinstance(msg, dict):
                        self.dispatch(msg)
                if len(buf) > MAX_LINE:
                    raise ValueError("a line that is too long")
        except (OSError, ValueError) as e:
            if isinstance(e, ValueError):
                log(f"{self.target}: closing the connection: {e}")
        self.close()
        for ev in self.cancels.values():
            ev.set()
        log(f"{self.target}: disconnected")
        if self.hive.links.get(self.target) is self:
            del self.hive.links[self.target]
        self.on_close(self)

    def close(self):
        try:
            self.sock.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self.sock.close()


class Listener(threading.Thread):
    """The door on the network: accepts connections and lets paired machines in."""

    def __init__(self, address: str, hive: "Hive"):
        super().__init__(daemon=True)
        host, _, port = address.rpartition(":")
        if not host:
            host, port = address, str(DEFAULT_PORT)
        self.hive = hive
        self.server = socket.create_server((host, int(port)))
        self.port = self.server.getsockname()[1]
        self.lock = threading.Lock()
        self.peers: dict[str, Peer] = {}
        self.open = 0                      # connections, proved or not
        self.wrong: dict[str, list] = {}   # address -> [wrong tries, ignored until]

    def run(self):
        while True:
            try:
                sock, (address, _) = self.server.accept()
            except OSError:
                return
            with self.lock:
                tries, until = self.wrong.get(address, [0, 0])
                refuse = self.open >= MAX_CONNECTIONS or time.monotonic() < until
                if not refuse:
                    self.open += 1
            if refuse:
                sock.close()
                continue
            threading.Thread(target=self.greet, args=(sock, address), daemon=True).start()

    def close(self):
        self.server.close()
        with self.lock:
            peers = list(self.peers.values())
        for peer in peers:
            peer.close()

    def greet(self, sock, address: str):
        """The proof. Ends with a running Peer, or a closed connection."""
        peer = None
        try:
            peer = self.prove(sock)
        except (OSError, ValueError):
            pass
        if peer is None:
            sock.close()
            with self.lock:
                self.open -= 1
                entry = self.wrong.setdefault(address, [0, 0])
                entry[0] += 1
                if entry[0] >= WRONG_TRIES:
                    self.wrong[address] = [0, time.monotonic() + LOCKOUT_SECONDS]
                    log(f"network: {WRONG_TRIES} wrong tries from one address: ignoring it for {LOCKOUT_SECONDS}s")
            return
        with self.lock:
            self.wrong.pop(address, None)
            old = self.peers.get(peer.machine)
            self.peers[peer.machine] = peer
        if old:
            old.close()  # one connection per machine: the new one replaces it
        self.hive.state.machine_seen(peer.machine)
        peer.start()

    def gone(self, peer: Peer):
        with self.lock:
            self.open -= 1
            if self.peers.get(peer.machine) is peer:
                del self.peers[peer.machine]

    def prove(self, sock) -> Peer | None:
        deadline = time.monotonic() + PROOF_SECONDS
        buf = b""

        def line() -> dict:
            nonlocal buf
            while b"\n" not in buf:
                left = deadline - time.monotonic()
                if left <= 0 or len(buf) > MAX_HELLO_LINE:
                    raise ValueError("too slow or too long")
                sock.settimeout(left)
                chunk = sock.recv(1024)
                if not chunk:
                    raise ValueError("closed")
                buf += chunk
            raw, buf = buf.split(b"\n", 1)
            msg = json.loads(raw.decode("utf-8", errors="replace"))
            if not isinstance(msg, dict):
                raise ValueError("not a message")
            return msg

        hello = line()
        name, a = hello.get("name"), hello.get("nonce")
        if hello.get("type") != "hello" or not isinstance(name, str) or not isinstance(a, str) or not NONCE.fullmatch(a):
            return None
        b = secrets.token_hex(16)
        sock.sendall(json.dumps({"type": "challenge", "nonce": b}).encode() + b"\n")
        auth = line()
        entry = self.hive.state.machines().get(name) if MACHINE_NAME.fullmatch(name) else None
        # An unknown name gets the same treatment with a key nobody has.
        key = key_bytes(entry["key"]) if entry else None
        key = key or secrets.token_bytes(20)
        mac = auth.get("mac")
        if auth.get("type") != "auth" or not isinstance(mac, str) or not entry \
                or not hmac.compare_digest(mac.encode(), proof(key, "os", a, b).encode()):
            return None
        if buf:
            return None  # nothing may follow the proof before our answer
        sock.settimeout(None)
        welcome = {"type": "welcome", "backend": self.hive.backend.name, "model": self.hive.backend.model,
                   "mac": proof(key, "hive", b, a)}
        sock.sendall(json.dumps(welcome).encode() + b"\n")
        return Peer(sock, name, session_key(key, a, b), self.hive, self.gone)


def manage_machines(state: State, action: list[str]) -> int:
    """`pair NAME`, `unpair NAME`, `machines`."""
    machines = state.machines()
    if action == ["machines"]:
        for name, entry in sorted(machines.items()):
            print(f"{name:<16} paired {entry.get('paired', '?')}, last seen {entry.get('seen') or 'never'}")
        if not machines:
            print("no paired machines (pair one with: beehive.py pair NAME)")
        return 0
    if len(action) == 2 and action[0] in ("pair", "unpair"):
        name = action[1]
        if not MACHINE_NAME.fullmatch(name):
            print("a machine name is 1 to 16 characters: a-z, 0-9 and -", file=sys.stderr)
            return 2
        if action[0] == "unpair":
            if machines.pop(name, None) is None:
                print(f"no machine called {name}", file=sys.stderr)
                return 1
            state.save_machines(machines)
            print(f"{name} is no longer paired (a running beehive drops it at its next connection)")
            return 0
        key = new_key()
        machines[name] = {"key": key, "paired": now_iso(), "seen": None}
        state.save_machines(machines)
        print(f"{name} is paired. "
              f"On that machine, type:\n\n    ai connect HOST:{DEFAULT_PORT} {name} {key}\n\n"
              f"HOST is this computer's address on your network; start beehive with\n"
              f"--listen HOST:{DEFAULT_PORT}. The key is shown only now.")
        return 0
    print("usage: beehive.py pair NAME | unpair NAME | machines", file=sys.stderr)
    return 2


UPDATE_CHUNK = 4608   # bytes of kernel per line: 6 KiB as text, within the OS's line limit and serial buffer
CONTROL_KINDS = ("run", "screenshot")


def offer_kernel(state: State, action: list[str], build: int | None) -> int:
    """`offer [KERNEL]` / `offer none`: the kernel machines get when they ask for an update."""
    if action == ["offer", "none"]:
        for name in ("offer.json", "offer.elf"):
            (state.path / name).unlink(missing_ok=True)
        print("no kernel is on offer now")
        return 0
    kernel = Path(action[1]) if len(action) == 2 else ROOT / "build" / "beecoders.elf"
    if len(action) > 2 or not kernel.is_file():
        print(f"usage: beehive.py offer [KERNEL | none]   ({kernel} not found)" if len(action) <= 2 else
              "usage: beehive.py offer [KERNEL | none]", file=sys.stderr)
        return 2
    data = kernel.read_bytes()
    if data[:4] != b"\x7fELF":
        print(f"{kernel} is not a kernel (ELF) file", file=sys.stderr)
        return 2
    if build is None:
        try:
            build = int((ROOT / "BUILD_NUMBER").read_text())
        except (OSError, ValueError):
            print("give the build number with --build", file=sys.stderr)
            return 2
    (state.path / "offer.json").unlink(missing_ok=True)  # never a description of another file
    (state.path / "offer.elf").write_bytes(data)
    (state.path / "offer.json").write_text(json.dumps(
        {"build": build, "size": len(data), "sha256": hashlib.sha256(data).hexdigest(), "t": now_iso()}))
    print(f"kernel b{build} is on offer ({len(data) // 1024} KiB): machines get it with `update`, "
          "or by themselves with `update auto on`")
    return 0


class Control(threading.Thread):
    """Requests of tools/beectl.py: files in control/in, sent to the machine they name.
    The machine's answers are collected in control/out (see Hive.control_answer)."""

    def __init__(self, hive: "Hive"):
        super().__init__(daemon=True)
        self.hive = hive
        self.inbox = hive.state.path / "control" / "in"
        self.inbox.mkdir(parents=True, exist_ok=True)
        (hive.state.path / "control" / "out").mkdir(parents=True, exist_ok=True)

    def run(self):
        while True:
            for f in sorted(self.inbox.glob("*.json")):
                try:
                    request = json.loads(f.read_text())
                except (OSError, ValueError):
                    continue  # still being written, or not ours
                f.unlink(missing_ok=True)
                rid, machine = request.get("rid"), request.get("machine")
                if not isinstance(rid, str) or not re.fullmatch(r"[A-Za-z0-9-]{1,40}", rid) or request.get("type") not in CONTROL_KINDS:
                    continue
                link = self.hive.links.get(machine)
                if link is None:
                    names = ", ".join(sorted(self.hive.links)) or "none"
                    self.hive.control_answer({"rid": rid, "done": True, "error": f"{machine} is not connected (connected: {names})"})
                    continue
                log(f"{machine}: control request {request['type']}")
                link.send({k: v for k, v in request.items() if k != "machine"})
            time.sleep(0.2)


class Hive:
    def __init__(self, backend, state: State, readme: str, retry: float):
        self.backend, self.state, self.retry = backend, state, retry
        self.system = SYSTEM_PROMPT.format(readme=readme)
        self.links: dict[str, Link] = {}  # connected machines by name (a serial port: its address)

    def control_answer(self, msg: dict):
        """One piece of a machine's answer to a control request; "done" ends it."""
        rid = msg["rid"]
        if not re.fullmatch(r"[A-Za-z0-9-]{1,40}", rid):
            return
        out = self.state.path / "control" / "out"
        out.mkdir(parents=True, exist_ok=True)
        with self.state.lock, open(out / f"{rid}.jsonl", "a") as f:
            f.write(json.dumps(msg, ensure_ascii=False) + "\n")
        if msg.get("done"):
            (out / f"{rid}.done").write_text("")

    def prompt(self, msg: dict) -> str:
        get = lambda k, d="": str(msg.get(k) or d)
        if msg["type"] == "ask":
            return f"Current directory: {get('cwd', '/')}\nQuestion: {get('text')}"
        if msg.get("crash"):
            return EXPLAIN_CRASH.format(build=get("build", "?"), cmd=get("cmd", "?"),
                                        kind=get("kind", "panic"), crash=get("crash"))
        return EXPLAIN_FAILURE.format(cmd=get("cmd"), error=get("error", "(no message)"), cwd=get("cwd", "/"))


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--config", type=Path, default=Path.home() / ".config/beehive/config.toml")
    ap.add_argument("--no-config", action="store_true", help="ignore the config file")
    ap.add_argument("action", nargs="*", metavar="pair|unpair|machines|offer",
                    help="pair NAME / unpair NAME / machines: manage machines on the network; "
                         "offer [KERNEL|none]: the kernel machines get with `update`. Then exit")
    ap.add_argument("--build", type=int, help="offer: the kernel's build number (default: BUILD_NUMBER)")
    ap.add_argument("--listen", metavar="ADDRESS[:PORT]",
                    help=f"also serve paired machines over the network on this address (port {DEFAULT_PORT} "
                         "unless given). Not encrypted. Off unless given")
    ap.add_argument("--max-requests-per-machine", type=int,
                    help="daily limit for one machine on the network (default: the daily limit)")
    ap.add_argument("--connect", action="append", metavar="HOST:PORT",
                    help=f"VM serial port(s) to serve (default: {', '.join(DEFAULT_TARGETS)}; "
                         "`none`: no serial ports, only --listen)")
    ap.add_argument("--backend", choices=["anthropic", "claude-code", "grok", "openai", "fake"])
    ap.add_argument("--model", help="model (anthropic default: claude-opus-5-5; CLIs: their default)")
    ap.add_argument("--effort", choices=["low", "medium", "high", "xhigh", "max"],
                    help="effort / reasoning level (default: low for anthropic and claude-code, "
                         "the CLI's default for grok)")
    ap.add_argument("--command", help="claude-code / grok: path to the CLI executable")
    ap.add_argument("--base-url", help="openai: endpoint, e.g. http://localhost:11434/v1 (Ollama)")
    ap.add_argument("--api-key-env", help="openai: environment variable holding the API key")
    ap.add_argument("--api-key-file", type=Path, help="openai: file holding the API key")
    ap.add_argument("--state-dir", type=Path)
    ap.add_argument("--max-requests-per-day", type=int)
    ap.add_argument("--retry", type=float, default=2.0, help="seconds between connection attempts")
    args = ap.parse_args()

    cfg = {}
    if not args.no_config and args.config.exists():
        cfg = tomllib.loads(args.config.read_text())
    opt = lambda name, default=None: getattr(args, name) if getattr(args, name) is not None else cfg.get(name, default)

    backend_name = opt("backend", "anthropic")
    state_dir = Path(opt("state_dir", Path.home() / ".local/state/beehive")).expanduser()
    if args.action and args.action[0] == "offer":
        sys.exit(offer_kernel(State(state_dir, 0), args.action, args.build))
    if args.action:
        sys.exit(manage_machines(State(state_dir, 0), args.action))
    if backend_name == "fake":
        backend = FakeBackend()
    elif backend_name == "claude-code":
        backend = ClaudeCodeBackend(opt("command"), opt("model"), opt("effort", "low"), state_dir / "cli-workdir")
    elif backend_name == "grok":
        backend = GrokBackend(opt("command"), opt("model"), opt("effort"), state_dir / "cli-workdir")
    elif backend_name == "anthropic":
        backend = AnthropicBackend(opt("model", "claude-opus-5-5"), opt("effort", "low"))
    else:
        if not opt("base_url") or not opt("model"):
            sys.exit("--backend openai needs --base-url and --model")
        key = None
        if opt("api_key_file"):
            key = Path(opt("api_key_file")).expanduser().read_text().strip()
        elif opt("api_key_env"):
            key = os.environ.get(opt("api_key_env"))
        backend = OpenAIBackend(opt("model"), opt("base_url"), key)

    readme_path = ROOT / "README.md"
    readme = readme_path.read_text() if readme_path.exists() else "(no README found)"
    per_machine = opt("max_requests_per_machine")
    state = State(state_dir, int(opt("max_requests_per_day", 200)), None if per_machine is None else int(per_machine))
    hive = Hive(backend, state, readme, args.retry)
    targets = args.connect or cfg.get("connect") or DEFAULT_TARGETS
    targets = [t for t in targets if t != "none"]  # --connect none: no serial ports, the network only
    log(f"backend {backend.name} ({backend.model}), state in {state_dir}")
    links = [Link(t, hive) for t in targets]
    for link in links:
        link.start()
    Control(hive).start()
    if opt("listen"):
        listener = Listener(opt("listen"), hive)
        listener.start()
        log(f"listening for paired machines on {opt('listen')} (port {listener.port}); not encrypted")
    try:
        while True:
            time.sleep(3600)
    except KeyboardInterrupt:
        log("stopped")


if __name__ == "__main__":
    main()
