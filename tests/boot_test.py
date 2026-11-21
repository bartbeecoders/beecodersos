#!/usr/bin/env python3
"""BeeCodersOs boot tests: boot the kernel in headless QEMU and check it over
the serial port. Exits 0 when every test passes, 1 otherwise.

    tests/boot_test.py all                 everything below (what `make check` runs)
    tests/boot_test.py selftest            in-kernel self-tests (`selftest` boot flag)
    tests/boot_test.py scenarios [FILE..]  shell scenarios in tests/scenarios/*.scn
    tests/boot_test.py iso                 boot build/beecoders.iso (BIOS) and shut down; once more
                                           with a graphics card only the bootloader can set up
                                           (the `bootfb` display driver), checking the picture
    tests/boot_test.py iso-uefi            same with OVMF (skipped if OVMF is missing)

Options: -v (print all serial output), --no-kvm (pure emulation).

Scenario files (tests/scenarios/*.scn):
    # comment                  ignored
    @disk                      (first lines) boot with a fresh BeeFS disk instead of the RAM disk
    @sata                      (first lines, with @disk) attach the disk to an AHCI (SATA)
                               controller instead of the legacy IDE ports
    @cpus N                    (first lines) the machine has N processors instead of one
    @e1000e                    (first lines, with @net or @wire) the network card is an
                               e1000e (Intel 82574L) instead of the e1000
    @ai                        (first lines) attach COM2 and run beehive with its fake backend
    @ai-offline                (first lines) attach COM2 but run no beehive
    @ai-net                    (first lines, with @net) run beehive with its fake backend
                               listening on the network, with one paired machine: in a
                               command {hive} is its address (10.0.2.2:PORT), {hive-name}
                               the machine's name and {hive-key} its pairing key
    @hive stop|start           (with @ai-net) stop beehive, or start it again
    @offer [newer|none]        (with @ai or @ai-net) put the kernel on offer at beehive, as
                               `beehive.py offer` does; `newer`: with a build number one
                               higher, so that the OS takes it as an update; `none`: no offer
    @ctl ARGS                  (with @ai or @ai-net) run tools/beectl.py with these arguments
                               (remote control through beehive); {machine} is this machine's
                               name there, {tmp} a folder for files. What it prints is
                               checked like a command's output
    @restart                   the machine must restart by itself now (within 30 s); it is
                               booted again and the boot messages are checked as output
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
    @forward                   (first lines, with @net) let the test reach the OS's ports 80
                               (for `@request`) and 23 (for `@remote`) from the host
    @request METHOD PATH       (with @forward) send that HTTP request to the OS; the lines of
                               the answer (status line, headers, body) are checked like a
                               command's output. No answer: the one line "(no answer)".
                               `@request raw TEXT` sends TEXT as it is (\r \n allowed)
    @s3 [FLAGS] METHOD PATH [BODY]
                               (with @forward) send that request to the OS signed as an S3
                               client does (AWS Signature Version 4, access key `beecoders`,
                               secret S3_SECRET below: the scenario makes it the file
                               server's key); the answer's lines are checked like a command's
                               output. PATH is sent as written (so percent-encoded, with its
                               query); BODY may use \r \n. FLAGS: `wrong-secret` (signed
                               with another secret), `late` (signed an hour ago), `chunked`
                               (the body in signed aws-chunked pieces), `changed` (the body
                               is changed after signing), `copy-from=/BUCKET/KEY` (sent as
                               the signed header x-amz-copy-source: "copy that object")
    @capture NAME REGEX        remember group 1 of REGEX in the last command's output (or the
                               last `@request` / `@s3` answer): {NAME} in later commands and
                               `@remote`, `@request` and `@s3` lines becomes that text
    @remote LINE | LINE ...    (with @forward) connect to the OS's remote shell and type
                               these lines one by one (^C / ^D: those keys); everything it
                               sent is checked like a command's output. Telnet option bytes
                               are left out. No answer: the one line "(no answer)".
                               `@remote` alone only connects and listens
Without @wire or @net the machine has no network card.
    @net                       (first lines) connect the card to QEMU's built-in network: a
                               DHCP server (gives 10.0.2.15/24) and a gateway at 10.0.2.2
                               that answers pings; isolated from the host's network
    @dns NAME=IP ...           (first lines, with @net) run a fake DNS server on the host;
                               `{dns}` in a command becomes its address (10.0.2.2:PORT).
                               Other names do not exist; names starting with "drop." get
                               no answer at all
    @args WORDS                (first lines) extra words for the kernel command line, e.g.
                               `@args display=text` (not with @bootdisk)
    @vga MODEL                 (first lines) the graphics card: QEMU's `-vga` (std, qxl,
                               virtio, cirrus, none). Default: std
    @screenshot WxH KIND       take a picture of the screen (what a monitor would show): it
                               must be W by H pixels and show KIND: `text` (a mostly black
                               screen with something on it) or `bars` (the eight color bars
                               of `display test`), or `pixels X,Y=RRGGBB ...` (these pixels
                               have these colors). Waits up to 3 s for it
    @usb DEVICE ...            (first lines) a USB 3 controller (xHCI) with these devices
                               plugged in: `kbd`, `mouse`, `tablet`, `hub`, `storage`
    @keys KEY ...              press these keys on the machine's keyboard (not the serial
                               port), one after the other. Names as QEMU has them: a, 1, ret,
                               spc, shift+a, ... `KEY:MS` holds a key for that long. Follow
                               with `$wait` to check what the shell printed
    @mouse DX DY               move the mouse that far right and down (pixels)
    @button NAME down|up       press or let go a mouse button (left, right, middle)
    @install-medium [WORDS]    (first lines) boot as from the install medium (the ISO or the
                               USB stick): the kernel, plus the install image the `install`
                               command writes to a disk. Made for the test with
                               tools/bootdisk.py; systems installed from it run in test mode.
                               WORDS: more kernel arguments for the installed system
    @blank-disk MIB [foreign]  (first lines) attach a disk of that size: empty, or with
                               `foreign` holding another system's partition and some data
    @boot-disk [crash]         shut down, and from now on boot from the disk (what the
                               install put there); the boot messages are checked as output.
                               `crash`: that boot must crash the kernel; it is booted again
    @boot-medium [newer]       shut down, and boot from the install medium again (the disk
                               stays attached). `newer`: a medium whose build number is one
                               higher, for an upgrade
    @disk-untouched            the disk holds, byte for byte, what it held at the start
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
    @frame-soon HEXREGEX       like @frame, but the frame must arrive within 1 s
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
import json
import os
import re
import shlex
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
                 wire=None, usernet=None, forward=None, forward_shell=None, sata=False, nic="e1000",
                 extra_args=None, vga=None, qmp=None, usb=None, initrd=None, cpus=None):
        # The ISO carries the install image (17 MiB), which the bootloader loads into
        # memory: under UEFI that needs more than 128 MiB.
        cmd = ["qemu-system-x86_64", "-m", "256M" if cdrom else "128M", "-display", "none", "-serial", "stdio",
               "-monitor", "none", "-no-reboot",
               "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"]
        if USE_KVM:
            cmd += ["-enable-kvm", "-cpu", "host"]
        if cpus:
            cmd += ["-smp", str(cpus)]
        if cdrom:
            cmd += ["-cdrom", str(cdrom)]
        elif bootdisk and sata:  # the same, from a SATA disk
            cmd += ["-machine", "q35", "-drive", f"file={bootdisk},format=raw,if=none,id=sata0",
                    "-device", "ide-hd,drive=sata0,bus=ide.0"]
        elif bootdisk:  # Limine on the disk picks the kernel slot
            cmd += ["-drive", f"file={bootdisk},format=raw,if=ide,index=0,media=disk"]
        else:
            args = kernel_args or ("test nodhcp" if wire else "test")
            cmd += ["-kernel", str(KERNEL), "-append", f"{args} {extra_args}" if extra_args else args]
            if initrd:  # a file the bootloader loads next to the kernel: the install image
                cmd += ["-initrd", str(initrd)]
        if vga:
            cmd += ["-vga", vga]
        if usb is not None:
            cmd += ["-device", "qemu-xhci"]
            for device in usb:
                if device == "storage":
                    cmd += ["-drive", "if=none,id=stick,format=raw,file=/dev/null,readonly=on",
                            "-device", "usb-storage,drive=stick"]
                else:
                    cmd += ["-device", f"usb-{device}"]
        self.qmp = qmp
        if qmp:  # QEMU's control port, for screenshots, keys and the mouse
            cmd += ["-qmp", f"tcp:127.0.0.1:{qmp},server=on,wait=off"]
        if disk and sata:
            cmd += ["-device", "ahci,id=ahci", "-drive", f"file={disk},format=raw,if=none,id=sata0",
                    "-device", "ide-hd,drive=sata0,bus=ahci.0"]
        elif disk:
            cmd += ["-drive", f"file={disk},format=raw,if=ide,index=0,media=disk"]
        if uefi:
            cmd += ["-drive", f"if=pflash,format=raw,readonly=on,file={OVMF_CODE}"]
        if wire:  # e1000 on a UDP "wire": each datagram is one Ethernet frame
            ours, theirs = wire
            cmd += ["-netdev", f"socket,id=n0,udp=127.0.0.1:{ours},localaddr=127.0.0.1:{theirs}",
                    "-device", f"{nic},netdev=n0,mac={WIRE_MAC}"]
        if usernet:  # QEMU's user-mode network; "isolated": cut off from the host and beyond
            restrict = "on" if usernet == "isolated" else "off"
            fwd = f",hostfwd=tcp:127.0.0.1:{forward}-:80" if forward else ""
            fwd += f",hostfwd=tcp:127.0.0.1:{forward_shell}-:23" if forward_shell else ""
            cmd += ["-netdev", f"user,id=n0,restrict={restrict}{fwd}", "-device", f"{nic},netdev=n0,mac={WIRE_MAC}"]
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

    def control(self, execute: str, **arguments):
        """One command to QEMU itself (QMP), e.g. a screenshot or a key press."""
        with socket.create_connection(("127.0.0.1", self.qmp), timeout=5) as conn:
            f = conn.makefile("rw")
            f.readline()  # QEMU's greeting
            for request in ({"execute": "qmp_capabilities"}, {"execute": execute, "arguments": arguments}):
                f.write(json.dumps(request) + "\n")
                f.flush()
                while "return" not in (answer := json.loads(f.readline())):  # skip events
                    if "error" in answer:
                        raise TestFailure(f"{execute}: QEMU says {answer['error'].get('desc')}")

    def screenshot(self, path: Path) -> tuple[int, int, bytes]:
        """What the screen shows now: (width, height, RGB bytes, 3 per pixel, row by row)."""
        self.control("screendump", filename=str(path))
        data = path.read_bytes()  # PPM: "P6\nWIDTH HEIGHT\n255\n", then the pixels
        magic, width, height, depth = data.split(maxsplit=4)[:4]
        width, height = int(width), int(height)
        pixels = data[-3 * width * height:]
        if magic != b"P6" or depth != b"255" or len(pixels) != 3 * width * height:
            raise TestFailure("screenshot: QEMU wrote something that is not a PPM picture")
        return width, height, pixels

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


BAR_COLORS = ["ffffff", "ffff55", "55ffff", "55ff55", "ff55ff", "ff5555", "5555ff", "000000"]  # fbcon::BARS


def check_screenshot(vm: Vm, tmp: Path, spec: str, where: str):
    """`@screenshot WxH KIND`: retry for 3 s, the screen may still be changing."""
    m = re.fullmatch(r"(\d+)x(\d+) (text|bars|pixels((?: \d+,\d+=[0-9a-f]{6})+))", spec)
    if not m:
        raise TestFailure(f"{where}: @screenshot wants WIDTHxHEIGHT and text, bars or pixels X,Y=RRGGBB, not `{spec}`")
    want_w, want_h, kind = int(m[1]), int(m[2]), m[3].split()[0]
    points = [(int(x), int(y), color) for x, y, color in re.findall(r"(\d+),(\d+)=(\w+)", m[4] or "")]
    deadline = time.monotonic() + 3
    while True:
        width, height, pixels = vm.screenshot(tmp / "screen.ppm")
        pixel = lambda x, y: pixels[3 * (y * width + x):3 * (y * width + x) + 3].hex()
        if (width, height) != (want_w, want_h):
            problem = f"the screen is {width}x{height}"
        elif kind == "pixels":
            wrong = [f"{x},{y} is {pixel(x, y)}, not {color}" for x, y, color in points
                     if x >= width or y >= height or pixel(x, y) != color]
            problem = "; ".join(wrong) or None
        elif kind == "bars":
            seen = [[pixel(width * (2 * i + 1) // 16, y) for i in range(8)] for y in (0, height // 2, height - 1)]
            problem = None if all(row == BAR_COLORS for row in seen) else f"no color bars: the colors are {seen[1]}"
        else:
            lit = sum(1 for i in range(0, len(pixels), 3) if pixels[i:i + 3] != b"\0\0\0")
            share = lit / (width * height)
            problem = None if 0.0005 < share < 0.5 else f"{share:.2%} of the pixels are lit: not a text screen"
        if not problem:
            return
        if time.monotonic() > deadline:
            raise TestFailure(f"{where}: @screenshot {spec}: {problem}")
        time.sleep(0.3)


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
        if line in ("@disk", "@ai", "@ai-offline", "@ai-net", "@bootdisk", "@wire", "@net", "@forward", "@sata", "@e1000e"):
            opts.add(line[1:])
        elif line.startswith("@dns "):
            opts.add("dns:" + line[5:])
        elif line == "@install-medium" or line.startswith(("@install-medium ", "@blank-disk ")):
            name, _, arg = line[1:].partition(" ")
            opts.add(f"{name}:{arg.strip()}")
        elif line in ("@boot-disk", "@boot-disk crash", "@disk-untouched", "@boot-medium") or line.startswith("@boot-medium "):
            kind, _, arg = line[1:].partition(" ")
            steps.append((kind, arg.strip() or f"({kind})", []))
        elif line.startswith(("@args ", "@vga ", "@cpus ")):
            opts.add(line[1:].replace(" ", ":", 1))
        elif line.startswith("@screenshot "):
            steps.append(("screenshot", line[12:].strip(), []))
        elif line.startswith("@usb"):
            opts.add("usb:" + line[4:].strip())
        elif line == "@restart" or line == "@offer":
            steps.append((line[1:], "", []))
        elif line.startswith(("@keys ", "@mouse ", "@button ", "@hive ", "@offer ", "@ctl ")):
            kind, _, arg = line[1:].partition(" ")
            steps.append((kind, arg.strip(), []))
        elif line.startswith(("@frame ", "@frame-soon ", "@send ")):
            kind, _, arg = line[1:].partition(" ")
            steps.append((kind, arg.strip(), []))
        elif line.startswith("@deploy"):
            kind, _, extra = line[1:].partition(" ")
            steps.append((kind, extra.strip(), []))
        elif line.startswith("@request "):
            steps.append(("request", line[9:].strip(), []))
        elif line.startswith("@s3 "):
            steps.append(("s3", line[4:].strip(), []))
        elif line == "@remote" or line.startswith("@remote "):
            steps.append(("remote", line[8:].strip(), []))
        elif line.startswith("@capture "):
            steps.append(("capture", line[9:].strip(), []))
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
    def lines(conn, lines):
        # Read what the client sent first: closing with unread data makes the
        # host answer with a reset instead of a normal end, which now and then
        # cut the transfer short (the OS then rightly said "connection reset").
        lines.readline()
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

    def expect(self, pattern: str, where: str, timeout: float = 3):
        rx, deadline = re.compile(pattern), time.monotonic() + timeout
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


def http_request(port: int, request: str) -> list[str]:
    """Send a request to the OS's web server (see @request); the answer's lines."""
    if request.startswith("raw "):
        data = request[4:].encode().decode("unicode_escape").encode("latin-1")
    else:
        data = f"{request} HTTP/1.1\r\nHost: test\r\n\r\n".encode()
    return http_exchange(port, data)


# The secret key `@s3` signs with; the access key id is the OS's fixed one.
S3_ACCESS_KEY = "beecoders"
S3_SECRET = "TestKeyTestKeyTestKeyTestKeyTestKey12345"


def s3_signed(port: int, spec: str) -> bytes:
    """The bytes of an S3 request (see @s3), signed with AWS Signature Version 4."""
    import hashlib
    import hmac
    import urllib.parse
    words = spec.split(" ")
    flags = set()
    copy_from = None
    while words and (words[0] in ("wrong-secret", "late", "chunked", "changed") or words[0].startswith("copy-from=")):
        word = words.pop(0)
        if word.startswith("copy-from="):
            copy_from = word[len("copy-from="):]
        else:
            flags.add(word)
    method, target = words[0], words[1]
    body = " ".join(words[2:]).encode().decode("unicode_escape").encode("latin-1")
    path, _, query = target.partition("?")
    secret = "not-the-secret" if "wrong-secret" in flags else S3_SECRET
    now = time.gmtime(time.time() - (3600 if "late" in flags else 0))
    amz_date, date = time.strftime("%Y%m%dT%H%M%SZ", now), time.strftime("%Y%m%d", now)
    scope = f"{date}/us-east-1/s3/aws4_request"

    def sign(key: bytes, text: str) -> bytes:
        return hmac.new(key, text.encode(), hashlib.sha256).digest()

    key = sign(sign(sign(sign(("AWS4" + secret).encode(), date), "us-east-1"), "s3"), "aws4_request")
    headers = {"host": f"127.0.0.1:{port}", "x-amz-date": amz_date}
    if copy_from:
        headers["x-amz-copy-source"] = copy_from
    if "chunked" in flags:
        headers["x-amz-content-sha256"] = "STREAMING-AWS4-HMAC-SHA256-PAYLOAD"
        headers["x-amz-decoded-content-length"] = str(len(body))
        headers["content-encoding"] = "aws-chunked"
    else:
        headers["x-amz-content-sha256"] = hashlib.sha256(body).hexdigest()
    quote = lambda text: urllib.parse.quote(urllib.parse.unquote(text), safe="-_.~")
    pairs = sorted((quote(n), quote(v)) for n, _, v in (p.partition("=") for p in query.split("&") if p))
    canonical = "\n".join([
        method, path, "&".join(f"{n}={v}" for n, v in pairs),
        "".join(f"{n}:{headers[n]}\n" for n in sorted(headers)), ";".join(sorted(headers)),
        headers["x-amz-content-sha256"],
    ])
    to_sign = f"AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{hashlib.sha256(canonical.encode()).hexdigest()}"
    signature = sign(key, to_sign).hex()
    if "chunked" in flags:
        # Pieces of 5 bytes, each signed with the one before it, then an empty one.
        sent, previous, empty = b"", signature, hashlib.sha256(b"").hexdigest()
        for piece in [body[i:i + 5] for i in range(0, len(body), 5)] + [b""]:
            previous = sign(key, f"AWS4-HMAC-SHA256-PAYLOAD\n{amz_date}\n{scope}\n{previous}\n{empty}\n"
                                 f"{hashlib.sha256(piece).hexdigest()}").hex()
            sent += f"{len(piece):x};chunk-signature={previous}\r\n".encode() + piece + b"\r\n"
        body = sent
    if "changed" in flags:
        body = body.replace(b"e", b"E")
    head = f"{method} {target} HTTP/1.1\r\n" + "".join(f"{n}: {v}\r\n" for n, v in headers.items())
    head += (f"Authorization: AWS4-HMAC-SHA256 Credential={S3_ACCESS_KEY}/{scope}, "
             f"SignedHeaders={';'.join(sorted(headers))}, Signature={signature}\r\n")
    if body or method in ("PUT", "POST"):
        head += f"Content-Length: {len(body)}\r\n"
    return head.encode() + b"\r\n" + body


def http_exchange(port: int, data: bytes) -> list[str]:
    """Send these bytes to the OS's forwarded port; the answer's lines."""
    answer = b""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=5) as conn:
            conn.settimeout(8)
            conn.sendall(data)
            while piece := conn.recv(65536):
                answer += piece
    except OSError:
        pass
    if not answer:
        return ["(no answer)"]
    return [line.rstrip("\r") for line in answer.decode(errors="replace").split("\n")]


def remote_talk(port: int, items: list[str]) -> list[str]:
    """Type lines into the OS's remote shell (see @remote); the lines of all it sent."""
    answer = b""
    try:
        with socket.create_connection(("127.0.0.1", port), timeout=5) as conn:
            def listen(quiet: float, first: float) -> bool:
                """Read until nothing came for `quiet` s (waiting up to `first` s for
                the first byte). False when the OS closed the connection."""
                nonlocal answer
                conn.settimeout(first)
                while True:
                    try:
                        piece = conn.recv(65536)
                    except TimeoutError:
                        return True
                    if not piece:
                        return False
                    answer += piece
                    conn.settimeout(quiet)

            is_open = listen(0.5, 5)
            for item in items:
                if not is_open:
                    break
                conn.sendall({"^C": b"\x03", "^D": b"\x04"}.get(item, item.encode() + b"\r\n"))
                is_open = listen(0.5, 5)
            if is_open:
                listen(1.5, 1.5)
    except OSError:
        pass
    answer = re.sub(rb"\xff[\xfb-\xfe].", b"", answer)  # Telnet options
    if not answer:
        return ["(no answer)"]
    return [line.rstrip("\r") for line in answer.decode(errors="replace").split("\n")]


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
        start_hive = None
        if "ai-net" in opts:  # beehive's door on the network, one machine paired
            state_dir, hive_port = Path(tmp) / "beehive", free_port()
            run_tool([sys.executable, str(BEEHIVE), "--no-config", "--state-dir", str(state_dir), "pair", "test"])
            hive_key = json.loads((state_dir / "machines.json").read_text())["test"]["key"]
            start_hive = lambda: subprocess.Popen(
                [sys.executable, str(BEEHIVE), "--no-config", "--backend", "fake", "--connect", "none",
                 "--listen", f"127.0.0.1:{hive_port}", "--state-dir", str(state_dir)],
                stdout=subprocess.PIPE if not VERBOSE else None, stderr=subprocess.STDOUT)
            hive = start_hive()
            time.sleep(0.5)  # let it open its port before the OS boots
        wire = Wire() if "wire" in opts else None
        dns_table = next((o[4:] for o in opts if o.startswith("dns:")), None)
        dns = FakeDns(dns_table) if dns_table is not None else None
        tcp = FakeTcp() if "tcp" in opts else None
        names = {"{dns}": f"10.0.2.2:{dns.port}"} if dns else {}
        if start_hive:
            names.update({"{hive}": f"10.0.2.2:{hive_port}", "{hive-name}": "test", "{hive-key}": hive_key})
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
        usernet = ("open" if servers or start_hive else "isolated") if "net" in opts else None
        forward = free_port() if "forward" in opts else None
        forward_shell = free_port() if "forward" in opts else None
        extra_args = next((o[5:] for o in opts if o.startswith("args:")), None)
        cpus = next((int(o[5:]) for o in opts if o.startswith("cpus:")), None)
        if extra_args and bootdisk:
            raise TestFailure(f"{path.name}: @args does not work with @bootdisk")
        vga = next((o[4:] for o in opts if o.startswith("vga:")), None)
        usb = next((o[4:].split() for o in opts if o.startswith("usb:")), None)
        qmp = free_port() if any(kind in ("screenshot", "keys", "mouse", "button") for kind, _, _ in steps) else None
        # Install tests: a blank (or foreign) disk, and install media made for the test.
        blank = next((o[11:].split() for o in opts if o.startswith("blank-disk:")), None)
        medium_words = next((o[15:] for o in opts if o.startswith("install-medium:")), None)
        media, original, mode = {}, b"", {"disk": False, "medium": ""}
        if blank:
            disk = Path(tmp) / "disk.img"
            with open(disk, "wb") as f:
                f.truncate(int(blank[0]) * 1024 * 1024)
                if blank[1:] == ["foreign"]:  # one Linux partition (type 0x83) with something in it
                    table = bytearray(512)
                    table[446:462] = bytes([0x80, 0xFE, 0xFF, 0xFF, 0x83, 0xFE, 0xFF, 0xFF]) + (2048).to_bytes(4, "little") \
                        + (int(blank[0]) * 2048 - 2048).to_bytes(4, "little")
                    table[510:512] = b"\x55\xAA"
                    f.write(table)
                    f.seek(2048 * 512)
                    f.write(b"somebody's files\n" * 1000)
            original = disk.read_bytes()

        def medium(kind: str) -> Path:
            """An install image, as `make` builds build/install.img, with test mode on the
            installed system; "newer": with a build number one higher."""
            if kind not in media:
                slot = ROOT / "build" / "kernel-slot.elf"
                if not slot.exists():
                    raise TestFailure(f"{slot} missing: run `make build/install.img`")
                build = int((ROOT / "BUILD_NUMBER").read_text()) + (1 if kind == "newer" else 0)
                whole = Path(tmp) / f"medium-{kind or 'plain'}.disk"
                run_tool([sys.executable, str(BOOTDISK), "create", str(whole), "--kernel", str(slot), "--size-mb", "20",
                          "--build", str(build), "--cmdline", f"test {medium_words or ''}".strip()])
                media[kind] = Path(tmp) / f"medium-{kind or 'plain'}.img"
                media[kind].write_bytes(whole.read_bytes()[:17 * 1024 * 1024])
                whole.unlink()
            return media[kind]

        def boot():
            if mode["disk"]:
                return Vm(bootdisk=disk, sata="sata" in opts, vga=vga, qmp=qmp, usb=usb)
            return Vm(disk=disk, ai_port=ai_port, bootdisk=bootdisk, wire=wire.ports if wire else None,
                      usernet=usernet, forward=forward, forward_shell=forward_shell, sata="sata" in opts,
                      nic="e1000e" if "e1000e" in opts else "e1000", extra_args=extra_args, vga=vga, qmp=qmp,
                      usb=usb, initrd=medium(mode["medium"]) if medium_words is not None else None, cpus=cpus)

        vm = boot()
        out = []
        try:
            boot_to_prompt(vm)
            if wire:
                wire.booted()
            commands = 0
            for kind, cmd, expectations in steps:
                if kind in ("frame", "frame-soon", "send"):
                    if not wire:
                        raise TestFailure(f"{path.name}: @{kind} needs @wire")
                    if kind == "send":
                        wire.send(cmd)
                    else:
                        wire.expect(cmd, path.name, timeout=3 if kind == "frame" else 1)
                    continue
                if kind == "screenshot":
                    check_screenshot(vm, Path(tmp), cmd, path.name)
                    continue
                if kind in ("offer", "ctl"):
                    state_dir = Path(tmp) / "beehive"
                    if not hive and not start_hive:
                        raise TestFailure(f"{path.name}: @{kind} needs @ai or @ai-net")
                    if kind == "offer":
                        build = int((ROOT / "BUILD_NUMBER").read_text()) + (1 if cmd == "newer" else 0)
                        what = ["none"] if cmd == "none" else [str(KERNEL), "--build", str(build)]
                        run_tool([sys.executable, str(BEEHIVE), "--no-config", "--state-dir", str(state_dir), "offer", *what])
                        continue
                    machine = "test" if start_hive else f"127.0.0.1:{ai_port}"
                    words = [w.replace("{machine}", machine).replace("{tmp}", tmp) for w in shlex.split(cmd)]
                    done = subprocess.run([sys.executable, str(ROOT / "tools" / "beectl.py"), "--state-dir", str(state_dir), *words],
                                          capture_output=True, text=True, timeout=120)
                    out = (done.stdout + done.stderr).splitlines()
                    check_output(path, f"@ctl {cmd}", out, expectations)
                    continue
                if kind == "restart":
                    status = vm.wait_exit(30)
                    if status != 0:
                        raise TestFailure(f"{path.name}: the machine should restart by itself, QEMU exit status {status}\n{tail(vm.text())}")
                    vm = boot()
                    out = boot_to_prompt(vm)
                    check_output(path, "(restart)", out, expectations)
                    continue
                if kind == "hive":
                    if not start_hive:
                        raise TestFailure(f"{path.name}: @hive needs @ai-net")
                    if hive and hive.poll() is None:
                        hive.kill()
                        hive.wait()
                    hive = start_hive() if cmd == "start" else None
                    time.sleep(0.5)
                    continue
                if kind == "keys":
                    for key in cmd.split():
                        key, _, hold = key.partition(":")
                        keys = [{"type": "qcode", "data": name} for name in key.split("+")]
                        vm.control("send-key", keys=keys, **({"hold-time": int(hold)} if hold else {}))
                        time.sleep(int(hold) / 1000 + 0.15 if hold else 0.12)
                    continue
                if kind in ("mouse", "button"):
                    a, b = cmd.split()
                    if kind == "mouse":
                        events = [{"type": "rel", "data": {"axis": axis, "value": int(v)}} for axis, v in (("x", a), ("y", b))]
                    else:
                        events = [{"type": "btn", "data": {"button": a, "down": b == "down"}}]
                    vm.control("input-send-event", events=events)
                    time.sleep(0.25)
                    continue
                if kind == "request":
                    if not forward:
                        raise TestFailure(f"{path.name}: @request needs @forward")
                    for name, value in names.items():
                        cmd = cmd.replace(name, value)
                    out = http_request(forward, cmd)
                    check_output(path, f"@request {cmd}", out, expectations)
                    continue
                if kind == "s3":
                    if not forward:
                        raise TestFailure(f"{path.name}: @s3 needs @forward")
                    for name, value in names.items():
                        cmd = cmd.replace(name, value)
                    out = http_exchange(forward, s3_signed(forward, cmd))
                    check_output(path, f"@s3 {cmd}", out, expectations)
                    continue
                if kind == "remote":
                    if not forward_shell:
                        raise TestFailure(f"{path.name}: @remote needs @forward")
                    for name, value in names.items():
                        cmd = cmd.replace(name, value)
                    items = [item.strip() for item in cmd.split("|")] if cmd else []
                    check_output(path, "@remote", remote_talk(forward_shell, items), expectations)
                    continue
                if kind == "capture":
                    name, _, pattern = cmd.partition(" ")
                    found = next((m for m in (re.search(pattern, line) for line in out) if m), None)
                    if not found:
                        raise TestFailure(f"{path.name}: @capture {name}: nothing in the last output matches {pattern}")
                    names["{%s}" % name] = found.group(1)
                    continue
                if kind == "disk-untouched":
                    vm.shutdown()  # so that everything the OS wrote has reached the file
                    if disk.read_bytes() != original:
                        raise TestFailure(f"{path.name}: the disk was changed")
                    vm = boot()
                    boot_to_prompt(vm)
                    continue
                if kind in ("reboot", "boot-disk", "boot-medium"):
                    vm.shutdown()
                    if kind != "reboot":
                        mode["disk"], mode["medium"] = kind == "boot-disk", "newer" if cmd == "newer" else ""
                    vm = boot()
                    if kind == "boot-disk" and cmd == "crash":
                        status = vm.wait_exit(BOOT_TIMEOUT)
                        if status != EXIT_FAILURE:
                            raise TestFailure(f"{path.name}: the kernel on trial should crash (exit {EXIT_FAILURE}), "
                                              f"QEMU exit status {status}\n{tail(vm.text())}")
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


