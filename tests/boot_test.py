#!/usr/bin/env python3
"""BeeCodersOs boot tests: boot the kernel in headless QEMU and check it over
the serial port. Exits 0 when every test passes, 1 otherwise.

    tests/boot_test.py all                 everything below (what `make check` runs)
    tests/boot_test.py selftest            in-kernel self-tests (`selftest` boot flag)
    tests/boot_test.py scenarios [FILE..]  shell scenarios in tests/scenarios/*.scn
    tests/boot_test.py iso                 boot build/beecoders.iso (BIOS) and shut down
    tests/boot_test.py iso-uefi            same with OVMF (skipped if OVMF is missing)

Options: -v (print all serial output), --no-kvm (pure emulation).

Scenario files (tests/scenarios/*.scn):
    # comment                  ignored
    @disk                      (first lines) boot with a fresh BeeFS disk instead of the RAM disk
    @ai                        (first lines) attach COM2 and run beehive with its fake backend
    @ai-offline                (first lines) attach COM2 but run no beehive
    @wire                      (first lines) connect the e1000 network card to the test: it
                               sees every frame the OS sends and can inject frames (the OS
                               boots with `nodhcp`, so it stays silent until told otherwise)
    @tcp                       (first lines, with @net) run small TCP services on the host;
                               in a command, {NAME} becomes "10.0.2.2 PORT":
                                 {once}    reads one line, answers "you said: LINE", closes
                                 {echo}    answers every line with "echo: LINE"; "bye" ends it
                                 {lines}   sends 300 numbered lines, then closes
                                 {sink}    reads until the client finishes, then reports
                                           how many bytes it got and closes
                                 {closed}  a port where nothing listens
    @http                      (first lines, with @net) run a small web server on the host;
                               {http} becomes its address (10.0.2.2:PORT), {http-port} its
                               port. Pages: see FakeHttp.PAGES
    @ntp                       (first lines, with @net) run a fake time server whose clock
                               started at 2030-01-02T03:04:05Z: {ntp} is its address,
                               {ntp-unsynced} one that says it does not know the time
    @tftp                      (first lines, with @net) run a TFTP server on the host with the
                               files hello.txt and big.bin (200 000 bytes): {tftp} is its
                               address. Fetching "compare:A:B" tells whether its files A and
                               B are identical
Without @wire or @net the machine has no network card.
    @net                       (first lines) connect the card to QEMU's built-in network: a
                               DHCP server (gives 10.0.2.15/24) and a gateway at 10.0.2.2
                               that answers pings; isolated from the host's network
    @dns NAME=IP ...           (first lines, with @net) run a fake DNS server on the host;
                               `{dns}` in a command becomes its address (10.0.2.2:PORT).
                               Other names do not exist; names starting with "drop." get
                               no answer at all
    @bootdisk                  (first lines) boot from an A/B boot disk (tools/bootdisk.py)
                               with the kernel in both slots, BeeFS in partition 2
    $ COMMAND                  type a command, wait for the next prompt
    $! COMMAND                 the command must crash the kernel; boots again (same disk)
    $^C COMMAND                type the command, press Ctrl+C after 1 s, wait for the prompt
    $& COMMAND                 start a command and go on without waiting for the prompt
    > TEXT                     type a line into the running command (>^D / >^C: press Ctrl+D / Ctrl+C)
    $wait                      wait for the prompt; expectations apply to everything since `$&`
    TEXT                       an output line of the last command must equal TEXT
    ~ REGEX                    an output line must match REGEX (re.search)
    !~ REGEX                   no output line may match REGEX
    @reboot                    shut down, boot again with the same disk
    @signal REGEX              (with @ai) beehive must have received a signal matching REGEX
    @frame HEXREGEX            (with @wire) the OS must have sent a frame whose bytes, as
                               lowercase hex, match the regex (waits up to 3 s)
    @send [Nx] HEX             (with @wire) inject this frame (hex, spaces allowed), N times
    @deploy [ARGS]             (with @bootdisk) shut down, deploy the kernel to the spare
                               slot (ARGS: extra kernel arguments for it), boot again
    @deploy-crash ARGS         like @deploy, but the trial kernel must crash; boots again
Expected lines must appear in order; other output lines in between are allowed.
Expectations after `$!` or `@reboot` apply to the boot messages of the new boot.
After the last command the runner types `shutdown` and expects a clean power-off.
"""

