#!/usr/bin/env python3
"""usbstick: write the BeeCodersOs ISO to a USB stick, to boot real hardware.

    usbstick.py                 list the USB sticks that are plugged in
    usbstick.py /dev/sdX        write build/beecoders.iso to that stick
    usbstick.py /dev/sdX --iso FILE     another image
    usbstick.py /dev/sdX --dry-run      do all the checks, write nothing

EVERYTHING ON THE STICK IS ERASED. To make a mistake hard, the tool only
writes to a device that
  * is a whole disk (not a partition), connected over USB, and removable;
  * is at most 128 GB (--allow-large lifts this one limit);
  * holds no part of the running system (/, /boot, /home, swap, ...).
It shows what is on the stick, asks you to type the device name, writes
through the stick's stable name in /dev/disk/by-id (so a stick that is
swapped in between is not hit), and reads everything back to compare.

Writing needs root: the tool runs `sudo dd` and `sudo cmp` and tells you so.
The ISO is a hybrid image (`make iso`): the same file boots from CD and USB,
with a legacy BIOS and with UEFI.
"""

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
ISO = ROOT / "build" / "beecoders.iso"
MAX_SIZE = 128 * 1000**3
BY_ID = Path("/dev/disk/by-id")
# A device holding one of these is part of the running system.
SYSTEM_MOUNTS = ("/", "/boot", "/boot/efi", "/efi", "/home", "/usr", "/var", "/nix", "[SWAP]")


def lsblk() -> list[dict]:
    """Every whole disk, as lsblk describes it (partitions under "children")."""
    out = subprocess.run(
        ["lsblk", "-J", "-b", "-o", "NAME,PATH,SIZE,TYPE,TRAN,RM,HOTPLUG,RO,VENDOR,MODEL,SERIAL,LABEL,FSTYPE,MOUNTPOINTS"],
        check=True, capture_output=True, text=True).stdout
    return json.loads(out)["blockdevices"]


def mounts(dev: dict) -> list[str]:
    """Every place the device or anything on it (partitions, LVM, LUKS) is mounted."""
    found = [m for m in dev.get("mountpoints") or [] if m]
    for child in dev.get("children") or []:
        found += mounts(child)
    return found


def refusal(dev: dict, allow_large: bool = False) -> str | None:
    """Why this device must not be written to; None when it is a plain USB stick."""
    if dev.get("type") != "disk":
        return "not a whole disk (give the stick itself, like /dev/sdc, not a partition)"
    if dev.get("tran") != "usb":
        return f"not connected over USB (it is {dev.get('tran') or 'unknown'})"
    if not (dev.get("rm") or dev.get("hotplug")):
        return "not a removable device"
    if dev.get("ro"):
        return "read-only (a write-protect switch?)"
    system = [m for m in mounts(dev) if m in SYSTEM_MOUNTS]
    if system:
        return f"part of the running system (mounted at {', '.join(system)})"
    size = dev.get("size") or 0
    if size == 0:
        return "empty (a card reader without a card?)"
    if size > MAX_SIZE and not allow_large:
        return f"larger than {MAX_SIZE // 1000**3} GB: probably a USB hard disk, not a stick (--allow-large to use it anyway)"
    return None


def gb(size: int) -> str:
    return f"{size / 1000**3:.1f} GB"


def describe(dev: dict) -> str:
    name = " ".join(x.strip() for x in (dev.get("vendor"), dev.get("model")) if x and x.strip()) or "unknown device"
    return f"{dev['path']}  {gb(dev.get('size') or 0)}  {name}"


def contents(dev: dict) -> list[str]:
    """What is on the device now: one line per partition."""
    lines = []
    for part in dev.get("children") or []:
        what = " ".join(x for x in (part.get("fstype"), f'"{part["label"]}"' if part.get("label") else None) if x)
        where = [m for m in part.get("mountpoints") or [] if m]
        lines.append(f"    {part['path']}  {gb(part.get('size') or 0)}  {what or 'unknown'}"
                     + (f"  (mounted at {', '.join(where)})" if where else ""))
    if not lines and dev.get("fstype"):
        lines.append(f"    no partitions; {dev['fstype']}" + (f' "{dev["label"]}"' if dev.get("label") else ""))
    return lines or ["    (no partitions)"]


def stable_path(dev: dict, by_id: Path = BY_ID) -> Path | None:
    """The /dev/disk/by-id name of the device: it follows the stick, not the letter."""
    if not by_id.is_dir():
        return None
    target = Path(dev["path"]).resolve()
    names = sorted(p for p in by_id.iterdir() if p.name.startswith("usb-") and p.resolve() == target)
    return names[0] if names else None


