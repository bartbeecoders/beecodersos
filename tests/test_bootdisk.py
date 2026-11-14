#!/usr/bin/env python3
"""Tests for the A/B boot disk tools (tools/bootdisk.py, tools/deploy.py):
the FAT16 writer/reader, slot state changes, and the deploy watchdog's verdicts.
Booting the disk is tested by tests/scenarios/slots.scn.

    python3 tests/test_bootdisk.py        (part of `make check`)
"""

import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import bootdisk  # noqa: E402
import deploy  # noqa: E402

KERNEL = ROOT / "build" / "beecoders.elf"


class Fat16(unittest.TestCase):
    def test_round_trip_with_long_names_and_folders(self):
        tree = {
            "limine.conf": b"#slots ...\n",
            "limine-bios.sys": bytes(range(256)) * 900,   # several clusters
            "kernel-a.elf": b"A" * 5000,
            "README": b"",                                   # empty file
            "EFI": {"BOOT": {"BOOTX64.EFI": b"efi"}},
        }
        data = bootdisk.Fat16Builder(32768, 2048).image(tree)
        self.assertEqual(bootdisk.Fat16Reader(data).tree(), tree)
        # The kernel finds limine.conf by this short name (kernel/src/bootslots.rs).
        root = bootdisk.Fat16Reader(data)
        sfns = [e[1] for e in root.entries(data[root.root_off:root.root_off + root.root_len])]
        self.assertIn(bootdisk.CONF_SFN, sfns)

    @unittest.skipUnless(shutil.which("fsck.fat"), "fsck.fat not installed")
    def test_fsck_is_happy(self):
        data = bootdisk.Fat16Builder(32768, 2048).image({"limine.conf": b"x", "a-long-name.txt": b"y" * 3000})
        with tempfile.NamedTemporaryFile() as f:
            f.write(data)
            f.flush()
            r = subprocess.run(["fsck.fat", "-n", f.name], capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        self.assertNotIn("label", r.stdout.lower())


@unittest.skipUnless(KERNEL.exists() and shutil.which("limine") and bootdisk.BEEFS_TOOL.exists(),
                     "needs build/beecoders.elf, limine and beefs-tool (make; make tool)")
class Slots(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.img = Path(self.tmp.name) / "boot.img"
        self.tool([ "create", self.img, "--kernel", KERNEL, "--size-mb", "40", "--cmdline", "test", "--build", "5"])

    def tearDown(self):
        self.tmp.cleanup()

    def tool(self, args):
        r = subprocess.run([sys.executable, str(ROOT / "tools/bootdisk.py"), *map(str, args)],
                           capture_output=True, text=True)
        self.assertEqual(r.returncode, 0, r.stdout + r.stderr)
        return r.stdout

    def test_create_deploy_rollback(self):
        st = bootdisk.get_state(self.img)
        self.assertEqual((st["good"], st["trial"], st["state"], st["default"]), ("a", "-", "ok", "a"))
        self.assertEqual(st["cmdline"], {"a": "slot=a test", "b": "slot=b test"})
        # BeeFS lives in partition 2 and beefs-tool finds it.
        r = subprocess.run([str(bootdisk.BEEFS_TOOL), str(self.img), "cat", "/welcome.txt"], capture_output=True, text=True)
        self.assertIn("Welcome", r.stdout)

        new_kernel = Path(self.tmp.name) / "new.elf"
        new_kernel.write_bytes(b"NEW KERNEL")
        self.tool(["deploy", self.img, "--kernel", new_kernel, "--build", "6", "--cmdline-extra", "failboot"])
        st = bootdisk.get_state(self.img)
        self.assertEqual((st["good"], st["trial"], st["state"], st["build"], st["default"]), ("a", "b", "pending", 6, "b"))
        self.assertEqual(st["cmdline"]["b"], "slot=b test failboot")
        tree = bootdisk.read_tree(self.img)
        self.assertEqual(tree["kernel-b.elf"], b"NEW KERNEL")
        self.assertEqual(tree["kernel-a.elf"], KERNEL.read_bytes())   # the good kernel is kept
        # The state lines keep the exact layout the kernel patches.
        first = tree["limine.conf"][:66].decode()
        self.assertEqual(first, "#slots good=a trial=b state=pending build=000006\ndefault_entry: 2\n")

        self.tool(["rollback", self.img])
        st = bootdisk.get_state(self.img)
        self.assertEqual((st["good"], st["state"], st["default"]), ("a", "failed", "a"))
        # Data in BeeFS survives rewriting the boot partition.
        r = subprocess.run([str(bootdisk.BEEFS_TOOL), str(self.img), "ls", "/"], capture_output=True, text=True)
        self.assertIn("welcome.txt", r.stdout)


class Watchdog(unittest.TestCase):
    def test_verdicts(self):
        health = {"kind": "health", "text": "slot b b12 passed its health check"}
        crash = {"kind": "crash", "text": "trial of build b12 in slot a failed (crashed or hung); rolled back to slot b"}
        self.assertEqual(deploy.verdict([health], 12), "healthy")
        self.assertEqual(deploy.verdict([crash], 12), "rolled-back")
        self.assertIsNone(deploy.verdict([health, crash], 1))     # b1 is not b12
        self.assertIsNone(deploy.verdict([{"kind": "wish", "text": "b12 "}], 12))
        self.assertIsNone(deploy.verdict([], 12))


if __name__ == "__main__":
    unittest.main(verbosity=1)
