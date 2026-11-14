#!/usr/bin/env python3
"""bee_report: summarize what BeeCodersOs recorded about itself.

Sources (use one or both):
  --disk build/disk.img     a BeeFS disk (VM switched off): event log, inbox, crash reports
  --beehive [DIR]           signals beehive received live over the AI bridge
                            (default ~/.local/state/beehive)

    tools/bee_report.py --disk build/disk.img --beehive
    make report

Prints crashes, commands that were not found (what people expect the OS to
do), wishes and feedback, command error rates and the boot-time trend.
--json prints the same data as JSON (for the improvement loop).
"""

import argparse
import collections
import json
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
BEEFS_TOOL = ROOT / "tools" / "beefs-tool" / "target" / "release" / "beefs-tool"


def read_disk_file(disk: Path, path: str) -> list[dict]:
    r = subprocess.run([str(BEEFS_TOOL), str(disk), "cat", path], capture_output=True, text=True)
    if r.returncode != 0:
        return []  # file not there (yet)
    return parse_jsonl(r.stdout)


def parse_jsonl(text: str) -> list[dict]:
    out = []
    for line in text.splitlines():
        try:
            obj = json.loads(line)
        except ValueError:
            continue
        if isinstance(obj, dict):
            out.append(obj)
    return out


def collect(disk: Path | None, beehive: Path | None) -> dict:
    events, inbox, crashes, signals = [], [], [], []
    if disk:
        if not BEEFS_TOOL.exists():
            sys.exit(f"{BEEFS_TOOL} missing: run `make tool`")
        for f in ("/system/log/events.old.jsonl", "/system/log/events.jsonl"):
            events += read_disk_file(disk, f)
        for f in ("/system/inbox/signals.old.jsonl", "/system/inbox/signals.jsonl"):
            inbox += read_disk_file(disk, f)
        for f in ("/system/crash/history.old.jsonl", "/system/crash/history.jsonl", "/system/crash/pending.jsonl"):
            crashes += read_disk_file(disk, f)
    if beehive and (beehive / "signals.jsonl").exists():
        signals = parse_jsonl((beehive / "signals.jsonl").read_text())

    cmds = [e for e in events if e.get("ev") == "cmd"]
    missed = collections.Counter(e["cmd"] for e in cmds if e.get("status") == "notfound")
    missed.update(s.get("text") for s in signals if s.get("kind") == "missed")
    errors = collections.Counter(e["cmd"] for e in cmds if e.get("status") == "error")
    used = collections.Counter(e["cmd"] for e in cmds if e.get("status") != "notfound")

    def notes(kind):
        seen, out = set(), []
        for s in inbox + signals:  # the same wish may be on disk and in beehive
            key = (s.get("text"), s.get("t"))
            if s.get("kind") == kind and key not in seen:
                seen.add(key)
                out.append({"t": s.get("t"), "text": s.get("text"), "build": s.get("build")})
        return out

    crash_list = [{"t": c.get("t"), "build": c.get("build"), "kind": c.get("kind"),
                   "cmd": c.get("cmd"), "msg": c.get("msg")} for c in crashes]
    seen_msgs = {c["msg"] for c in crash_list}
    crash_list += [{"t": s.get("t"), "build": s.get("build"), "kind": "crash", "cmd": None, "msg": s.get("text")}
                   for s in signals if s.get("kind") == "crash" and s.get("text") not in seen_msgs]
    boots = [{"t": e.get("t"), "boot": e.get("boot"), "build": e.get("build"), "boot_ms": e.get("boot_ms")}
             for e in events if e.get("ev") == "boot"]
    return {
        "boots": boots,
        "crashes": crash_list,
        "missed_commands": missed.most_common(),
        "wishes": notes("wish"),
        "feedback": notes("feedback"),
        "commands": {"total": len(cmds), "used": used.most_common(), "errors": errors.most_common()},
    }


def print_report(r: dict):
    def section(title):
        print(f"\n== {title} " + "=" * max(0, 60 - len(title)))

    print("BeeCodersOs report")
    section(f"Crashes ({len(r['crashes'])})")
    for c in r["crashes"] or [{"msg": None}]:
        if c["msg"] is None:
            print("  none")
        else:
            print(f"  {c['t']}  b{c['build']}  {c['kind']} in '{c['cmd'] or '?'}': {c['msg']}")
    section("Commands people typed that don't exist")
    for cmd, n in r["missed_commands"] or [("none", "")]:
        print(f"  {n:>4}  {cmd}" if n else "  none")
    for kind in ("wishes", "feedback"):
        section(f"{kind.capitalize()} ({len(r[kind])})")
        for w in r[kind] or [{"text": None}]:
            print(f"  {w['t']}  b{w['build']}  {w['text']}" if w["text"] else "  none")
    c = r["commands"]
    section(f"Commands ({c['total']} run)")
    errors = dict(c["errors"])
    for cmd, n in c["used"][:15]:
        e = errors.get(cmd, 0)
        print(f"  {cmd:<12} {n:>5} runs" + (f", {e} failed ({100 * e // n}%)" if e else ""))
    section(f"Boots ({len(r['boots'])})")
    for b in r["boots"][-10:]:
        print(f"  #{b['boot']:<4} {b['t']}  b{b['build']:<5} shell ready after {b['boot_ms']} ms")


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--disk", type=Path, help="BeeFS disk image (VM must be off)")
    ap.add_argument("--beehive", type=Path, nargs="?", const=Path.home() / ".local/state/beehive",
                    help="beehive state directory")
    ap.add_argument("--json", action="store_true", help="print JSON")
    args = ap.parse_args()
    if not args.disk and not args.beehive:
        ap.error("give --disk and/or --beehive")
    report = collect(args.disk, args.beehive)
    if args.json:
        print(json.dumps(report, indent=2, ensure_ascii=False))
    else:
        print_report(report)


if __name__ == "__main__":
    main()
