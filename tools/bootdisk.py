#!/usr/bin/env python3
"""bootdisk: the BeeCodersOs boot disk with two kernel slots (A/B) and rollback.

Disk layout (MBR):
    partition 1  FAT16 (type 0xEF), bootable: Limine (BIOS + UEFI), limine.conf,
                 kernel-a.elf, kernel-b.elf
    partition 2  BeeFS (type 0x7F): the OS's data disk

The slot state is the first line of limine.conf, fixed width so the kernel can
patch it in place (one sector, same length):

    #slots good=a trial=b state=pending build=000012
    default_entry: 2

    good   the slot that last passed its health check
    trial  the slot being tried (- = none)
    state  pending  deployed by the host, not booted yet
           armed    the trial kernel booted and pointed default_entry back to
                    the good slot: if it crashes or hangs, the next boot rolls back
           ok       the trial passed its health check and became the good slot
           failed   the trial failed; the good slot runs again

Commands:
    bootdisk.py create IMG --kernel K [--size-mb 64] [--cmdline ARGS]
    bootdisk.py status IMG
    bootdisk.py deploy IMG --kernel K [--build N] [--cmdline-extra ARGS]   (VM off)
    bootdisk.py rollback IMG                                              (VM off)
    bootdisk.py get IMG PATH OUT        copy a file out of the FAT partition

Needs only Python and the `limine` binary (for the BIOS boot code); the FAT16
file system is written by this script, so no mtools or root rights are needed.
"""

import argparse
import re
import shutil
import struct
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
LIMINE_DIR = Path("/usr/share/limine")
BEEFS_TOOL = ROOT / "tools" / "beefs-tool" / "target" / "release" / "beefs-tool"
SECTOR = 512
BOOT_START = 2048           # first sector of the boot partition (1 MiB aligned)
BOOT_SECTORS = 16 * 2048    # 16 MiB FAT16 boot partition
FAT_TYPE, BEEFS_TYPE = 0xEF, 0x7F
CONF_SFN = b"LIMINE~1CON"   # short name the kernel looks for (see kernel/src/bootslots.rs)
STATE_RE = re.compile(r"^#slots good=([ab]) trial=([ab-]) state=(\w+) *build=(\d{6})$")
STATES = ("pending", "armed", "ok", "failed")


# --- slot state and limine.conf ----------------------------------------------------------

def render_conf(state: dict, cmdline: str = "") -> bytes:
    """limine.conf with the state lines first (the kernel patches them in place)."""
    default = state["trial"] if state["state"] == "pending" else state["good"]
    entries = ""
    for slot in "ab":
        args = " ".join(x for x in (f"slot={slot}", cmdline, state.get(f"extra_{slot}", "")) if x)
        entries += (f"\n/BeeCodersOs (slot {slot})\n    protocol: multiboot1\n"
                    f"    path: boot():/kernel-{slot}.elf\n    cmdline: {args}\n")
    text = (f"#slots good={state['good']} trial={state['trial']} state={state['state']:<7} "
            f"build={int(state['build']):06d}\n"
            f"default_entry: {'ab'.index(default) + 1}\n"
            "# BeeCodersOs A/B kernel slots: managed by the kernel and tools/bootdisk.py.\n"
            "# Keep the two lines above first and the same length.\n"
            "timeout: 0\nverbose: no\n" + entries)
    return text.encode()


def parse_conf(data: bytes) -> dict:
    lines = data.decode().splitlines()
    m = STATE_RE.match(lines[0]) if lines else None
    if not m:
        raise SystemExit("limine.conf has no slot state line: not a BeeCodersOs boot disk?")
    state = {"good": m[1], "trial": m[2], "state": m[3], "build": int(m[4]),
             "default": "ab"[int(lines[1].split(":")[1]) - 1]}
    cmdline = {}
    for slot, args in re.findall(r"path: boot\(\):/kernel-([ab])\.elf\n    cmdline: (.*)", data.decode()):
        cmdline[slot] = args
    state["cmdline"] = cmdline
    return state


