#!/usr/bin/env python3
"""Make website/commands.html from the OS's own help (COMMANDS in kernel/src/shell.rs)
and its desktop shortcuts (KEYS in kernel/src/launcher.rs).

Run it after commands change: python3 website/scripts/commands.py
The page has no inline styles or scripts (the site's CSP forbids them).
"""
import html
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
SHELL = ROOT / "kernel" / "src" / "shell.rs"
OUT = ROOT / "website" / "commands.html"
BUILD = (ROOT / "BUILD_NUMBER").read_text().strip()

source = SHELL.read_text()
block = source[source.index("const COMMANDS: &[(&str, &str)] = &["):]
block = block[: block.index("\n];")]
entry = re.compile(r'\(\s*"((?:[^"\\]|\\.)*)",\s*"((?:[^"\\]|\\.)*)"\s*\),')
commands = [(a.encode().decode("unicode_escape"), b.encode().decode("unicode_escape")) for a, b in entry.findall(block)]
if len(commands) < 20:
    raise SystemExit(f"only {len(commands)} commands found: has the COMMANDS list changed shape?")

# The desktop's keyboard shortcuts (KEYS in kernel/src/launcher.rs, which Super+K shows).
launcher = (ROOT / "kernel" / "src" / "launcher.rs").read_text()
keys_block = launcher[launcher.index("pub const KEYS:"):]
keys_block = keys_block[: keys_block.index("\n];")]
keys = [(a.encode().decode("unicode_escape"), b.encode().decode("unicode_escape")) for a, b in entry.findall(keys_block)]
if len(keys) < 5:
    raise SystemExit(f"only {len(keys)} keys found: has the KEYS list changed shape?")
key_items = "\n".join(
    f'      <div class="cmd"><dt><kbd>{html.escape(k)}</kbd></dt><dd>{html.escape(t)}</dd></div>' for k, t in keys
)

items = "\n".join(
    f'      <div class="cmd"><dt><code>{html.escape(name)}</code></dt><dd>{html.escape(text)}</dd></div>' for name, text in commands
)
page = f"""<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>Commands — BeeCodersOs</title>
<meta name="description" content="Every command of BeeShell, the terminal of BeeCodersOs, and every desktop shortcut, as the OS's own help describes them.">
<meta name="theme-color" content="#0d0b07">
<link rel="icon" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 100 100'%3E%3Cpolygon points='50,4 92,27 92,73 50,96 8,73 8,27' fill='%23f5b301'/%3E%3Cpolygon points='50,30 70,41 70,63 50,74 30,63 30,41' fill='%230d0b07'/%3E%3C/svg%3E">
<link rel="preconnect" href="https://fonts.googleapis.com">
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Space+Grotesk:wght@500;700&family=Inter:wght@400;500;600&family=JetBrains+Mono:wght@400;600&display=swap" rel="stylesheet">
<link rel="stylesheet" href="style.css?v={BUILD}">
</head>
<body>

<a class="skip" href="#main">Skip to content</a>

<header class="top">
  <a class="brand" href="index.html"><span class="logo" aria-hidden="true"></span>BeeCoders<b>Os</b></a>
  <nav aria-label="Main">
    <a href="index.html#features">Features</a>
    <a href="index.html#progress">Progress</a>
    <a href="commands.html" aria-current="page">Commands</a>
    <a class="gh" href="https://github.com/bartbeecoders/beecodersos">GitHub</a>
  </nav>
</header>

<main id="main">
  <section class="wrap commands">
    <p class="kicker">BeeShell</p>
    <h2>Every <em>command</em>.</h2>
    <p class="sub">What <code>help</code> shows in BeeCodersOs build {BUILD}: {len(commands)} entries, taken from the
      OS's own source (<code>COMMANDS</code> in <code>kernel/src/shell.rs</code>), so this page says what the OS says.
      Paths can be relative; <code>"quotes"</code> group words.</p>
    <dl class="cmds">
{items}
    </dl>
  </section>
  <section class="wrap commands" id="keys">
    <p class="kicker">Desktop</p>
    <h2>Every <em>key</em>.</h2>
    <p class="sub">The desktop's keyboard shortcuts, as on Omarchy (<code>KEYS</code> in <code>kernel/src/launcher.rs</code>;
      Super+K shows them on the machine itself).</p>
    <dl class="cmds">
{key_items}
    </dl>
  </section>
</main>

<footer class="wrap foot">
  <span class="brand small"><span class="logo" aria-hidden="true"></span>BeeCoders<b>Os</b></span>
  <p>A <a href="https://beecoders.com">BeeCoders</a> project. Written with AI, reviewed by people.</p>
  <a href="https://github.com/bartbeecoders/beecodersos">GitHub</a>
</footer>

</body>
</html>
"""
OUT.write_text(page)
print(f"{OUT.relative_to(ROOT)}: {len(commands)} commands, {len(keys)} keys")
