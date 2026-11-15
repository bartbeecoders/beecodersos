#!/usr/bin/env python3
"""Tests for tools/usbstick.py: which devices it agrees to overwrite.
Nothing is written anywhere: only the checks are tested, on made-up devices."""

import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent.parent / "tools"))
import usbstick  # noqa: E402

GB = 1000**3


def stick(**changes) -> dict:
    dev = {"name": "sdc", "path": "/dev/sdc", "size": 16 * GB, "type": "disk", "tran": "usb", "rm": True,
           "hotplug": True, "ro": False, "vendor": "SanDisk ", "model": "Cruzer", "mountpoints": [None],
           "children": [{"name": "sdc1", "path": "/dev/sdc1", "size": 16 * GB, "type": "part", "fstype": "vfat",
                         "label": "STICK", "mountpoints": ["/run/media/me/STICK"]}]}
    dev.update(changes)
    return dev


class Refusal(unittest.TestCase):
    def test_a_plain_stick_is_accepted(self):
        self.assertIsNone(usbstick.refusal(stick()))
        self.assertIsNone(usbstick.refusal(stick(children=[])))

    def test_internal_disks_are_refused(self):
        for tran in ("sata", "nvme", "ata", None):
            self.assertIn("not connected over USB", usbstick.refusal(stick(tran=tran, rm=False, hotplug=False)))

    def test_partitions_are_refused(self):
        self.assertIn("not a whole disk", usbstick.refusal(stick(type="part")))

    def test_fixed_usb_devices_are_refused(self):
        self.assertIn("not a removable", usbstick.refusal(stick(rm=False, hotplug=False)))

    def test_read_only_and_empty_devices_are_refused(self):
        self.assertIn("read-only", usbstick.refusal(stick(ro=True)))
        self.assertIn("empty", usbstick.refusal(stick(size=0)))

    def test_the_running_system_is_refused(self):
        for where in ("/", "/boot", "/home", "[SWAP]"):
            dev = stick(children=[{"path": "/dev/sdc1", "mountpoints": [where]}])
            self.assertIn("part of the running system", usbstick.refusal(dev))
        # Also when it hides below an encrypted or LVM layer.
        deep = stick(children=[{"path": "/dev/sdc1", "mountpoints": [None],
                                "children": [{"path": "/dev/mapper/root", "mountpoints": ["/"]}]}])
        self.assertIn("part of the running system", usbstick.refusal(deep))
        # --allow-large never lifts this.
        self.assertIn("part of the running system", usbstick.refusal(deep, allow_large=True))

    def test_large_devices_need_a_flag(self):
        big = stick(size=500 * GB)
        self.assertIn("larger than 128 GB", usbstick.refusal(big))
        self.assertIsNone(usbstick.refusal(big, allow_large=True))
        self.assertIsNone(usbstick.refusal(stick(size=128 * GB)))


class Describe(unittest.TestCase):
    def test_contents_name_every_partition(self):
        lines = usbstick.contents(stick())
        self.assertEqual(len(lines), 1)
        self.assertIn("/dev/sdc1", lines[0])
        self.assertIn('vfat "STICK"', lines[0])
        self.assertIn("mounted at /run/media/me/STICK", lines[0])
        self.assertEqual(usbstick.contents(stick(children=[])), ["    (no partitions)"])

    def test_stable_path_follows_the_usb_name_only(self):
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            device, other = tmp / "sdc", tmp / "sdd"
            device.touch()
            other.touch()
            by_id = tmp / "by-id"
            by_id.mkdir()
            (by_id / "usb-SanDisk_Cruzer_123-0:0").symlink_to(device)
            (by_id / "ata-Some_Disk").symlink_to(device)
            (by_id / "usb-Other_Stick-0:0").symlink_to(other)
            self.assertEqual(usbstick.stable_path({"path": str(device)}, by_id), by_id / "usb-SanDisk_Cruzer_123-0:0")
            (by_id / "usb-SanDisk_Cruzer_123-0:0").unlink()
            self.assertIsNone(usbstick.stable_path({"path": str(device)}, by_id))
            self.assertIsNone(usbstick.stable_path({"path": str(device)}, tmp / "missing"))


if __name__ == "__main__":
    unittest.main()
