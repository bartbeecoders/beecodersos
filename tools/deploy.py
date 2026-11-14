#!/usr/bin/env python3
"""deploy: put a new kernel on the libvirt VM's boot disk, with a watchdog.

    deploy.py setup-vm      one time: give vm1 the A/B boot disk (build/beecoders.img)
    deploy.py               deploy build/beecoders.elf to the spare slot and watch it boot

How a deploy goes:
1. Stop the VM; write the kernel into the spare slot (tools/bootdisk.py deploy).
2. Start the VM. The trial kernel arms the rollback, boots, runs its health
   check and sends a `health` signal to beehive (or a `crash` signal when it
   failed and the good slot took over).
3. The watchdog waits for that signal. If nothing arrives in time (for
   example a kernel that dies before it can arm the rollback), it stops the VM,
   points the boot disk back at the good slot and starts it again.

The watchdog needs beehive running (`make beehive`) to hear the VM; without
it, deploy only reports what it did and you check with `slots` in the VM.
"""

import argparse
import json
import shutil
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import bootdisk  # noqa: E402

IMAGE = ROOT / "build" / "beecoders.img"
KERNEL = ROOT / "build" / "beecoders.elf"
BEEHIVE_STATE = Path.home() / ".local/state/beehive"


def say(msg):
    print(f"deploy: {msg}", flush=True)


class Virsh:
    def __init__(self, uri: str, vm: str, command: str = "virsh"):
        self.base, self.vm = [command, "-c", uri], vm

    def run(self, *args, check=True) -> str:
        r = subprocess.run(self.base + list(args), capture_output=True, text=True)
        if check and r.returncode != 0:
            raise SystemExit(f"virsh {' '.join(args)}: {r.stderr.strip()}")
        return r.stdout.strip()

    def state(self) -> str:
        return self.run("domstate", self.vm)

    def stop(self):
        if self.state() != "shut off":
            self.run("destroy", self.vm, check=False)
        for _ in range(50):
            if self.state() == "shut off":
                return
            time.sleep(0.1)
        raise SystemExit(f"{self.vm} did not stop")

    def start(self):
        self.run("start", self.vm)


def beehive_running() -> bool:
    return subprocess.run(["pgrep", "-f", "beehive.py"], capture_output=True).returncode == 0


def read_signals(state_dir: Path, since: str) -> list[dict]:
    try:
        lines = (state_dir / "signals.jsonl").read_text().splitlines()
    except OSError:
        return []
    out = []
    for line in lines:
        try:
            s = json.loads(line)
        except ValueError:
            continue
        if s.get("received", "") >= since:
            out.append(s)
    return out


def verdict(signals: list[dict], build: int) -> str | None:
    """'healthy', 'rolled-back' (by the OS) or None (no news yet)."""
    for s in signals:
        text = str(s.get("text", ""))
        if s.get("kind") == "health" and f"b{build} " in text + " ":
            return "healthy"
        if s.get("kind") == "crash" and f"build b{build} " in text and "failed" in text:
            return "rolled-back"
    return None


def cmd_deploy(args):
    virsh = Virsh(args.uri, args.vm, args.virsh)
    image, kernel = Path(args.image), Path(args.kernel)
    if not image.exists():
        raise SystemExit(f"{image} missing: run `make bootdisk` and `deploy.py setup-vm` first")
    build = args.build or bootdisk.current_build()
    say(f"stopping {args.vm}")
    virsh.stop()
    subprocess.run([sys.executable, str(ROOT / "tools/bootdisk.py"), "deploy", str(image), "--kernel", str(kernel),
                    "--build", str(build)], check=True)
    since = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    say(f"starting {args.vm} with build b{build} on trial")
    virsh.start()
    if args.no_watch:
        return say("not watching (--no-watch): check with `slots` in the VM")
    if not beehive_running():
        return say("beehive is not running, so the boot can't be watched: check with `slots` in the VM "
                   "(or start `make beehive` and deploy again)")
    deadline = time.monotonic() + args.timeout
    while time.monotonic() < deadline:
        v = verdict(read_signals(args.beehive, since), build)
        if v == "healthy":
            return say(f"b{build} passed its health check: it is now the good slot")
        if v == "rolled-back":
            say(f"b{build} failed its trial; the VM rolled back to the good slot by itself")
            sys.exit(1)
        time.sleep(1)
    say(f"no health report from b{build} within {args.timeout}s: rolling back from the host")
    virsh.stop()
    st = bootdisk.get_state(image)
    if st["state"] in ("pending", "armed"):
        subprocess.run([sys.executable, str(ROOT / "tools/bootdisk.py"), "rollback", str(image)], check=True)
    virsh.start()
    say(f"{args.vm} restarted on the good slot ({st['good']})")
    sys.exit(1)


def cmd_setup_vm(args):
    """Point the VM's first disk at the boot disk image (backing up its config)."""
    virsh = Virsh(args.uri, args.vm, args.virsh)
    image = Path(args.image).resolve()
    if not image.exists():
        raise SystemExit(f"{image} missing: run `make bootdisk` first")
    xml = virsh.run("dumpxml", "--inactive", args.vm)
    if str(image) in xml:
        return say(f"{args.vm} already boots from {image}")
    backup = Path.home() / ".local/state/beecodersos" / f"{args.vm}-backup-before-bootdisk.xml"
    backup.parent.mkdir(parents=True, exist_ok=True)
    backup.write_text(xml)
    import xml.etree.ElementTree as ET
    root = ET.fromstring(xml)
    devices = root.find("devices")
    disk = next((d for d in devices.findall("disk") if d.get("device") == "disk"), None)
    if disk is None:
        disk = ET.SubElement(devices, "disk", {"type": "file", "device": "disk"})
        ET.SubElement(disk, "target", {"dev": "hda", "bus": "ide"})
    old = disk.find("source").get("file") if disk.find("source") is not None else None
    for child in list(disk):
        if child.tag in ("driver", "source", "backingStore"):
            disk.remove(child)
    disk.set("type", "file")
    disk.insert(0, ET.Element("source", {"file": str(image)}))
    disk.insert(0, ET.Element("driver", {"name": "qemu", "type": "raw"}))
    os_el = root.find("os")
    for b in os_el.findall("boot"):
        os_el.remove(b)
    os_el.append(ET.Element("boot", {"dev": "hd"}))
    tmp = backup.with_suffix(".new.xml")
    tmp.write_text(ET.tostring(root, encoding="unicode"))
    virsh.stop()
    virsh.run("define", str(tmp))
    say(f"{args.vm} now boots from {image} (A/B slots, BeeFS in partition 2)")
    if old:
        say(f"its previous disk {old} is detached, not deleted")
    say(f"old config saved to {backup}")
    virsh.start()


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("cmd", nargs="?", default="deploy", choices=["deploy", "setup-vm"])
    ap.add_argument("--vm", default="vm1")
    ap.add_argument("--uri", default="qemu:///system")
    ap.add_argument("--virsh", default=shutil.which("virsh") or "virsh")
    ap.add_argument("--image", default=str(IMAGE))
    ap.add_argument("--kernel", default=str(KERNEL))
    ap.add_argument("--build", type=int)
    ap.add_argument("--timeout", type=int, default=60, help="seconds to wait for the health report")
    ap.add_argument("--beehive", type=Path, default=BEEHIVE_STATE)
    ap.add_argument("--no-watch", action="store_true")
    args = ap.parse_args()
    {"deploy": cmd_deploy, "setup-vm": cmd_setup_vm}[args.cmd](args)


if __name__ == "__main__":
    main()