# --- FAT16 ----------------------------------------------------------------------------------

def sfn_checksum(sfn: bytes) -> int:
    s = 0
    for c in sfn:
        s = (((s & 1) << 7) + (s >> 1) + c) & 0xFF
    return s


def short_name(name: str, taken: set) -> bytes:
    stem, _, ext = name.upper().rpartition(".") if "." in name else (name.upper(), "", "")
    clean = lambda s: re.sub(r"[^A-Z0-9_]", "", s)
    stem, ext = clean(stem), clean(ext)[:3]
    if stem[:8] == stem and stem and f"{stem}.{ext}".rstrip(".") == name.upper():
        sfn = stem.ljust(8).encode() + ext.ljust(3).encode()
        if sfn not in taken:
            return sfn
    for n in range(1, 100):
        tail = f"~{n}"
        sfn = (stem[:8 - len(tail)] + tail).ljust(8).encode() + ext.ljust(3).encode()
        if sfn not in taken:
            return sfn
    raise SystemExit(f"too many files like {name}")


def dir_entries(name: str, sfn: bytes, attr: int, cluster: int, size: int) -> bytes:
    """LFN entries (if the name is not a plain short name) + the short entry."""
    out = b""
    plain = name.upper() == name and sfn[:8].rstrip() + (b"." + sfn[8:].rstrip() if sfn[8:].strip() else b"") == name.encode()
    if not plain and name not in (".", ".."):
        chars = [ord(c) for c in name] + [0]
        chars += [0xFFFF] * (-len(chars) % 13)
        parts = [chars[i:i + 13] for i in range(0, len(chars), 13)]
        csum = sfn_checksum(sfn)
        for i in range(len(parts), 0, -1):
            p = parts[i - 1]
            u = lambda cs: b"".join(struct.pack("<H", c) for c in cs)
            seq = i | (0x40 if i == len(parts) else 0)
            out += bytes([seq]) + u(p[0:5]) + bytes([0x0F, 0, csum]) + u(p[5:11]) + b"\0\0" + u(p[11:13])
    date = ((2026 - 1980) << 9) | (1 << 5) | 1
    out += sfn + bytes([attr, 0, 0]) + struct.pack("<HHHHHHHI", 0, date, date, 0, 0, date, cluster, size)
    return out