def test_iso(uefi=False, bootfb=False):
    """Boot the ISO through Limine. With `bootfb` the graphics card is a Cirrus: the kernel
    has no driver for it, so the screen must come from the framebuffer the bootloader set
    up (VBE under BIOS, GOP under UEFI), and the desktop must really be on it."""
    if not ISO.exists():
        raise TestFailure(f"{ISO} missing: run `make iso`")
    vm = Vm(cdrom=ISO, uefi=uefi, vga="cirrus" if bootfb else None, qmp=free_port() if bootfb else None)
    try:
        boot_to_prompt(vm)
        out = vm.command("version")
        if not any(l.startswith("BeeCodersOs ") for l in out):
            raise TestFailure("`version` printed:\n" + "\n".join(out))
        if bootfb:
            shown = vm.command("display")
            mode = next((l for l in shown if l.startswith("mode: ")), "no mode")
            if "driver: bootfb" not in shown:
                raise TestFailure("`display` should show the bootfb driver, it printed:\n" + "\n".join(shown))
            with tempfile.TemporaryDirectory(prefix="beetest-") as tmp:
                deadline = time.monotonic() + 3
                while True:
                    width, height, pixels = vm.screenshot(Path(tmp) / "screen.ppm")
                    corners = {pixels[3 * i:3 * i + 3].hex() for i in (0, width - 1, width * height - 1)}
                    if f"mode: {width}x{height}," in mode and corners == {"a8d0f0"}:  # wm::BACKGROUND
                        break
                    if time.monotonic() > deadline:
                        raise TestFailure(f"the screen is {width}x{height} with corners {sorted(corners)}; expected "
                                          f"the desktop background (a8d0f0) and `{mode}`")
                    time.sleep(0.3)
            vm.shutdown()
            return f"bootfb, {mode}"
        vm.shutdown()
        return out[0]
    finally:
        vm.kill()