import argparse
import codecs
import os
import re
import shutil
import socket
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
KERNEL = ROOT / "build" / "beecoders.elf"
ISO = ROOT / "build" / "beecoders.iso"
BOOTDISK = ROOT / "tools" / "bootdisk.py"
BEEHIVE = ROOT / "tools" / "beehive" / "beehive.py"
BEEFS_TOOL = ROOT / "tools" / "beefs-tool" / "target" / "release" / "beefs-tool"
SCENARIO_DIR = ROOT / "tests" / "scenarios"
OVMF_CODE = Path("/usr/share/edk2/x64/OVMF_CODE.4m.fd")

EXIT_SUCCESS = (0x10 << 1) | 1  # testmode::SUCCESS through isa-debug-exit
EXIT_FAILURE = (0x11 << 1) | 1

WIRE_MAC = "52:54:00:be:e0:01"  # the OS's card in @wire scenarios

ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\x1b[()][A-Z0-9]|\x1b[=>]")
PROMPT = re.compile(r"bee:\S*\$ $")
BOOT_TIMEOUT = 30
CMD_TIMEOUT = 10

VERBOSE = False
USE_KVM = os.access("/dev/kvm", os.R_OK | os.W_OK)


class TestFailure(Exception):
    pass


def clean(text: str) -> str:
    """Serial output as plain text: no ANSI codes, no CR, backspaces applied."""
    text = ANSI.sub("", text).replace("\r", "")
    while "\b" in text:
        text = re.sub(r"[^\n\b]\b|^\b", "", text, count=0, flags=re.M)
    return text