def list_sticks(devices: list[dict]) -> int:
    sticks = [d for d in devices if refusal(d) is None]
    if not sticks:
        print("No USB stick found. Plug one in and try again.")
        others = [d for d in devices if d.get("tran") == "usb"]
        for d in others:
            print(f"  not usable: {describe(d)}: {refusal(d)}")
        return 1
    print("USB sticks:")
    for d in sticks:
        print(f"  {describe(d)}")
        for line in contents(d):
            print(line)
    print(f"\nWrite the ISO with: {Path(sys.argv[0])} /dev/sdX   (everything on the stick is erased)")
    return 0


def run(cmd: list[str]) -> None:
    print("  $ " + " ".join(cmd))
    subprocess.run(cmd, check=True)


def write(device: str, iso: Path, allow_large: bool, dry_run: bool, yes: bool) -> int:
    if not iso.is_file():
        print(f"{iso} does not exist: run `make iso` first.")
        return 1
    wanted = Path(device).resolve()
    dev = next((d for d in lsblk() if Path(d["path"]) == wanted), None)
    if dev is None:
        print(f"{device} is not a disk (partitions are refused: give the stick itself). Sticks found:")
        list_sticks(lsblk())
        return 1
    why = refusal(dev, allow_large)
    if why:
        print(f"Refusing to write to {device}: {why}.")
        return 1
    size = iso.stat().st_size
    if size > dev["size"]:
        print(f"The image ({gb(size)}) does not fit on {device} ({gb(dev['size'])}).")
        return 1
    target = stable_path(dev)
    if target is None:
        print(f"Refusing to write to {device}: it has no usb-* name in {BY_ID}, so it can't be followed safely.")
        return 1

    print(f"Image: {iso} ({size / 1024**2:.1f} MiB)")
    print(f"Stick: {describe(dev)}")
    print(f"       {target}")
    print("On it now (ALL OF THIS WILL BE ERASED):")
    for line in contents(dev):
        print(line)
    if dry_run:
        print("\nDry run: all checks passed, nothing was written.")
        return 0
    if not yes:
        answer = input(f"\nType {dev['path']} to erase it and write the image: ").strip()
        if answer != dev["path"]:
            print("Not confirmed: nothing was written.")
            return 1

    print("\nUnmounting, writing and checking (sudo asks for your password):")
    for part in [dev] + (dev.get("children") or []):
        if any(part.get("mountpoints") or []):
            run(["sudo", "umount", part["path"]])
    # Still the same stick? (It could have been swapped while the question was open.)
    if target.resolve() != wanted:
        print(f"{target} no longer points at {device}: stopping, nothing was written.")
        return 1
    run(["sudo", "dd", f"if={iso}", f"of={target}", "bs=4M", "conv=fsync", "oflag=direct", "status=progress"])
    run(["sudo", "sync"])
    # Read it back: a worn-out stick can accept a write and return something else.
    check = subprocess.run(["sudo", "cmp", "-n", str(size), str(iso), str(target)])
    if check.returncode != 0:
        print(f"\nFAILED: what is on {device} differs from the image. The stick may be broken; try another one.")
        return 1
    print(f"\nDone: {device} holds BeeCodersOs and was read back correctly. It can be unplugged.")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Write the BeeCodersOs ISO to a USB stick (erases the stick).")
    parser.add_argument("device", nargs="?", help="the stick, like /dev/sdc (omit to list the sticks)")
    parser.add_argument("--iso", type=Path, default=ISO, help=f"image to write (default: {ISO.relative_to(ROOT)})")
    parser.add_argument("--allow-large", action="store_true", help="accept a USB device over 128 GB")
    parser.add_argument("--dry-run", action="store_true", help="check everything, write nothing")
    parser.add_argument("--yes", action="store_true", help="do not ask for confirmation")
    args = parser.parse_args()
    if os.geteuid() == 0:
        print("Run this as yourself, not as root: it calls sudo for the two commands that need it.")
        return 1
    try:
        if args.device is None:
            return list_sticks(lsblk())
        return write(args.device, args.iso, args.allow_large, args.dry_run, args.yes)
    except subprocess.CalledProcessError as e:
        print(f"\nFAILED: `{' '.join(map(str, e.cmd))}` ended with status {e.returncode}.")
        return 1
    except KeyboardInterrupt:
        print("\nInterrupted.")
        return 1


if __name__ == "__main__":
    sys.exit(main())