def test_iso_install(uefi=False):
    """The real install medium: boot the ISO with an empty SATA disk, install, then boot
    the disk without the ISO, under the same firmware."""
    if not ISO.exists():
        raise TestFailure(f"{ISO} missing: run `make iso`")
    with tempfile.TemporaryDirectory(prefix="beetest-") as tmp:
        disk = Path(tmp) / "disk.img"
        with open(disk, "wb") as f:
            f.truncate(64 * 1024 * 1024)
        vm = Vm(cdrom=ISO, uefi=uefi, disk=disk, sata=True)
        try:
            boot_to_prompt(vm)
            listed = vm.command("install")
            if not any(re.fullmatch(r"This install medium carries BeeCodersOs b\d+\.", l) for l in listed) \
                    or not any("sata0" in l and l.endswith(": empty") for l in listed):
                raise TestFailure("`install` should show the medium's build and an empty disk:\n" + "\n".join(listed))
            vm.send("install sata0")
            time.sleep(0.5)
            done = vm.command("yes")
            if not any(l.startswith("Installed on sata0.") for l in done):
                raise TestFailure("the install did not finish:\n" + "\n".join(done))
            vm.shutdown()
            vm = Vm(bootdisk=disk, uefi=uefi, sata=True)
            boot = boot_to_prompt(vm)
            slots = vm.command("slots")
            where = vm.command("pwd")
            if "good slot:   a" not in slots or where != ["/data"]:
                raise TestFailure("the installed disk did not come up as expected:\n" + "\n".join(boot + slots + where))
            vm.shutdown()
            return "installed and booted from the disk"
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
        tests.append(("iso (BIOS, bootloader's framebuffer)", lambda: test_iso(bootfb=True)))
        tests.append(("iso (BIOS, install to a disk and boot it)", test_iso_install))
    if args.suite in ("all", "iso-uefi"):
        if OVMF_CODE.exists():
            tests.append(("iso (UEFI)", lambda: test_iso(uefi=True)))
            tests.append(("iso (UEFI, bootloader's framebuffer)", lambda: test_iso(uefi=True, bootfb=True)))
            tests.append(("iso (UEFI, install to a disk and boot it)", lambda: test_iso_install(uefi=True)))
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
