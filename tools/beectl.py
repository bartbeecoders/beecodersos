#!/usr/bin/env python3
"""beectl: run a command on a BeeCodersOs machine, or take a picture of its screen,
through a running beehive (tools/beehive/beehive.py).

    beectl.py machines                       paired machines, and when each was last seen
    beectl.py run MACHINE "COMMAND" [--timeout 30]
                                             type the command in a shell on the machine and
                                             print what it showed; several lines (separated by
                                             \\n in the argument) are typed one after the other
    beectl.py screenshot MACHINE OUT.ppm [--scale 4]
                                             the machine's screen, every SCALE-th pixel
    beectl.py text MACHINE                   the text on the machine's screen
    beectl.py deploy MACHINE [--kernel K] [--timeout 300]
                                             put the kernel on offer, have the machine fetch it
                                             and restart (`update`, `reboot`), and wait for its
                                             report: healthy, or failed and rolled back. A machine
                                             with `update auto on` does it without being told

MACHINE is the name a machine was paired under (`beehive.py pair NAME`), or the
address of a VM's serial port as beehive shows it (127.0.0.1:4555 for vm1).
The machine must allow it: `ai control on`, typed on its own console.

It works through files in beehive's state folder (control/in, control/out), so it
needs no connection of its own and beehive stays the only door to a machine.
"""

import argparse
import base64
import json
import secrets
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def request(state: Path, machine: str, kind: str, wait: float, **fields) -> list[dict]:
    """Send one request and wait for the whole answer: the pieces, in order."""
    rid = secrets.token_hex(8)
    inbox, out = state / "control" / "in", state / "control" / "out"
    if not inbox.is_dir():
        raise SystemExit(f"{inbox} does not exist: is beehive running (with this --state-dir)?")
    tmp = inbox / f"{rid}.tmp"
    tmp.write_text(json.dumps({"rid": rid, "machine": machine, "type": kind, **fields}))
    tmp.rename(inbox / f"{rid}.json")
    deadline = time.monotonic() + wait + 5
    done, answer = out / f"{rid}.done", out / f"{rid}.jsonl"
    while not done.exists():
        if time.monotonic() > deadline:
            (inbox / f"{rid}.json").unlink(missing_ok=True)
            raise SystemExit(f"no answer from {machine} within {wait:.0f} s (is beehive running, and the machine connected?)")
        time.sleep(0.05)
    pieces = [json.loads(line) for line in answer.read_text().splitlines()] if answer.exists() else []
    for f in (done, answer):
        f.unlink(missing_ok=True)
    for piece in pieces:
        if piece.get("error"):
            raise SystemExit(f"{machine}: {piece['error']}")
    return pieces


def signals(state: Path) -> list[dict]:
    try:
        lines = (state / "signals.jsonl").read_text().splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        try:
            out.append(json.loads(line))
        except ValueError:
            pass
    return out