class Fat16Builder:
    SPC, RESERVED, NFATS, ROOT_ENTRIES = 4, 4, 2, 512

    def __init__(self, sectors: int, hidden: int):
        self.sectors, self.hidden = sectors, hidden
        self.root_sectors = self.ROOT_ENTRIES * 32 // SECTOR
        fatsz = 1
        while True:
            data = sectors - self.RESERVED - self.root_sectors - self.NFATS * fatsz
            clusters = data // self.SPC
            need = -(-(clusters + 2) * 2 // SECTOR)
            if need <= fatsz:
                break
            fatsz = need
        if not 4085 <= clusters < 65525:
            raise SystemExit("boot partition size gives no valid FAT16")
        self.fatsz, self.clusters = fatsz, clusters
        self.data_start = self.RESERVED + self.NFATS * fatsz + self.root_sectors
        self.fat = [0xFFF8, 0xFFFF] + [0] * clusters
        self.next = 2
        self.data = {}  # cluster -> bytes

    def alloc(self, payload: bytes) -> int:
        csize = self.SPC * SECTOR
        n = max(1, -(-len(payload) // csize))
        first = self.next
        if first + n - 2 > self.clusters:
            raise SystemExit("boot partition is full")
        for i in range(n):
            c = first + i
            self.fat[c] = c + 1 if i < n - 1 else 0xFFFF
            self.data[c] = payload[i * csize:(i + 1) * csize]
        self.next += n
        return first

    def build_dir(self, tree: dict, parent: int | None) -> bytes:
        """Entries for `tree` ({name: bytes | dict}); subdirectories are written first."""
        out, taken = b"", set()
        children = []
        for name in tree:  # dict order: limine.conf first, as given
            sfn = short_name(name, taken)
            taken.add(sfn)
            children.append((name, sfn, tree[name]))
        for name, sfn, content in children:
            if isinstance(content, dict):
                cluster = self.next
                self.next += 1  # reserve the directory's cluster, fill it after its children
                body = (dir_entries(".", b".          ", 0x10, cluster, 0)
                        + dir_entries("..", b"..         ", 0x10, parent or 0, 0)
                        + self.build_dir(content, cluster))
                if len(body) > self.SPC * SECTOR:
                    raise SystemExit(f"directory {name} too large")
                self.fat[cluster] = 0xFFFF
                self.data[cluster] = body
                out += dir_entries(name, sfn, 0x10, cluster, 0)
            else:
                out += dir_entries(name, sfn, 0x20, self.alloc(content), len(content))
        return out

    def image(self, tree: dict) -> bytes:
        root = self.build_dir(tree, None)
        if len(root) > self.ROOT_ENTRIES * 32:
            raise SystemExit("too many files in the root directory")
        bs = bytearray(SECTOR)
        bs[0:3] = b"\xEB\x3C\x90"
        bs[3:11] = b"BEEOS   "
        struct.pack_into("<HBHBHHBHHHII", bs, 11, SECTOR, self.SPC, self.RESERVED, self.NFATS,
                         self.ROOT_ENTRIES, self.sectors if self.sectors < 65536 else 0, 0xF8,
                         self.fatsz, 63, 255, self.hidden, self.sectors if self.sectors >= 65536 else 0)
        bs[36], bs[38] = 0x80, 0x29
        bs[39:43] = b"BEE1"
        bs[43:54] = b"NO NAME    "  # no volume label entry in the root directory
        bs[54:62] = b"FAT16   "
        bs[510:512] = b"\x55\xAA"
        img = bytearray(self.sectors * SECTOR)
        img[0:SECTOR] = bs
        fat = b"".join(struct.pack("<H", e) for e in self.fat).ljust(self.fatsz * SECTOR, b"\0")
        for i in range(self.NFATS):
            off = (self.RESERVED + i * self.fatsz) * SECTOR
            img[off:off + len(fat)] = fat
        off = (self.RESERVED + self.NFATS * self.fatsz) * SECTOR
        img[off:off + len(root)] = root
        for c, payload in self.data.items():
            off = (self.data_start + (c - 2) * self.SPC) * SECTOR
            img[off:off + len(payload)] = payload
        return bytes(img)


class Fat16Reader:
    def __init__(self, part: bytes):
        (bps, self.spc, reserved, nfats, root_entries, _t16, _m, fatsz) = struct.unpack_from("<HBHBHHBH", part, 11)
        if bps != SECTOR or part[54:59] != b"FAT16":
            raise SystemExit("boot partition is not FAT16")
        self.part = part
        self.fat_off = reserved * SECTOR
        self.root_off = (reserved + nfats * fatsz) * SECTOR
        self.root_len = root_entries * 32
        self.data_off = self.root_off + self.root_len

    def chain(self, cluster: int) -> bytes:
        out, csize = b"", self.spc * SECTOR
        while 2 <= cluster < 0xFFF8:
            off = self.data_off + (cluster - 2) * csize
            out += self.part[off:off + csize]
            cluster = struct.unpack_from("<H", self.part, self.fat_off + cluster * 2)[0]
        return out

    def entries(self, raw: bytes):
        """(long name, sfn, attr, cluster, size, offset of the short entry)."""
        lfn = {}
        for off in range(0, len(raw), 32):
            e = raw[off:off + 32]
            if e[0] == 0:
                break
            if e[0] == 0xE5:
                lfn = {}
                continue
            if e[11] == 0x0F:
                chars = e[1:11] + e[14:26] + e[28:32]
                lfn[e[0] & 0x1F] = "".join(chr(c) for c in struct.unpack("<13H", chars) if c not in (0, 0xFFFF))
                continue
            name = "".join(lfn[k] for k in sorted(lfn)) if lfn else self.sfn_to_name(e[:11])
            lfn = {}
            cluster, size = struct.unpack_from("<H", e, 26)[0], struct.unpack_from("<I", e, 28)[0]
            yield name, e[:11], e[11], cluster, size, off

    @staticmethod
    def sfn_to_name(sfn: bytes) -> str:
        base, ext = sfn[:8].decode().rstrip(), sfn[8:].decode().rstrip()
        return f"{base}.{ext}" if ext else base

    def tree(self, raw=None) -> dict:
        raw = self.part[self.root_off:self.root_off + self.root_len] if raw is None else raw
        out = {}
        for name, _sfn, attr, cluster, size, _ in self.entries(raw):
            if name in (".", ".."):
                continue
            out[name] = self.tree(self.chain(cluster)) if attr & 0x10 else self.chain(cluster)[:size]
        return out


# --- disk image -----------------------------------------------------------------------------

def read_mbr(img: Path) -> list:
    with open(img, "rb") as f:
        mbr = f.read(SECTOR)
    if mbr[510:512] != b"\x55\xAA":
        raise SystemExit(f"{img}: no partition table")
    return [struct.unpack_from("<BBBBBBBBII", mbr, 446 + i * 16) for i in range(4)]


def boot_partition(img: Path) -> tuple[int, int]:
    for e in read_mbr(img):
        if e[4] == FAT_TYPE:
            return e[8], e[9]
    raise SystemExit(f"{img}: no FAT boot partition")


def read_tree(img: Path) -> dict:
    start, length = boot_partition(img)
    with open(img, "rb") as f:
        f.seek(start * SECTOR)
        return Fat16Reader(f.read(length * SECTOR)).tree()


def write_tree(img: Path, tree: dict):
    """Rewrite the whole boot partition (the BeeFS partition is not touched)."""
    start, length = boot_partition(img)
    # limine.conf first: its first cluster must hold the state lines.
    tree = {"limine.conf": tree["limine.conf"], **{k: v for k, v in tree.items() if k != "limine.conf"}}
    data = Fat16Builder(length, start).image(tree)
    with open(img, "r+b") as f:
        f.seek(start * SECTOR)
        f.write(data)


def get_state(img: Path) -> dict:
    return parse_conf(read_tree(img)["limine.conf"])


def cmd_create(args):
    img, kernel = Path(args.image), Path(args.kernel).read_bytes()
    total = args.size_mb * 2048
    beefs_start = BOOT_START + BOOT_SECTORS
    beefs_len = total - beefs_start
    if beefs_len < 2048:
        raise SystemExit("--size-mb too small")
    with open(img, "wb") as f:
        f.truncate(total * SECTOR)
        mbr = bytearray(SECTOR)
        for i, (boot, ptype, start, length) in enumerate([(0x80, FAT_TYPE, BOOT_START, BOOT_SECTORS),
                                                          (0, BEEFS_TYPE, beefs_start, beefs_len)]):
            struct.pack_into("<BBBBBBBBII", mbr, 446 + i * 16, boot, 0xFE, 0xFF, 0xFF, ptype, 0xFE, 0xFF, 0xFF,
                             start, length)
        mbr[510:512] = b"\x55\xAA"
        f.write(mbr)
    state = {"good": "a", "trial": "-", "state": "ok", "build": args.build or current_build()}
    tree = {
        "limine.conf": render_conf(state, args.cmdline),
        "limine-bios.sys": (LIMINE_DIR / "limine-bios.sys").read_bytes(),
        "kernel-a.elf": kernel,
        "kernel-b.elf": kernel,
        "EFI": {"BOOT": {"BOOTX64.EFI": (LIMINE_DIR / "BOOTX64.EFI").read_bytes()}},
    }
    write_tree(img, tree)
    # BeeFS in partition 2: format a temporary image and copy it in.
    if not BEEFS_TOOL.exists():
        raise SystemExit(f"{BEEFS_TOOL} missing: run `make tool`")
    with tempfile.TemporaryDirectory() as tmp:
        part = Path(tmp) / "beefs.img"
        subprocess.run([str(BEEFS_TOOL), str(part), "mkfs", str(beefs_len // 2048)], check=True, capture_output=True)
        with open(img, "r+b") as f:
            f.seek(beefs_start * SECTOR)
            f.write(part.read_bytes())
    for cmd in (["write", "/welcome.txt", "Welcome to BeeFS on the BeeCodersOs boot disk!"], ["mkdir", "/home/bee"]):
        subprocess.run([str(BEEFS_TOOL), str(img), *cmd], check=True, capture_output=True)
    subprocess.run(["limine", "bios-install", str(img)], check=True, capture_output=True)
    print(f"{img}: boot disk, {args.size_mb} MiB, both slots = {args.kernel}, BeeFS {beefs_len // 2048} MiB")


def current_build() -> int:
    try:
        return int((ROOT / "BUILD_NUMBER").read_text().strip())
    except (OSError, ValueError):
        return 0


def cmd_status(args):
    s = get_state(Path(args.image))
    print(f"good slot:  {s['good']}")
    print(f"trial slot: {s['trial']}  (state: {s['state']}, build b{s['build']})")
    print(f"next boot:  slot {s['default']}")
    for slot, cmdline in s["cmdline"].items():
        print(f"slot {slot} cmdline: {cmdline}")


def cmd_deploy(args):
    img = Path(args.image)
    tree = read_tree(img)
    s = parse_conf(tree["limine.conf"])
    if s["state"] in ("pending", "armed"):
        print(f"note: replacing an unfinished trial of slot {s['trial']} ({s['state']})")
    target = "b" if s["good"] == "a" else "a"
    tree[f"kernel-{target}.elf"] = Path(args.kernel).read_bytes()
    base = " ".join(w for w in s["cmdline"].get(s["good"], "").split() if not w.startswith("slot="))
    state = {"good": s["good"], "trial": target, "state": "pending", "build": args.build or current_build(),
             f"extra_{target}": args.cmdline_extra or ""}
    tree["limine.conf"] = render_conf(state, base)
    write_tree(img, tree)
    print(f"deployed {args.kernel} to slot {target} (b{state['build']}); next boot tries it, "
          f"slot {s['good']} stays the fallback")


def cmd_rollback(args):
    img = Path(args.image)
    tree = read_tree(img)
    s = parse_conf(tree["limine.conf"])
    base = " ".join(w for w in s["cmdline"].get(s["good"], "").split() if not w.startswith("slot="))
    state = {"good": s["good"], "trial": s["trial"], "state": "failed" if s["trial"] != "-" else "ok",
             "build": s["build"]}
    tree["limine.conf"] = render_conf(state, base)
    write_tree(img, tree)
    print(f"rolled back: next boot runs slot {s['good']}")


def cmd_get(args):
    node = read_tree(Path(args.image))
    for part in args.path.strip("/").split("/"):
        node = node[part]
    Path(args.out).write_bytes(node)


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    p = sub.add_parser("create")
    p.add_argument("image")
    p.add_argument("--kernel", required=True)
    p.add_argument("--size-mb", type=int, default=64)
    p.add_argument("--cmdline", default="", help="extra kernel arguments for both slots (tests: test)")
    p.add_argument("--build", type=int)
    p = sub.add_parser("status")
    p.add_argument("image")
    p = sub.add_parser("deploy")
    p.add_argument("image")
    p.add_argument("--kernel", required=True)
    p.add_argument("--build", type=int)
    p.add_argument("--cmdline-extra", help="extra arguments for the new slot only (tests: failboot)")
    p = sub.add_parser("rollback")
    p.add_argument("image")
    p = sub.add_parser("get")
    p.add_argument("image")
    p.add_argument("path")
    p.add_argument("out")
    args = ap.parse_args()
    if args.cmd == "create" and not shutil.which("limine"):
        sys.exit("`limine` not found (Arch: pacman -S limine)")
    {"create": cmd_create, "status": cmd_status, "deploy": cmd_deploy, "rollback": cmd_rollback,
     "get": cmd_get}[args.cmd](args)


if __name__ == "__main__":
    main()
