#!/usr/bin/env python3
"""Tests for the improvement loop (tools/evolve/evolve.py) in a throwaway git
repository, with the fake agent and fake check/ship commands: no LLM needed.

    python3 tests/test_evolve.py        (part of `make check`)
"""

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
EVOLVE = ROOT / "tools" / "evolve" / "evolve.py"


class EvolveLoop(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self.tmp.name) / "repo"
        self.hive = Path(self.tmp.name) / "hive"
        self.repo.mkdir()
        self.hive.mkdir()
        for cmd in (["init", "-q", "-b", "main"], ["config", "user.name", "Test"],
                    ["config", "user.email", "test@example.invalid"]):
            self.git(*cmd)
        (self.repo / "README.md").write_text("test repo\n")
        (self.repo / "CLAUDE.md").write_text("rules for agents\n")
        (self.repo / "Makefile").write_text("check:\n\ttrue\n")
        (self.repo / ".gitignore").write_text(".evolve/\n")
        (self.repo / "tests" / "scenarios").mkdir(parents=True)
        (self.repo / "tests" / "scenarios" / "base.scn").write_text("$ echo hi\nhi\n")
        self.git("add", "-A")
        self.git("commit", "-q", "-m", "base")
        signals = [
            {"kind": "wish", "text": "a text editor", "t": "2026-11-14T10:00:00Z", "build": "9"},
            {"kind": "wish", "text": "A text  editor", "t": "2026-11-14T10:05:00Z", "build": "9"},
            {"kind": "missed", "text": "edit", "t": "2026-11-14T10:01:00Z", "build": "9"},
            {"kind": "missed", "text": "edit", "t": "2026-11-14T10:02:00Z", "build": "9"},
            {"kind": "missed", "text": "edit", "t": "2026-11-14T10:03:00Z", "build": "9"},
            {"kind": "crash", "text": "panicked at src/shell.rs:240:30: boom", "t": "2026-11-14T10:04:00Z", "build": "9"},
            {"kind": "hello", "text": "ignored kind", "t": "2026-11-14T10:04:00Z"},
        ]
        (self.hive / "signals.jsonl").write_text("".join(json.dumps(s) + "\n" for s in signals))

    def tearDown(self):
        self.tmp.cleanup()

    def git(self, *args) -> str:
        return subprocess.run(["git", "-C", str(self.repo), *args], capture_output=True, text=True, check=True).stdout

    def evolve(self, *args, ok=True) -> str:
        r = subprocess.run([sys.executable, str(EVOLVE), "--repo", str(self.repo), *args],
                           capture_output=True, text=True)
        if ok and r.returncode != 0:
            self.fail(f"evolve {' '.join(args)} failed:\n{r.stdout}\n{r.stderr}")
        return r.stdout + r.stderr

    def work(self, *extra) -> str:
        return self.evolve("work", *extra, "--agent", "fake", "--check-cmd", "test -f README.md")

    def harvest(self) -> str:
        return self.evolve("harvest", "--beehive", str(self.hive), "--triage", "rules", "--agent", "fake")

    def items(self) -> dict:
        out = {}
        for p in (self.repo / "Docs" / "backlog").glob("[0-9][0-9][0-9]-*.md"):
            front = p.read_text().split("---\n")[1]
            fields = dict(l.split(": ", 1) for l in front.splitlines() if ": " in l)
            out[int(fields["id"])] = fields
        return out

    def test_full_cycle(self):
        out = self.harvest()
        self.assertIn("6 new signals", out)
        items = self.items()
        self.assertEqual(len(items), 3)  # crash, the wish (said twice), the missed command
        by_title = {i["title"]: i for i in items.values()}
        self.assertEqual(by_title["Fix crash: panicked at src/shell.rs:240:30: boom"]["priority"], "1")
        self.assertEqual(json.loads(by_title["a text editor"]["signals"]).__len__(), 2)
        self.assertEqual(by_title["Add a `edit` command"]["priority"], "2")  # asked 3 times
        self.assertIn("no new signals", self.harvest())

        # The top item (the crash) goes to the agent; evolve verifies and commits.
        out = self.work()
        self.assertIn("READY FOR REVIEW", out)
        crash = next(i for i in self.items().values() if i["kind"] == "bug")
        self.assertEqual(crash["status"], "review")
        branch = crash["branch"]
        self.assertIn("evolve #", self.git("log", "-1", "--format=%s", branch))
        review = (self.repo / "Docs" / "backlog" / "reviews" / f"{int(crash['id']):03d}.md").read_text()
        self.assertIn("| `make check` | pass", review)
        self.assertIn("scenarios 1 -> 2", review)

        out = self.evolve("approve", crash["id"], "--ship-cmd", "true")
        self.assertIn("done", out)
        self.assertTrue((self.repo / f"feature-{crash['id']}.txt").exists())
        self.assertEqual(self.items()[int(crash["id"])]["status"], "done")
        self.assertNotIn(branch, self.git("branch"))
        self.assertFalse((self.repo / ".evolve" / "worktrees" / f"{int(crash['id']):03d}").exists())
        self.assertIn("evolve: merge", self.git("log", "--format=%s"))

    def test_gates(self):
        self.harvest()
        out = self.work("--fake-mode", "protected")
        self.assertIn("NEEDS ATTENTION", out)
        self.assertIn("touches protected files: Makefile", out)
        item = next(i for i in self.items().values() if i["status"] == "review")
        out = self.evolve("approve", item["id"], "--ship-cmd", "true", ok=False)
        self.assertIn("needs attention", out)
        self.assertFalse((self.repo / f"feature-{item['id']}.txt").exists())  # not merged
        self.evolve("reject", item["id"], "--reason", "keep the Makefile as it is", "--retry")
        again = self.items()[int(item["id"])]
        self.assertEqual(again["status"], "open")
        self.assertNotIn("evolve/", self.git("branch"))

        out = self.work(item["id"], "--fake-mode", "no-tests")
        self.assertIn("no test added or changed", out)
        self.evolve("reject", item["id"])
        out = self.work(next(i for i in self.items().values() if i["status"] == "open")["id"], "--fake-mode", "big")
        self.assertIn("diff too big", out)
        out = self.evolve("work", "--agent", "fake", "--check-cmd", "false", ok=False)
        self.assertIn("failed", out)

    def test_work_needs_committed_infrastructure(self):
        self.harvest()
        (self.repo / "new_feature.rs").write_text("uncommitted\n")
        out = self.evolve("work", "--agent", "fake", "--check-cmd", "true", ok=False)
        self.assertIn("uncommitted changes the agent would not see", out)
        self.assertIn("new_feature.rs", out)
        self.assertNotIn("evolve/", self.git("branch"))  # nothing started
        (self.repo / "new_feature.rs").unlink()
        self.git("rm", "-q", "CLAUDE.md")
        self.git("commit", "-q", "-m", "drop rules")
        out = self.evolve("work", "--agent", "fake", "--check-cmd", "true", ok=False)
        self.assertIn("CLAUDE.md is not committed", out)
        self.assertTrue(all(i["status"] == "open" for i in self.items().values()))

    def test_approve_refuses_dirty_main(self):
        self.harvest()
        self.work()
        (self.repo / "README.md").write_text("local edit\n")
        item = next(i for i in self.items().values() if i["status"] == "review")
        out = self.evolve("approve", item["id"], "--ship-cmd", "true", ok=False)
        self.assertIn("uncommitted changes", out)


if __name__ == "__main__":
    unittest.main(verbosity=1)