def deploy(state: Path, args) -> int:
    """Offer the kernel, nudge the machine, and wait for what it reports. 0: healthy."""
    build = args.build or int((ROOT / "BUILD_NUMBER").read_text())
    offer = [sys.executable, str(ROOT / "tools/beehive/beehive.py"), "--no-config", "--state-dir", str(state),
             "offer", str(args.kernel), "--build", str(build)]
    if subprocess.run(offer).returncode != 0:
        return 2
    seen = len(signals(state))
    # A machine on `update auto on` fetches it within a minute by itself; one that allows
    # remote control is told now. Neither is an error if it does not work: we wait either way.
    try:
        told = request(state, args.machine, "run", 90, cmd="update", timeout=90)
        text = "".join(p.get("text", "") for p in told)
        sys.stdout.write(text)
        if "on trial at the next start" in text:
            try:
                request(state, args.machine, "run", 5, cmd="reboot", timeout=5)
            except SystemExit:
                pass  # it restarted before it could answer
        elif "no newer kernel" in text or "installed already" in text or "is the kernel that is running" in text:
            print(f"{args.machine} already has b{build}")
            return 0
    except SystemExit as e:
        print(f"could not tell {args.machine} to update ({e}); waiting for it to fetch the kernel by itself")
    wait = 300 if args.timeout is None else args.timeout
    deadline = time.monotonic() + wait
    while time.monotonic() < deadline:
        for s in signals(state)[seen:]:
            text = str(s.get("text", ""))
            if s.get("target") != args.machine:
                continue
            if s.get("kind") == "health" and f"b{build} " in text + " ":
                print(f"{args.machine}: b{build} passed its health check and is now the good slot")
                return 0
            if s.get("kind") == "crash" and f"build b{build} " in text and "failed" in text:
                print(f"{args.machine}: {text}")
                return 1
        time.sleep(1)
    print(f"no report from {args.machine} about b{build} within {wait:.0f} s: look at it (beectl.py text {args.machine})")
    return 3


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("action", choices=["machines", "run", "screenshot", "text", "deploy"])
    ap.add_argument("machine", nargs="?")
    ap.add_argument("argument", nargs="?", help="run: the command; screenshot: the file to write")
    ap.add_argument("--timeout", type=float, default=None, help="run: seconds the command may take (30); "
                    "deploy: seconds to wait for the machine's report (300)")
    ap.add_argument("--kernel", type=Path, default=ROOT / "build" / "beecoders.elf", help="deploy: the kernel to offer")
    ap.add_argument("--build", type=int, help="deploy: its build number (default: BUILD_NUMBER)")
    ap.add_argument("--scale", type=int, default=4, help="screenshot: take every N-th pixel (1 = all)")
    ap.add_argument("--state-dir", type=Path, default=Path.home() / ".local/state/beehive")
    args = ap.parse_args()
    state = args.state_dir.expanduser()
    if args.action == "machines":
        try:
            machines = json.loads((state / "machines.json").read_text())
        except (OSError, ValueError):
            machines = {}
        for name, entry in sorted(machines.items()):
            print(f"{name:<16} last seen {entry.get('seen') or 'never'}")
        if not machines:
            print("no paired machines (pair one with: tools/beehive/beehive.py pair NAME)")
        return
    if not args.machine:
        ap.error("which machine?")
    if args.action == "deploy":
        sys.exit(deploy(state, args))
    args.timeout = 30 if args.timeout is None else args.timeout
    if args.action == "run":
        if args.argument is None:
            ap.error("which command?")
        command = args.argument.replace("\\n", "\n")
        pieces = request(state, args.machine, "run", args.timeout, cmd=command, timeout=int(args.timeout))
        sys.stdout.write("".join(p.get("text", "") for p in pieces))
        if any(p.get("timeout") for p in pieces):
            raise SystemExit(f"(the command did not finish within {args.timeout:.0f} s)")
    elif args.action == "text":
        pieces = request(state, args.machine, "screenshot", 30, scale=0)
        sys.stdout.write("".join(p.get("text", "") for p in pieces))
    else:
        if args.argument is None:
            ap.error("which file to write?")
        pieces = request(state, args.machine, "screenshot", 60, scale=max(1, args.scale))
        head = next((p for p in pieces if "w" in p), None)
        if head is None:
            raise SystemExit(f"{args.machine} sent no picture (a text screen? try: beectl.py text {args.machine})")
        pixels = b"".join(base64.b64decode(p.get("data", "")) for p in pieces)
        if len(pixels) != head["w"] * head["h"] * 3:
            raise SystemExit(f"the picture is incomplete ({len(pixels)} of {head['w'] * head['h'] * 3} bytes)")
        Path(args.argument).write_bytes(f"P6\n{head['w']} {head['h']}\n255\n".encode() + pixels)
        print(f"{args.argument}: {head['w']}x{head['h']} (every {head.get('scale', args.scale)}th pixel of the screen)")


if __name__ == "__main__":
    main()
