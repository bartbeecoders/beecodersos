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
    $ COMMAND                  type a command, wait for the next prompt
    $! COMMAND                 the command must crash the kernel; boots again (same disk)
    $^C COMMAND                type the command, press Ctrl+C after 1 s, wait for the prompt
    TEXT                       an output line of the last command must equal TEXT
    ~ REGEX                    an output line must match REGEX (re.search)
    !~ REGEX                   no output line may match REGEX
    @reboot                    shut down, boot again with the same disk
    @signal REGEX              (with @ai) beehive must have received a signal matching REGEX
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
BEEHIVE = ROOT / "tools" / "beehive" / "beehive.py"
BEEFS_TOOL = ROOT / "tools" / "beefs-tool" / "target" / "release" / "beefs-tool"
SCENARIO_DIR = ROOT / "tests" / "scenarios"
OVMF_CODE = Path("/usr/share/edk2/x64/OVMF_CODE.4m.fd")

EXIT_SUCCESS = (0x10 << 1) | 1  # testmode::SUCCESS through isa-debug-exit
EXIT_FAILURE = (0x11 << 1) | 1

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

    def __init__(self, *, kernel_args=None, cdrom=None, disk=None, uefi=False, ai_port=None):
        cmd = ["qemu-system-x86_64", "-m", "128M", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-no-reboot",
               "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]
        if USE_KVM:
            cmd += ["-enable-kvm", "-cpu", "host"]
        if cdrom:
            cmd += ["-cdrom", str(cdrom)]
        else:
            cmd += ["-kernel", str(KERNEL), "-append", kernel_args or "test"]
        if disk:
            cmd += ["-drive", f"file={disk},format=raw,if=ide,index=0,media=disk"]
        if uefi:
            cmd += ["-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}"]
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
        if line in ("@disk", "@ai", "@ai-offline"):
            opts.add(line[1:])
        elif line == "@reboot":
            steps.append(("reboot", "(reboot)", []))
        elif line.startswith("@signal "):
            signals.append((re.compile(line[8:]), n))
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
        ai_port = free_port() if opts & {"ai", "ai-offline"} else None
        hive = None
        if "ai" in opts:
            hive = subprocess.Popen(
                [sys.executable, str(BEEHIVE), "--no-config", "--backend", "fake", "--retry", "0.1",
                 "--connect", f"127.0.0.1:{ai_port}", "--state-dir", str(Path(tmp) / "beehive")],
                stdout=subprocess.PIPE if not VERBOSE else None, stderr=subprocess.STDOUT)
        boot = lambda: Vm(disk=disk, ai_port=ai_port)
        vm = boot()
        try:
            boot_to_prompt(vm)
            commands = 0
            for kind, cmd, expectations in steps:
                if kind == "reboot":
                    vm.shutdown()
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
                else:
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