class Vm:
    """One QEMU run with the serial port on stdin/stdout."""

    def __init__(self, *, kernel_args=None, cdrom=None, disk=None, uefi=False, ai_port=None, bootdisk=None,
                 wire=None, usernet=None):
        cmd = ["qemu-system-x86_64", "-m", "128M", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-no-reboot",
               "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]
        if USE_KVM:
            cmd += ["-enable-kvm", "-cpu", "host"]
        if cdrom:
            cmd += ["-cdrom", str(cdrom)]
        elif bootdisk:  # Limine on the disk picks the kernel slot
            cmd += ["-drive", f"file={bootdisk},format=raw,if=ide,index=0,media=disk"]
        else:
            cmd += ["-kernel", str(KERNEL), "-append", kernel_args or ("test nodhcp" if wire else "test")]
        if disk:
            cmd += ["-drive", f"file={disk},format=raw,if=ide,index=0,media=disk"]
        if uefi:
            cmd += ["-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}"]
        if wire:  # e1000 on a UDP "wire": each datagram is one Ethernet frame
            ours, theirs = wire
            cmd += ["-netdev", f"socket,id=n0,udp=127.0.0.1:{ours},localaddr=127.0.0.1:{theirs}",
                    "-device", f"e1000,netdev=n0,mac={WIRE_MAC}"]
        if usernet:  # QEMU's user-mode network; "isolated": cut off from the host and beyond
            restrict = "on" if usernet == "isolated" else "off"
            cmd += ["-netdev", f"user,id=n0,restrict={restrict}", "-device", f"e1000,netdev=n0,mac={WIRE_MAC}"]
            if USE_KVM:
                # With KVM's in-kernel timer, QEMU's main loop sleeps up to a second, and its
                # user-mode network only notices new host sockets when that loop wakes up.
                # Emulating the timer and interrupt controller in QEMU keeps the loop ticking.
                cmd += ["-machine", "kernel-irqchip=split"]
        if not wire and not usernet:
            cmd += ["-nic", "none"]
        if ai_port:  # COM2 = AI bridge, a TCP server for beehive
            cmd += ["-chardev", f"socket,id=ai,host=127.0.0.1,port={ai_port},server=on,wait=off",
                    "-serial", "chardev:ai"]
        self.proc = subprocess.Popen(cmd, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                     stderr=subprocess.STDOUT)
        self.raw = ""
        self.eof = False
        self.pos = 0  # start of not yet consumed output (index into clean text)
        self.lock = threading.Condition()
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        # QEMU often sends one byte at a time: decode UTF-8 across chunk borders.
        decoder = codecs.getincrementaldecoder("utf-8")(errors="replace")
        while chunk := self.proc.stdout.read1(4096):
            text = decoder.decode(chunk)
            if VERBOSE:
                sys.stdout.write(text)
                sys.stdout.flush()
            with self.lock:
                self.raw += text
                self.lock.notify_all()
        with self.lock:
            self.eof = True
            self.lock.notify_all()

    def text(self) -> str:
        with self.lock:
            return clean(self.raw)

    def expect(self, pattern: re.Pattern, timeout: float, what: str) -> str:
        """Wait until `pattern` matches the unconsumed output; consume and return it."""
        deadline = time.monotonic() + timeout
        with self.lock:
            while True:
                text = clean(self.raw)
                m = pattern.search(text, self.pos)
                if m:
                    out = text[self.pos:m.end()]
                    self.pos = m.end()
                    return out
                remaining = deadline - time.monotonic()
                if self.eof or remaining <= 0:
                    break
                self.lock.wait(min(0.5, remaining))
        status = None
        if self.eof:
            try:
                status = self.proc.wait(5)
            except subprocess.TimeoutExpired:
                pass
        state = f"QEMU exited with status {status}" if self.eof else f"timeout after {timeout}s"
        raise TestFailure(f"waiting for {what}: {state}\n--- last output ---\n{tail(self.text())}")

    def send(self, line: str):
        self.proc.stdin.write((line + "\r").encode())
        self.proc.stdin.flush()

    def command(self, line: str, interrupt_after: float | None = None) -> list[str]:
        """Type a command and return its output lines (without echo and prompt)."""
        self.send(line)
        if interrupt_after is not None:
            time.sleep(interrupt_after)
            self.proc.stdin.write(b"\x03")
            self.proc.stdin.flush()
        out = self.expect(PROMPT, CMD_TIMEOUT, f"prompt after `{line}`")
        lines = out.split("\n")
        return [l.rstrip() for l in lines[1:-1]]  # drop echoed command and the new prompt

    def wait_exit(self, timeout: float) -> int:
        try:
            return self.proc.wait(timeout)
        except subprocess.TimeoutExpired:
            raise TestFailure(f"QEMU did not exit within {timeout}s\n--- last output ---\n{tail(self.text())}")

    def shutdown(self):
        self.send("shutdown")
        status = self.wait_exit(BOOT_TIMEOUT)
        if status != 0 or "Shutting down..." not in self.text():
            raise TestFailure(f"clean shutdown expected, QEMU exit status {status}\n"
                              f"--- last output ---\n{tail(self.text())}")

    def kill(self):
        if self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait()


def tail(text: str, n: int = 25) -> str:
    return "\n".join(text.rstrip().split("\n")[-n:])


def boot_to_prompt(vm: Vm) -> list[str]:
    """Wait for the first prompt; return the boot messages."""
    out = vm.expect(PROMPT, BOOT_TIMEOUT, "the first shell prompt")
    if "KERNEL PANIC" in out or "CPU EXCEPTION" in out:
        raise TestFailure(f"kernel error during boot\n{tail(vm.text())}")
    return [l.rstrip() for l in out.split("\n")[:-1]]


# --- tests -----------------------------------------------------------------------

def test_selftest():
    vm = Vm(kernel_args="test selftest")
    try:
        status = vm.wait_exit(BOOT_TIMEOUT)
        out = vm.text()
        if status != EXIT_SUCCESS:
            fails = "\n".join(l for l in out.split("\n") if "FAIL" in l or l.startswith("      "))
            raise TestFailure(f"QEMU exit status {status} (pass = {EXIT_SUCCESS})\n{fails or tail(out)}")
        summary = re.search(r"selftest: \d+ passed, 0 failed", out)
        return summary.group(0) if summary else "passed"
    finally:
        vm.kill()


def run_tool(cmd: list):
    r = subprocess.run(cmd, capture_output=True, text=True)
    if r.returncode != 0:
        raise TestFailure(f"{' '.join(map(str, cmd[1:3]))} failed:\n{r.stdout}{r.stderr}")


def make_disk(path: Path):
    if not BEEFS_TOOL.exists():
        raise TestFailure(f"{BEEFS_TOOL} missing: run `make tool`")
    subprocess.run([str(BEEFS_TOOL), str(path), "mkfs", "4"], check=True, capture_output=True)


def parse_scenario(path: Path):
    """Return (options, steps, signals); a step is (kind, command, [expectations])
    with kind "cmd", "crash", "interrupt" or "reboot"."""
    opts, steps, signals = set(), [], []
    for n, raw in enumerate(path.read_text().splitlines(), 1):
        line = raw.rstrip()
        if not line or line.startswith("#"):
            continue
        if line in ("@disk", "@ai", "@ai-offline", "@bootdisk", "@wire", "@net"):
            opts.add(line[1:])
        elif line.startswith("@dns "):
            opts.add("dns:" + line[5:])
        elif line.startswith(("@frame ", "@send ")):
            kind, _, arg = line[1:].partition(" ")
            steps.append((kind, arg.strip(), []))
        elif line.startswith("@deploy"):
            kind, _, extra = line[1:].partition(" ")
            steps.append((kind, extra.strip(), []))
        elif line == "@reboot":
            steps.append(("reboot", "(reboot)", []))
        elif line.startswith("@signal "):
            signals.append((re.compile(line[8:]), n))
        elif line in ("@tcp", "@http", "@ntp", "@tftp"):
            opts.add(line[1:])
        elif line.startswith("$& "):
            steps.append(("start", line[3:], []))
        elif line.startswith("> ") or line in (">^D", ">^C"):
            steps.append(("type", line[2:] if line.startswith("> ") else line[1:], []))
        elif line == "$wait":
            steps.append(("wait", "(wait)", []))
        elif line.startswith("$! "):
            steps.append(("crash", line[3:], []))
        elif line.startswith("$^C "):
            steps.append(("interrupt", line[4:], []))
        elif line.startswith("$ "):
            steps.append(("cmd", line[2:], []))
        elif steps:
            if line.startswith("!~ "):
                steps[-1][2].append(("absent", re.compile(line[3:]), n))
            elif line.startswith("~ "):
                steps[-1][2].append(("regex", re.compile(line[2:]), n))
            else:
                steps[-1][2].append(("exact", line, n))
        else:
            raise TestFailure(f"{path.name}:{n}: expected output before any `$ command`")
    return opts, steps, signals


def check_output(path: Path, cmd: str, out: list[str], expectations):
    i = 0
    for kind, want, n in expectations:
        if kind == "absent":
            if any(want.search(l) for l in out):
                raise TestFailure(f"{path.name}:{n}: `{cmd}` printed a line matching /{want.pattern}/\n"
                                  + "\n".join(out))
            continue
        while i < len(out):
            line = out[i]
            i += 1
            if (kind == "exact" and line == want) or (kind == "regex" and want.search(line)):
                break
        else:
            shown = want if kind == "exact" else f"/{want.pattern}/"
            raise TestFailure(f"{path.name}:{n}: `{cmd}` did not print {shown!r} (in order)\n"
                              f"--- output ---\n" + "\n".join(out))


class FakeDns(threading.Thread):
    """A tiny DNS server for tests (see @dns): A records from a table."""

    def __init__(self, table: str):
        super().__init__(daemon=True)
        self.table = dict(pair.split("=") for pair in table.split())
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.queries = []
        self.start()

    def run(self):
        while True:
            try:
                data, peer = self.sock.recvfrom(1500)
            except OSError:
                return
            labels, pos = [], 12
            while pos < len(data) and data[pos]:
                labels.append(data[pos + 1:pos + 1 + data[pos]].decode(errors="replace"))
                pos += 1 + data[pos]
            name = ".".join(labels).lower()
            self.queries.append(name)
            if name.startswith("drop."):
                continue
            question = data[12:pos + 5]
            ip = self.table.get(name)
            header = data[:2] + (b"\x81\x80" if ip else b"\x81\x83") + b"\x00\x01" + (b"\x00\x01" if ip else b"\x00\x00") + b"\x00\x00\x00\x00"
            answer = b"\xc0\x0c\x00\x01\x00\x01\x00\x00\x00\x3c\x00\x04" + socket.inet_aton(ip) if ip else b""
            self.sock.sendto(header + question + answer, peer)

    def close(self):
        self.sock.close()


class FakeTcp:
    """Small TCP services for tests (see @tcp)."""

    def __init__(self):
        self.ports, self.socks = {}, []
        for name in ("once", "echo", "lines", "sink"):
            srv = socket.socket()
            srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
            srv.bind(("127.0.0.1", 0))
            srv.listen(8)
            self.ports[name] = srv.getsockname()[1]
            self.socks.append(srv)
            threading.Thread(target=self.accept, args=(srv, getattr(self, name)), daemon=True).start()
        probe = socket.socket()
        probe.bind(("127.0.0.1", 0))
        self.ports["closed"] = probe.getsockname()[1]
        probe.close()

    def accept(self, srv, handler):
        while True:
            try:
                conn, _ = srv.accept()
            except OSError:
                return
            threading.Thread(target=self.serve, args=(conn, handler), daemon=True).start()

    @staticmethod
    def serve(conn, handler):
        try:
            with conn:
                conn.settimeout(20)
                handler(conn, conn.makefile("rb"))
        except OSError:
            pass

    @staticmethod
    def once(conn, lines):
        conn.sendall(b"you said: " + lines.readline().rstrip(b"\r\n") + b"\n")

    @staticmethod
    def echo(conn, lines):
        for line in lines:
            line = line.rstrip(b"\r\n")
            if line == b"bye":
                conn.sendall(b"goodbye\n")
                return
            conn.sendall(b"echo: " + line + b"\n")

    @staticmethod
    def lines(conn, _):
        conn.sendall(b"".join(b"line %03d of 300 %s\n" % (i, b"=" * 30) for i in range(1, 301)))

    @staticmethod
    def sink(conn, lines):
        data = lines.read()  # until the client finishes its side
        conn.sendall(b"received %d bytes in %d lines\n" % (len(data), data.count(b"\n")))

    def close(self):
        for s in self.socks:
            s.close()


class FakeHttp:
    """A small web server for tests (see @http)."""

    BIG = b"".join(b"line %05d of the big page %s\n" % (i, b"-" * 40) for i in range(1, 4001))
    PAGES = {  # path: (status line, extra headers, body); "{port}" in a header is our port
        "/hello": ("200 OK", ["Content-Type: text/plain"], b"Hello from the test web server\n"),
        "/redirect": ("302 Found", ["Location: /hello"], b"go away\n"),
        "/redirect-full": ("301 Moved Permanently", ["Location: http://10.0.2.2:{port}/chunked"], b""),
        "/loop": ("302 Found", ["Location: /loop"], b""),
        "/secure": ("301 Moved Permanently", ["Location: https://example.com/"], b""),
        "/big": ("200 OK", [], BIG),
        "/empty": ("204 No Content", [], b""),
        "/binary": ("200 OK", [], bytes(range(256))),
    }

    def __init__(self):
        self.srv = socket.socket()
        self.srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.srv.bind(("127.0.0.1", 0))
        self.srv.listen(8)
        self.port = self.srv.getsockname()[1]
        self.requests = []
        threading.Thread(target=self.accept, daemon=True).start()

    def accept(self):
        while True:
            try:
                conn, _ = self.srv.accept()
            except OSError:
                return
            threading.Thread(target=self.serve, args=(conn,), daemon=True).start()

    def serve(self, conn):
        try:
            with conn:
                conn.settimeout(20)
                head = b""
                while b"\r\n\r\n" not in head:
                    piece = conn.recv(4096)
                    if not piece:
                        return
                    head += piece
                lines = head.decode(errors="replace").split("\r\n")
                self.requests.append(lines)
                method, path, _ = (lines[0].split(" ") + ["", ""])[:3]
                headers = {k.lower(): v.strip() for k, _, v in (l.partition(":") for l in lines[1:] if l)}
                send = conn.sendall
                if method != "GET":
                    send(b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\n\r\n")
                elif path == "/chunked":  # in pieces, with a pause in the middle
                    send(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n6\r\nfirst\n\r\n")
                    time.sleep(0.2)
                    send(b"7;x=y\r\nsecond\n\r\n1\r\nt\r\n5\r\nhird\n\r\n0\r\n\r\n")
                elif path == "/until-close":  # an old-style answer: the end is the close
                    send(b"HTTP/1.0 200 OK\r\n\r\nno length given\nbut two lines\n")
                elif path == "/cut":  # promises 100 bytes, sends 10
                    send(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\n0123456789")
                elif path == "/huge":  # larger than a BeeFS file can be
                    send(b"HTTP/1.1 200 OK\r\nContent-Length: 9000000\r\n\r\n" + b"x" * 1000)
                elif path == "/who":  # what the client said about itself
                    body = f"host={headers.get('host')} agent={headers.get('user-agent')} " \
                           f"connection={headers.get('connection')}\n".encode()
                    send(b"HTTP/1.1 200 OK\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
                elif path == "/not-http":
                    send(b"SSH-2.0-NotAWebServer\r\n\r\n")
                elif path.split("?")[0] in self.PAGES:
                    status, extra, body = self.PAGES[path.split("?")[0]]
                    head = [f"HTTP/1.1 {status}"] + [h.replace("{port}", str(self.port)) for h in extra]
                    if not status.startswith("204"):
                        head.append(f"Content-Length: {len(body)}")
                    send("\r\n".join(head).encode() + b"\r\n\r\n" + body)
                else:
                    body = b"no such page\n"
                    send(b"HTTP/1.1 404 Not Found\r\nContent-Length: %d\r\n\r\n" % len(body) + body)
        except OSError:
            pass

    def close(self):
        self.srv.close()


class FakeNtp(threading.Thread):
    """A fake time server (see @ntp). Its clock started at 2030-01-02T03:04:05Z
    when the test began; with `synced=False` it claims to have no time."""

    START = 1893553445  # 2030-01-02T03:04:05Z

    def __init__(self, synced=True):
        super().__init__(daemon=True)
        self.synced = synced
        self.began = time.monotonic()
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.start()

    def run(self):
        while True:
            try:
                data, peer = self.sock.recvfrom(1500)
            except OSError:
                return
            if len(data) < 48 or data[0] & 7 != 3:
                continue
            now = self.START + 2208988800 + (time.monotonic() - self.began)
            stamp = int(now).to_bytes(4, "big") + int(now % 1 * 2**32).to_bytes(4, "big")
            first = bytes([0x24 if self.synced else 0xE4, 2 if self.synced else 0, 0, 0xEC])
            reply = first + bytes(12) + stamp + data[40:48] + stamp + stamp  # reference, origin, receive, transmit
            self.sock.sendto(reply, peer)

    def close(self):
        self.sock.close()


class FakeTftp(threading.Thread):
    """A TFTP server for tests (see @tftp): files in memory, binary mode, 512-byte blocks."""

    BIG = bytes((i * 7 + i // 512) % 256 for i in range(200_000))

    def __init__(self):
        super().__init__(daemon=True)
        self.files = {"hello.txt": b"Hello from the TFTP server\n", "big.bin": self.BIG, "empty": b""}
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        self.port = self.sock.getsockname()[1]
        self.start()

    def run(self):
        while True:
            try:
                data, peer = self.sock.recvfrom(1500)
            except OSError:
                return
            fields = data[2:].split(b"\0")
            if data[:2] in (b"\0\1", b"\0\2") and len(fields) >= 2 and fields[1].lower() == b"octet":
                name = fields[0].decode(errors="replace")
                threading.Thread(target=self.transfer, args=(data[1] == 2, name, peer), daemon=True).start()

    def content(self, name):
        if name.startswith("compare:"):
            _, a, b = (name.split(":") + ["", ""])[:3]
            fa, fb = self.files.get(a), self.files.get(b)
            if fa is None or fb is None:
                return None
            same = f"are identical ({len(fa)} bytes)" if fa == fb else "are different"
            return f"{a} and {b} {same}\n".encode()
        return self.files.get(name)

    def transfer(self, writing, name, peer):
        """One transfer, on a socket (port) of its own as TFTP requires."""
        sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        sock.bind(("127.0.0.1", 0))
        sock.settimeout(1)

        def exchange(packet, want_op, want_block):
            """Send `packet` until the answer with that opcode and block arrives; its payload."""
            for _ in range(5):
                sock.sendto(packet, peer)
                try:
                    while True:
                        data, sender = sock.recvfrom(1500)
                        if sender == peer and data[:2] == want_op and data[2:4] == want_block.to_bytes(2, "big"):
                            return data[4:]
                        if sender == peer and data[:2] == b"\0\5":
                            return None
                except socket.timeout:
                    pass
            return None

        try:
            if writing:
                if name == "readonly":
                    sock.sendto(b"\0\5\0\2Access violation\0", peer)
                    return
                data, block = b"", 0
                while True:
                    piece = exchange(b"\0\4" + block.to_bytes(2, "big"), b"\0\3", block + 1)
                    if piece is None:
                        return
                    data, block = data + piece, block + 1
                    if len(piece) < 512:
                        break
                self.files[name] = data
                sock.sendto(b"\0\4" + block.to_bytes(2, "big"), peer)
            else:
                data = self.content(name)
                if data is None:
                    sock.sendto(b"\0\5\0\1File not found\0", peer)
                    return
                for block in range(1, len(data) // 512 + 2):
                    piece = data[(block - 1) * 512:block * 512]
                    if exchange(b"\0\3" + block.to_bytes(2, "big") + piece, b"\0\4", block) is None:
                        return
        except OSError:
            pass
        finally:
            sock.close()

    def close(self):
        self.sock.close()


class Wire:
    """The test's end of the network cable (see @wire)."""

    def __init__(self):
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.bind(("127.0.0.1", 0))
        probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        probe.bind(("127.0.0.1", 0))
        self.ports = (self.sock.getsockname()[1], probe.getsockname()[1])  # (ours, QEMU's)
        probe.close()
        self.frames = []
        self.ready_at = 0.0

    def booted(self):
        # QEMU's e1000 holds back received frames for 1 s after the guest
        # enables its receiver; inject nothing before that.
        self.ready_at = time.monotonic() + 1.2

    def expect(self, pattern: str, where: str):
        rx, deadline = re.compile(pattern), time.monotonic() + 3
        while True:
            for i, f in enumerate(self.frames):
                if rx.search(f):
                    del self.frames[:i + 1]
                    return
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TestFailure(f"{where}: the OS sent no frame matching /{pattern}/\n--- frames seen ---\n"
                                  + ("\n".join(self.frames) or "(none)"))
            self.sock.settimeout(remaining)
            try:
                self.frames.append(self.sock.recv(4096).hex())
            except TimeoutError:
                pass

    def send(self, hex_frame: str):
        time.sleep(max(0.0, self.ready_at - time.monotonic()))
        count = 1
        if m := re.match(r"(\d+)x\s+(.*)", hex_frame):
            count, hex_frame = int(m[1]), m[2]
        frame = bytes.fromhex(hex_frame.replace(" ", ""))
        for _ in range(count):
            self.sock.sendto(frame, ("127.0.0.1", self.ports[1]))
            time.sleep(0.02)  # slower than the OS's 10 ms polling: no overrun
        time.sleep(0.2)  # let QEMU deliver the last one

    def close(self):
        self.sock.close()


def free_port() -> int:
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def check_signals(path: Path, state_dir: Path, signals):
    if not signals:
        return
    deadline = time.monotonic() + 3
    while True:
        try:
            lines = (state_dir / "signals.jsonl").read_text().splitlines()
        except OSError:
            lines = []
        missing = [(rx, n) for rx, n in signals if not any(rx.search(l) for l in lines)]
        if not missing or time.monotonic() > deadline:
            break
        time.sleep(0.2)
    if missing:
        rx, n = missing[0]
        raise TestFailure(f"{path.name}:{n}: beehive got no signal matching /{rx.pattern}/\n"
                          "--- signals.jsonl ---\n" + "\n".join(lines))


def test_scenario(path: Path):
    opts, steps, signals = parse_scenario(path)
    with tempfile.TemporaryDirectory(prefix="beetest-") as tmp:
        disk = Path(tmp) / "disk.img" if "disk" in opts else None
        if disk:
            make_disk(disk)
        bootdisk = Path(tmp) / "boot.img" if "bootdisk" in opts else None
        if bootdisk:
            run_tool([sys.executable, str(BOOTDISK), "create", str(bootdisk), "--kernel", str(KERNEL),
                      "--cmdline", "test", "--size-mb", "40"])
        ai_port = free_port() if opts & {"ai", "ai-offline"} else None
        hive = None
        if "ai" in opts:
            hive = subprocess.Popen(
                [sys.executable, str(BEEHIVE), "--no-config", "--backend", "fake", "--retry", "0.1",
                 "--connect", f"127.0.0.1:{ai_port}", "--state-dir", str(Path(tmp) / "beehive")],
                stdout=subprocess.PIPE if not VERBOSE else None, stderr=subprocess.STDOUT)
        wire = Wire() if "wire" in opts else None
        dns_table = next((o[4:] for o in opts if o.startswith("dns:")), None)
        dns = FakeDns(dns_table) if dns_table is not None else None
        tcp = FakeTcp() if "tcp" in opts else None
        names = {"{dns}": f"10.0.2.2:{dns.port}"} if dns else {}
        if tcp:
            names.update({"{%s}" % name: f"10.0.2.2 {port}" for name, port in tcp.ports.items()})
        servers = [x for x in (dns, tcp) if x]
        if "http" in opts:
            servers.append(FakeHttp())
            names.update({"{http}": f"10.0.2.2:{servers[-1].port}", "{http-port}": str(servers[-1].port)})
        if "ntp" in opts:
            servers += [FakeNtp(), FakeNtp(synced=False)]
            names.update({"{ntp}": f"10.0.2.2:{servers[-2].port}", "{ntp-unsynced}": f"10.0.2.2:{servers[-1].port}"})
        if "tftp" in opts:
            servers.append(FakeTftp())
            names["{tftp}"] = f"10.0.2.2:{servers[-1].port}"
        # The fake servers listen on the host, so the guest must be able to reach it.
        usernet = ("open" if servers else "isolated") if "net" in opts else None
        boot = lambda: Vm(disk=disk, ai_port=ai_port, bootdisk=bootdisk, wire=wire.ports if wire else None,
                          usernet=usernet)
        vm = boot()
        try:
            boot_to_prompt(vm)
            if wire:
                wire.booted()
            commands = 0
            for kind, cmd, expectations in steps:
                if kind in ("frame", "send"):
                    if not wire:
                        raise TestFailure(f"{path.name}: @{kind} needs @wire")
                    wire.expect(cmd, path.name) if kind == "frame" else wire.send(cmd)
                    continue
                if kind == "reboot":
                    vm.shutdown()
                    vm = boot()
                    out = boot_to_prompt(vm)
                elif kind in ("deploy", "deploy-crash"):
                    vm.shutdown()
                    deploy = [sys.executable, str(BOOTDISK), "deploy", str(bootdisk), "--kernel", str(KERNEL)]
                    run_tool(deploy + (["--cmdline-extra", cmd] if cmd else []))
                    vm = boot()
                    if kind == "deploy-crash":
                        status = vm.wait_exit(BOOT_TIMEOUT)
                        if status != EXIT_FAILURE:
                            raise TestFailure(f"{path.name}: the trial kernel should crash (exit {EXIT_FAILURE}), "
                                              f"QEMU exit status {status}\n{tail(vm.text())}")
                        vm = boot()
                    out = boot_to_prompt(vm)
                elif kind == "crash":
                    vm.send(cmd)
                    status = vm.wait_exit(BOOT_TIMEOUT)
                    if status != EXIT_FAILURE:
                        raise TestFailure(f"{path.name}: `{cmd}` should crash the kernel (exit {EXIT_FAILURE}), "
                                          f"QEMU exit status {status}\n{tail(vm.text())}")
                    vm = boot()
                    out = boot_to_prompt(vm)
                    commands += 1
                elif kind == "start":
                    for name, value in names.items():
                        cmd = cmd.replace(name, value)
                    vm.send(cmd)
                    time.sleep(0.5)
                    commands += 1
                    continue
                elif kind == "type":
                    if cmd in ("^D", "^C"):
                        vm.proc.stdin.write(b"\x04" if cmd == "^D" else b"\x03")
                        vm.proc.stdin.flush()
                    else:
                        vm.send(cmd)
                    time.sleep(0.4)
                    continue
                elif kind == "wait":
                    text = vm.expect(PROMPT, CMD_TIMEOUT, "the prompt after `$&`")
                    out = [l.rstrip() for l in text.split("\n")[1:-1]]
                else:
                    for name, value in names.items():
                        cmd = cmd.replace(name, value)
                    out = vm.command(cmd, interrupt_after=1.0 if kind == "interrupt" else None)
                    commands += 1
                    bad = [l for l in out if "KERNEL PANIC" in l or "CPU EXCEPTION" in l]
                    if bad:
                        raise TestFailure(f"`{cmd}` crashed the kernel\n" + "\n".join(out))
                check_output(path, cmd, out, expectations)
            vm.shutdown()
            if hive:
                check_signals(path, Path(tmp) / "beehive", signals)
            return f"{commands} commands"
        finally:
            vm.kill()
            if wire:
                wire.close()
            for server in servers:
                server.close()
            if hive:
                hive.kill()
                hive.wait()


def test_iso(uefi=False):
    if not ISO.exists():
        raise TestFailure(f"{ISO} missing: run `make iso`")
    vm = Vm(cdrom=ISO, uefi=uefi)
    try:
        boot_to_prompt(vm)
        out = vm.command("version")
        if not any(l.startswith("BeeCodersOs ") for l in out):
            raise TestFailure("`version` printed:\n" + "\n".join(out))
        vm.shutdown()
        return out[0]
    finally:
        vm.kill()


# --- main --------------------------------------------------------------------------

def main():
    global VERBOSE, USE_KVM
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("suite", choices=["all", "selftest", "scenarios", "iso", "iso-uefi"])
    ap.add_argument("files", nargs="*", help="scenario files (default: tests/scenarios/*.scn)")
    ap.add_argument("-v", "--verbose", action="store_true")
    ap.add_argument("--no-kvm", action="store_true")
    args = ap.parse_args()
    VERBOSE = args.verbose
    USE_KVM = USE_KVM and not args.no_kvm
    if not shutil.which("qemu-system-x86_64"):
        sys.exit("qemu-system-x86_64 not found")
    if args.suite in ("all", "selftest", "scenarios") and not KERNEL.exists():
        sys.exit(f"{KERNEL} missing: run `make`")

    tests = []
    if args.suite in ("all", "selftest"):
        tests.append(("selftest", test_selftest))
    if args.suite in ("all", "scenarios"):
        files = [Path(f) for f in args.files] or sorted(SCENARIO_DIR.glob("*.scn"))
        tests += [(f"scenario {f.stem}", lambda f=f: test_scenario(f)) for f in files]
    if args.suite in ("all", "iso"):
        tests.append(("iso (BIOS)", test_iso))
    if args.suite in ("all", "iso-uefi"):
        if OVMF_CODE.exists():
            tests.append(("iso (UEFI)", lambda: test_iso(uefi=True)))
        else:
            print(f"skip  iso (UEFI): {OVMF_CODE} not found")

    print(f"Running {len(tests)} boot tests ({'KVM' if USE_KVM else 'TCG emulation'})")
    failed = []
    started = time.monotonic()
    for name, fn in tests:
        t0 = time.monotonic()
        try:
            detail = fn()
            print(f"ok    {name} ({time.monotonic() - t0:.1f}s){': ' + detail if detail else ''}")
        except TestFailure as e:
            failed.append(name)
            print(f"FAIL  {name} ({time.monotonic() - t0:.1f}s)")
            print("      " + str(e).replace("\n", "\n      "))
    total = time.monotonic() - started
    print(f"\n{len(tests) - len(failed)} passed, {len(failed)} failed in {total:.1f}s")
    sys.exit(1 if failed else 0)


if __name__ == "__main__":
    main()
