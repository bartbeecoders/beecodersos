# BeeCodersOs

A from-scratch operating system for amd64 (x86_64), written in Rust and
assembly, that helps improve itself. It records what goes wrong and what
people miss, an AI turns that into small tested changes that a person
approves, and each new kernel is tried in a spare slot and rolled back
automatically if it fails.

Current version: **0.0.2** (see [Versioning](#versioning)).

## At a glance

| Phase | What it added | Status |
|---|---|---|
| 1 | 64-bit kernel, boots via Multiboot / Limine (BIOS + UEFI) | ✅ |
| 2 | BeeFS file system (RAM disk, ATA disk, host tool) | ✅ |
| 3 | BeeShell terminal, keyboard layouts, ACPI power | ✅ |
| 4 | Automated tests: `make check` | ✅ |
| 5 | The OS observes itself: clock, event log, crash reports, `wish` | ✅ |
| 6 | AI bridge: `ask` / `explain` through beehive on the host | ✅ |
| 7 | Improvement loop: signals → backlog → coding agent → review → ship | ✅ |
| 8 | A/B kernel slots with automatic rollback | ✅ |
| 9 | AI-written apps in a WebAssembly sandbox | planned |
| N1 | Networking: PCI and the e1000 network card driver | ✅ |
| N2 | Networking: ARP, IPv4, ICMP: `ping` | ✅ |
| N3 | Networking: UDP, DHCP (automatic address), DNS (names) | ✅ |
| N4 | Networking: TCP connections (`nc`) | ✅ |
| N5 | Networking: HTTP (`wget`), time (`ntp`), file transfer (`tftp`) | ✅ |
| T | Cooperative kernel threads, console sessions | ✅ |
| N6 | Networking: status page (`httpd`) ✅, remote shell (`remote`) ✅; network signals, kernel download | in progress |

The roadmap is `Docs/self-improving-os-plan.html`, the network plan is
`Docs/network-stack-plan.html`; each phase is described below.

## Quick start

```sh
make run-serial              # build and boot in this terminal (quit: Ctrl-A then X)
make check                   # all tests, headless (~1 min)
make beehive                 # AI bridge on the host; then `ask ...` in the OS
make evolve                  # turn collected signals into a change for you to review
python3 tools/evolve/evolve.py approve N --deploy   # ship it to the VM (rollback if it fails)
```

**Requirements** (Arch package names in brackets):
- Rust with the `x86_64-unknown-none` target (`kernel/rust-toolchain.toml`
  requests it), `make`, `qemu-system-x86_64` [qemu-full], `objcopy` [binutils]
- `limine` and `xorriso` [libisoburn] for the ISO and the boot disk;
  `edk2-ovmf` for UEFI boots
- Python 3.11+ for the tests and host tools; `uv` to run beehive with Claude
  (it installs the Anthropic SDK); optional: the `claude` / `grok` CLIs,
  Ollama, `dosfstools` (`fsck.fat` in the tests)
- libvirt / virt-manager for the VM (`vm1`), optional

No NASM or GCC cross-compiler needed: the assembly is built by Rust's LLVM
assembler and linked with the bundled `rust-lld`.

**Where things are documented**

| What | Where |
|---|---|
| Design and every phase | this README |
| Rules for AI agents (and humans) changing the code | `CLAUDE.md` |
| Roadmap | `Docs/self-improving-os-plan.html` |
| Network stack plan (phases N1–N6, threads) | `Docs/network-stack-plan.html` |
| Backlog and review reports | `Docs/backlog/` (see its README) |
| Tool options | `--help` of `tools/beehive/beehive.py`, `tools/evolve/evolve.py`, `tools/bootdisk.py`, `tools/deploy.py`, `tools/usbstick.py`, `tools/bee_report.py`, `tests/boot_test.py` |
| Shell commands | `help` in the OS |

## Troubleshooting

- **The VM window keeps the keyboard and mouse:** press **Left Ctrl + Left Alt**
  to release them (virt-manager). While the VM has focus, Hyprland's own
  shortcuts go to the VM too.
- **virt-manager: "network 'default' is not active":**
  `virsh -c qemu:///system net-start default` and, to start it at every boot,
  `virsh -c qemu:///system net-autostart default`.
- **`ask` says "AI offline":** start `make beehive` on the host. `ai` in the OS
  shows the bridge status; `ai off` / `ai on` switches AI features.
- **No network address:** `net` shows the state. The OS asks for an address
  by itself (DHCP) at boot; `net dhcp` asks again and shows the result. If the
  network has no DHCP server, set one by hand: `net static IP/PREFIX [GATEWAY]`
  and `net dns ADDRESS`.
- **Names don't resolve:** `net` shows the DNS server; `net dns 1.1.1.1` sets one
  by hand, `net dns auto` goes back to the one DHCP gives.
- **`evolve work` refuses to start:** the coding agent only sees committed
  code; commit your changes first (it lists them).
- **A deploy was rolled back:** `slots` in the OS shows which slot runs and the
  last trial; `make report` lists the crash; `python3 tools/bootdisk.py status build/beecoders.img`
  shows the slot state from the host (VM off).

## Phase 1 — Hello World kernel ✅

Boots in QEMU/KVM and shows `Hello World, from BeeCodersOs` on screen (VGA text
mode) and on the serial port (COM1).

### Build & run

```sh
make             # -> build/beecoders.elf
make run         # QEMU + KVM: VGA window, plus the terminal on this console
make run-nokvm   # pure emulation
make run-serial  # headless, serial only (quit: Ctrl-A then X)
make run-disk    # boot the A/B boot disk (phase 8) instead of the kernel file
```

### Bootable ISO

```sh
make iso           # -> build/beecoders.iso (hybrid BIOS + UEFI, Limine bootloader)
make run-iso       # boot the ISO in QEMU (BIOS)
make run-iso-uefi  # boot the ISO in QEMU with OVMF (UEFI)
```

Write it to a USB stick for real hardware with `tools/usbstick.py`:

```sh
tools/usbstick.py            # list the USB sticks that are plugged in
tools/usbstick.py /dev/sdX   # erase that stick and write the ISO to it
```

It only writes to a removable whole disk on USB of at most 128 GB that holds
no part of the running system, shows what is on the stick, makes you type
the device name, and reads the image back to compare. `--dry-run` does the
checks without writing. Writing needs `sudo`.

On a real PC, boot the stick in **legacy BIOS mode**: the screen is VGA
text mode, which UEFI firmware does not provide (a UEFI boot runs, but
shows nothing; only the serial port talks). The OS knows a PS/2 keyboard
(or a USB keyboard the firmware presents as one), a disk on the legacy IDE
ports and an Intel e1000 network card; without those it runs from a RAM
disk and without a network. A disk that is not BeeFS is never written to.
(double-check `/dev/sdX`!). Limine loads the kernel with the Multiboot 1 protocol
(`boot/limine.conf`).

> UEFI note: UEFI firmware has no VGA text mode, so on UEFI all output (and the
> terminal) only appears on the serial port; the keyboard still works. On-screen
> output on UEFI needs a framebuffer text renderer (planned for a later phase).
> BIOS/CSM boots show everything on screen.

Or by hand: `qemu-system-x86_64 -enable-kvm -kernel build/beecoders.elf`

### Layout

```
boot/limine.conf       Limine boot menu entry (ISO)
beefs/                 BeeFS filesystem library (no_std + alloc, shared by kernel and host)
  src/device.rs        BlockDevice trait + RamDisk
  src/fs.rs            superblock, bitmap, inodes, directories, path ops (+ unit tests)
  src/partition.rs     MBR partitions: BeeFS inside partition 2 of the boot disk
beenet/                network stack library (no_std + alloc, no hardware access; unit-tested on the host)
  src/wire.rs          packet formats: Ethernet, ARP, IPv4, ICMP, internet checksum
  src/stack.rs         the stack: ARP cache, routing, ping, UDP sockets; frames in, frames out
  src/dhcp.rs          DHCP client: automatic address, renewal
  src/dns.rs           DNS resolver: names to addresses, with a cache
  src/tcp.rs           TCP connections
  src/http.rs          HTTP/1.1 client: addresses, requests, decoding answers
  src/sntp.rs          SNTP: the time from a time server
  src/tftp.rs          TFTP client: fetch / store a file
  src/telnet.rs        Telnet, server side: what a remote shell needs
  src/tcp.rs           TCP: connections, retransmission, flow control, close and reset
tools/beefs-tool/      host CLI to create/inspect/edit BeeFS images (and boot disks)
tests/boot_test.py     boot tests: drives the kernel in headless QEMU over serial
tests/scenarios/       shell scenario tests (*.scn)
tests/test_*.py        unit tests of the host tools (beehive, evolve, bootdisk)
tools/beehive/         beehive: host daemon for the AI bridge (Claude / local models)
tools/bee_report.py    report of what the OS recorded (crashes, missing commands, wishes)
tools/evolve/          improvement loop: signals -> backlog -> coding agent -> review
tools/bootdisk.py      A/B boot disk: FAT16 boot partition (Limine, two kernels) + BeeFS
tools/deploy.py        deploy to the libvirt VM's spare slot, with a watchdog
tools/usbstick.py      write the ISO to a USB stick, with checks against the wrong disk
Docs/backlog/          the backlog (one file per item) and review reports
Docs/                  plans (self-improving OS roadmap)
CLAUDE.md              rules for AI agents working on the code
kernel/
  linker.ld            kernel loaded at 1 MiB
  src/boot.s           Multiboot header, CPU checks, page tables (4 GiB), 32→64-bit switch
  src/interrupts.s/.rs IDT, exception handlers, 8259 PIC, IRQ dispatch
  src/main.rs          kernel_main, panic handler
  src/bootargs.rs      kernel command line (Multiboot cmdline): `test`, `selftest` flags
  src/testmode.rs      test mode: pass/fail exit codes for QEMU (isa-debug-exit)
  src/selftest.rs      in-kernel self-tests (`selftest` command / boot flag, health check)
  src/time.rs          PIT timer (IRQ0, 100 Hz) + CMOS real-time clock
  src/eventlog.rs      event log /system/log/events.jsonl
  src/crash.rs         crash reports saved to disk, shown on the next boot
  src/json.rs          minimal JSON (event log, crash reports, AI protocol)
  src/aibridge.rs      AI bridge: JSON lines on COM2 to beehive on the host
  src/bootslots.rs     A/B kernel slots: arm the rollback, health check, report failed trials
  src/pci.rs           PCI: find devices, read their configuration (`lspci`)
  src/e1000.rs         Intel e1000 network card driver (polled descriptor rings)
  src/net.rs           card + beenet stack: polling, settings, TCP streams, HTTP / time / TFTP transfers
  src/thread.rs        cooperative kernel threads: stacks, switching, waiting
  src/httpd.rs         status page for the local network (off by default)
  src/remote.rs        remote shell with a password, Telnet style (off by default)
  src/session.rs       console sessions: a shell that is not on the real screen and keyboard
  src/rand.rs          random numbers (RDRAND, or a seeded generator)
  src/allocator.rs     kernel heap in free RAM from the Multiboot memory map
  src/console.rs       print!/println! to VGA + serial, line redraw helpers
  src/keyboard.rs      PS/2 keyboard driver + scancode decoder (modifiers, dead keys)
  src/layouts.rs       keyboard layouts (US, Belgian Period)
  src/serial.rs        16550 UART (COM1) output + interrupt-driven input
  src/input.rs         unified key events, sleeps (hlt) until input arrives
  src/editor.rs        line editor with history
  src/shell.rs         BeeShell commands (incl. `calc`, the first change made by the improvement loop)
  src/storage.rs       mount BeeFS disk or fall back to a RAM disk
  src/ata.rs           ATA/IDE disk driver (PIO, LBA28, polling)
  src/acpi.rs          ACPI tables (FADT/DSDT), power button (SCI), S5 power-off, reset
  src/power.rs         reboot / shutdown (ACPI first, then fallbacks)
  src/vga.rs           80x25 VGA text writer (UTF-8 → code page 437), hardware cursor
  src/ring.rs          lock-free queue for IRQ → main hand-off
  src/port.rs          in/out port I/O
```

### Boot flow

1. Limine (ISO or boot disk) or QEMU's own Multiboot v1 loader (`-kernel`) loads the
   kernel at 1 MiB and enters `_start` in 32-bit protected mode.
2. `boot.s` checks the Multiboot magic, CPUID and long-mode support (prints `ERR: M/C/L` on failure).
3. It identity-maps the first 4 GiB using 2 MiB pages, enables PAE + EFER.LME + paging,
   loads a 64-bit GDT and far-jumps into 64-bit mode.
4. `kernel_main(magic, multiboot_info)` prints the greeting, saves the kernel
   command line, puts the heap in free RAM and, on a trial kernel, arms the A/B
   rollback (phase 8).
5. It sets up interrupts, the timer and clock, the keyboard, COM1 and the COM2
   AI bridge, reads ACPI, starts the network card, and mounts BeeFS (a disk, the boot disk's partition 2,
   or a RAM disk).
6. It reports a crash from the previous boot, logs the boot, runs a trial
   kernel's health check, and starts the terminal.

## Phase 2 — BeeFS filesystem ✅

BeeFS is an inode-based filesystem written in Rust (`beefs/`). It runs on any
block device: a **RAM disk** in memory, an **ATA disk** attached to the VM, or a
disk **image file** on the host.

**Operations:** format, mount, `mkdir`, `mkdir_all`, `create`, `write_file`,
`append`, `read_file`, `read_at`, `read_dir`, `metadata`, `exists`, `rename`
(move), `remove` (file or empty dir), `stats`.

**On-disk format** (512-byte blocks): superblock → block bitmap → inode table
(64-byte inodes: 10 direct + single + double indirect pointers, max file ≈ 8 MiB)
→ data. Directories hold 32-byte entries; names up to 26 bytes.

At boot the kernel mounts an attached ATA disk containing BeeFS (incrementing
`/system/boot_count` and appending to `/system/boot.log`); on a partitioned
boot disk (phase 8) it uses the BeeFS partition. Without one it uses a 4 MiB
RAM disk. Disks without BeeFS are **never** formatted automatically.

```sh
make disk     # (re)create build/disk.img: 16 MiB BeeFS (run targets create it if missing)
make run      # boots with the disk attached; data persists between runs
make test     # BeeFS unit tests on the host

# inspect / edit the disk from the host (while QEMU is not running)
T=tools/beefs-tool/target/release/beefs-tool
$T build/disk.img tree
$T build/disk.img cat /system/boot.log
$T build/disk.img put notes.txt /home/bee/notes.txt
$T build/disk.img info          # also: ls, mkdir, get, write, rm, mv
$T build/beecoders.img tree     # the boot disk: its BeeFS partition is found by itself
```

Attach a disk manually: `-drive file=build/disk.img,format=raw,if=ide,index=0,media=disk`

## Phase 3 — Terminal (BeeShell) ✅

The terminal starts at boot. Type on the QEMU window (PS/2 keyboard) or in the
terminal you started QEMU from (serial console; `make run-serial` for serial only).

**Keyboard layouts:** Belgian (Period) is the default. `layout` lists them, `layout be` / `layout us` switches.
The choice is saved in `/system/keyboard` and restored at boot.

| id | layout | notes |
|----|--------|-------|
| `us` | English (US) QWERTY | |
| `be` | Belgian (Period) AZERTY — **default** | AltGr: `@` `#` `{` `}` `[` `]` `€` `\|` `\`; dead keys `^` `¨` `´` `` ` `` `~` (`^` then `e` = `ê`); keypad decimal = `.` |

Serial input bypasses the layout: the host terminal has already translated keys.

**Line editing:** ←/→, Home/End, Backspace/Delete, ↑/↓ history, Esc clears the
line, Ctrl+C cancels, Ctrl+L clears the screen, Ctrl+A/E/U/K as in bash.

**Commands:** `help`, `clear`, `echo` (with `>` / `>>` redirection), `ls`, `cd`, `pwd`,
`cat`, `touch`, `mkdir`, `rm`, `mv`, `cp`, `tree`, `df`, `layout`, `history`,
`mem`, `about`, `calc`, `date`, `uptime`, `log`, `wish`, `feedback`, `ask`, `explain`, `ai`,
`slots`, `net`, `ping`, `nslookup`, `nc`, `wget`, `ntp`, `tftp`, `httpd`, `remote`, `arp`, `lspci`, `threads`, `selftest`, `crash`, `reboot`, `shutdown`. `help` in the OS describes each one. Paths can be relative; `"quotes"` group words.

**Under the hood:** interrupts are enabled (IDT + remapped PIC; IRQ0 timer,
IRQ1 keyboard, IRQ3 COM2 AI bridge, IRQ4 COM1 serial). Handlers queue raw bytes; the shell decodes them and sleeps with
`hlt` while waiting, so the VM uses no CPU when idle. CPU exceptions print a
register dump instead of silently rebooting.

### Running in virt-manager (libvirt)

**`vm1` boots from the A/B boot disk** (phase 8): `build/beecoders.img` is its
first disk, with the kernels and its files (BeeFS). New kernels go to it with
`make deploy`, not `make run-vm`. To set up a VM like that:
`make bootdisk`, then `python3 tools/deploy.py setup-vm [--vm NAME]` (backs up
the old config; the old disk is detached, not deleted). The VM also needs the
COM2 port for the AI bridge (phase 6).

**From the ISO** (a new VM, or without the boot disk): create a VM from `build/beecoders.iso` (OS type: "Generic" / other, BIOS firmware,
i440fx or Q35). The virt-manager **Shut Down** and **Reboot** buttons work: they
press the ACPI power button, which BeeCodersOs handles by powering off (libvirt
then restarts it for Reboot). `shutdown` and `reboot` work inside the VM too.

- During virt-manager's first "install" boot, libvirt turns a guest reboot into a
  power-off; that's libvirt's install mode, not the OS. Later boots restart normally.
- After that first boot virt-manager removes the CD-ROM from the boot order: enable
  it again in *VM details → Boot Options* or the VM won't find the OS.
- With UEFI firmware the screen stays blank (serial only) and the power button is
  not detected yet (`shutdown`/`reboot` commands still work).
- `make iso` replaces the ISO file, so rebuilding works while libvirt has it attached;
  `make run-vm` restarts an ISO-booted VM with the new build.

## Phase 4 — Automated tests ✅

One command checks the whole OS without anyone watching a screen:

```sh
make check    # unit tests (BeeFS, beenet, host tools) + kernel self-tests + shell scenarios + ISO boots
```

Example output (from when this phase was added; there are more tests now):

```
Running 7 boot tests (KVM)
ok    selftest (0.2s): selftest: 11 passed, 0 failed
ok    scenario files (0.5s): 21 commands
ok    scenario persistence (0.7s): 9 commands
...
7 passed, 0 failed in 4.2s
```

It exits 0 only when everything passes, so it can gate any change, by a
person or an AI (this is the first step of `Docs/self-improving-os-plan.html`).

- **Test mode:** with `test` on the kernel command line (`qemu -append test`), a
  kernel panic or CPU exception ends QEMU at once through the `isa-debug-exit`
  device (exit status 35) instead of hanging.
- **Self-tests** (`kernel/src/selftest.rs`) check kernel code inside the
  kernel: heap, IRQ queue, serial and PS/2 key decoding (US, Belgian, dead
  keys), shell parsing and BeeFS on a RAM disk. Run them with the `selftest`
  shell command, or boot with `-append "test selftest"` (exit 33 = pass).
- **Shell scenarios** (`tests/scenarios/*.scn`) type commands on the serial
  console and check the output:

  ```
  @disk                          # boot with a fresh BeeFS disk (default: RAM disk)
  $ echo "remember me" > /note.txt
  @reboot                        # power off and boot again with the same disk
  $ cat /note.txt
  remember me                    # exact output line
  ~ ^remember                    # or a regex; `!~ REGEX` = must not appear
  ```

  Later phases added `@ai` / `@ai-offline` (COM2 with or without beehive),
  `$! CMD` (the command must crash the kernel), `$^C CMD` (Ctrl+C after 1 s),
  `@signal REGEX` (beehive must have received it), `@bootdisk` /
  `@deploy` / `@deploy-crash` (A/B slots), `@wire` / `@frame` / `@send`
  (a network cable to the test), `@net` (QEMU's built-in network), `@dns`
  (a fake DNS server), `@tcp` (small TCP services) and `$& CMD` / `> TEXT` /
  `$wait` (type into a running command). Scenarios without `@wire` or `@net` have no network card. The full format is at the top of
  `tests/boot_test.py`.
- **Host tool tests:** `tests/test_beehive.py`, `tests/test_evolve.py` and
  `tests/test_bootdisk.py` (Python `unittest`, no LLM or login needed).
- **ISO boots:** the Limine ISO boots under BIOS and UEFI (OVMF), answers
  `version` and powers off cleanly.

Single suites: `tests/boot_test.py selftest|scenarios|iso|iso-uefi [FILE..] [-v] [--no-kvm]`
(`-v` prints all serial output). A failure prints the expected line and what
the OS printed instead.

## Phase 5 — The OS observes itself ✅

The OS now records what happens to it, so problems and missing features can be
found and fixed (by a person or, later, an AI):

- **Clock:** PIT timer (IRQ0, 100 Hz) and the CMOS real-time clock: `date`
  (UTC), `uptime`, and timestamps for everything below.
- **Event log** `/system/log/events.jsonl` (JSON lines, rotated at 64 KiB):
  every boot (build, boot number, time to shell) and every command (name,
  ok / error / notfound, duration). Commands are logged by **name only**, not
  their arguments. `log [N]` shows the last entries.
- **Missing commands:** an unknown command is logged, and BeeShell suggests the
  closest real one (`mkdri` -> "did you mean 'mkdir'?").
- **Wishes and feedback:** `wish a text editor`, `feedback "ls could use colors"`
  go to `/system/inbox/signals.jsonl`.
- **Crash reports:** a kernel panic or CPU exception writes a report to
  `/system/crash/pending.jsonl`; the next boot shows it, moves it to
  `history.jsonl` and offers `explain`. Safety rule: the report is only written
  when no filesystem operation was in progress, so a crash can't corrupt files.
  `crash` crashes the kernel on purpose (to test this).
- **Host report:** `make report` reads the VM's disk (`build/beecoders.img` if
  it exists, else `build/disk.img`; the VM should be off) and beehive's
  signals. By hand: `tools/bee_report.py --disk IMAGE --beehive [--json]`.

```
== Crashes (1) ==================================================
  2026-11-14T15:01:50Z  b9  panic in 'crash': panicked at src/shell.rs:240:30: ...
== Commands people typed that don't exist =======================
     2  edit
== Wishes (1) ===================================================
  2026-11-14T15:01:50Z  b9  a text editor
```

## Phase 6 — AI bridge ✅

BeeCodersOs can ask an LLM for help, without a network stack and without ever
holding an API key:

```
 BeeCodersOs (VM)                          host
 ┌──────────────────┐   COM2, JSON lines   ┌────────────────────────┐
 │ ask / explain /  │ ───────────────────▶ │ beehive (tools/beehive)│──▶ Claude API
 │ wish / crashes   │ ◀─────────────────── │ API key, budget, logs  │    (or a local model)
 └──────────────────┘  TCP 127.0.0.1:4555  └────────────────────────┘
```

- **In the OS:** `ask how do I copy a folder?` answers on screen (Ctrl+C
  cancels). `explain` explains the last failed command, or the crash of the
  previous boot. `ai` shows the bridge status; `ai off` / `ai on` is the kill
  switch (saved in `/system/ai`). Wishes, feedback, missing commands and crash
  reports are also sent to beehive when it runs. Without beehive, or without a
  COM2 port, everything else works as before.
- **On the host:** `make beehive` starts the daemon. It connects to the VMs'
  COM2 ports (`127.0.0.1:4555` = libvirt `vm1`, `127.0.0.1:4556` = `make run`),
  and reconnects when a VM restarts.
  - **Claude (default):** `claude-opus-5-5` through the official Anthropic SDK
    (`uv` installs it on first run), effort `low` for short console answers,
    server-side fallback if a request is declined. Needs `ANTHROPIC_API_KEY`
    (or `ant auth login`).
  - **Claude Code / grok CLIs:** use the coding agents installed on this computer,
    with their own logins (no API key needed):
    `make beehive BEEHIVE_ARGS="--backend claude-code"` or `BEEHIVE_ARGS="--backend grok"`
    (`--model` and `--effort` are passed on). They run as plain chat models:
    every tool, MCP server, plugin and user setting is switched off, they run in
    an empty folder, and beehive kills them if they start a tool call anyway,
    so nothing typed in the VM can run commands on the host. Answers take a few
    seconds longer, because each question starts the CLI.
  - **Local or other models:** any OpenAI-compatible endpoint, e.g. Ollama:
    `make beehive BEEHIVE_ARGS="--backend openai --base-url http://localhost:11434/v1 --model llama3.2"`
    (add `--api-key-file` for OpenRouter).
  - **Fake:** `--backend fake` answers without an LLM (used by the tests).
  - Settings can go in `~/.config/beehive/config.toml` (`backend = "openai"`, ...).
  - The OS's README (this file) is the assistant's reference for answers.
  - Limits and logs in `~/.local/state/beehive/`: `--max-requests-per-day`
    (default 200), every request with its answer in `exchanges.jsonl`, signals
    in `signals.jsonl`.
- **Protocol** (one JSON object per line, documented in `kernel/src/aibridge.rs`):
  `hello`/`welcome`, `ask`/`explain` -> `ack`, `line`..., `done` (or `error`),
  `cancel`, and fire-and-forget `signal`s.
- **QEMU:** `make run` adds COM2 as a TCP server on port 4556. **libvirt:** `vm1`
  has a second serial port: *Add Hardware → Serial*, mode TCP, bind
  `127.0.0.1:4555`, raw protocol. Or in XML:
  `<serial type='tcp'><source mode='bind' host='127.0.0.1' service='4555'/><protocol type='raw'/><target port='1'/></serial>`

Tests: `tests/test_beehive.py` drives the CLI backends with fake `claude` / `grok`
executables (streaming, blocked tool calls, errors, Ctrl+C); `ai-bridge.scn` runs beehive with the fake backend (ask, explain,
Ctrl+C, kill switch, signals arriving on the host); `ai-offline.scn` checks the
OS without beehive.

## Phase 7 — The improvement loop ✅

What people do in the OS turns into tested changes; the only human step is
approving them:

```
 signals ──▶ backlog ──▶ coding agent ──▶ verify ─────▶ review ──▶ approve ──▶ ship
 (the OS)    Docs/backlog  own git worktree   make check     report    (you)      merge, new build, ISO
```

```sh
make evolve                                   # harvest + work on the top item
python3 tools/evolve/evolve.py status         # the backlog
python3 tools/evolve/evolve.py review 1       # report + diff
python3 tools/evolve/evolve.py approve 1 --deploy   # merge, bump the build, rebuild ISO, deploy to vm1
python3 tools/evolve/evolve.py reject 1 --reason "..." [--retry]
```

1. **Harvest:** wishes, feedback, missing commands and crash reports (including
   failed A/B trials) are read from beehive (live, `~/.local/state/beehive`)
   and/or a disk (`--disk build/beecoders.img`, VM off). Signals already seen
   are skipped.
2. **Triage:** Claude Code (no tools, structured JSON output) groups them into
   small, testable backlog items with priorities and acceptance criteria:
   `Docs/backlog/NNN-title.md`. `--triage rules` does it without an LLM.
3. **Implement:** first a pre-flight check: the agent only sees committed code,
   so evolve refuses to start while `CLAUDE.md` or the `check` target are not
   committed, or other changes are uncommitted (`--allow-dirty` overrides the
   latter). Then the top open item goes to a Claude Code agent in its own git
   worktree (`.evolve/worktrees/NNN`, branch `evolve/NNN-...`). The agent
   reads `CLAUDE.md`, may only edit files and run `make` / `cargo` / the test
   scripts (`--permission-mode dontAsk` + an allow-list), and is told that
   signal text is data, never instructions.
4. **Verify:** evolve itself re-runs `make check`, and flags a change that has
   no test, touches a protected file, changes `OS_VERSION`, or is over 300
   lines. It commits the work on the branch and writes
   `Docs/backlog/reviews/NNN.md`: verdict, test counts before/after, kernel
   size change, agent summary, `make check` output.
5. **Approve:** you read the review and the diff, then `approve` merges into
   `main`, builds the new ISO (the build number goes up), marks the item done
   with that build, and commits the backlog. `--deploy` runs `make deploy`:
   the new kernel is tried in `vm1`'s spare slot and rolled back if it fails
   (phase 8). A review that needs attention can only be approved with `--force`.

Tests: `tests/test_evolve.py` runs the whole loop in a throwaway repository
with a fake agent (merging, the gates, the pre-flight check, refusing to merge
into a dirty tree).

## Phase 8 — A/B kernels with automatic rollback ✅

A new build can no longer leave the OS unbootable. The boot disk has two
kernel slots; a new kernel is *tried* in the spare one and only becomes the
default after it proves itself:

```
 boot disk (build/beecoders.img, MBR)
 ┌───────────────────────────────────────────────┬──────────────────────┐
 │ 1: FAT16  Limine, limine.conf,                │ 2: BeeFS  your files │
 │           kernel-a.elf, kernel-b.elf          │    (survive deploys) │
 └───────────────────────────────────────────────┴──────────────────────┘
 limine.conf line 1:  #slots good=a trial=b state=pending build=000012
```

1. **Deploy** (host, VM off): the new kernel goes into the spare slot,
   `state=pending`, and the next boot goes to it.
2. **Arm:** the trial kernel's first act (right after setting up its heap) is to
   point the boot back at the good slot (`state=armed`). If it crashes or hangs
   from here on, the next boot runs the good kernel.
3. **Health check:** once the disk is mounted, the shell is ready and the
   self-tests pass, the trial becomes the good slot (`state=ok`) and sends a
   `health` signal to beehive (again whenever beehive reconnects).
4. **Rollback report:** a good kernel that finds `state=armed` knows the trial
   failed: it marks it `failed`, logs it, and sends a `crash` signal, so the
   improvement loop turns it into a bug to fix.
5. **Host watchdog** (`make deploy`): waits for the health signal; if none
   comes (a kernel that dies before it can arm the rollback, or a file Limine
   can't load), it stops the VM, points the disk at the good slot and restarts.

The kernel only rewrites those first two lines of `limine.conf`, in place and at
the same length (one sector), so the FAT file system never changes shape.
`slots` in the shell shows the state.

```sh
make bootdisk                      # create build/beecoders.img (once; it holds the VM's files)
python3 tools/deploy.py setup-vm   # once: vm1 boots from it (old config backed up)
make beehive                       # so the watchdog can hear the VM
make deploy                        # new kernel -> spare slot -> trial -> good, or rollback
python3 tools/bootdisk.py status build/beecoders.img
make run-disk                      # boot the boot disk in plain QEMU
```

`evolve approve N --deploy` uses `make deploy`, so an approved change that
fails on the VM is rolled back automatically and reported. The FAT16 file
system is written by `tools/bootdisk.py` itself (with long file names, which
Limine needs), so no mtools or root rights are needed; BIOS and UEFI both boot it.
`beefs-tool` and `make report` find BeeFS in partition 2 by themselves.

Tests: `slots.scn` boots a boot disk, deploys a good build (becomes the good
slot) and a broken one (`failboot`: crashes after arming; the good slot
boots again and reports it), checks that files survive and that beehive gets
both signals. `tests/test_bootdisk.py` covers the FAT16 writer/reader, the
state changes and the watchdog's verdicts.

## Networking — phase N1: the network card ✅

The first step of `Docs/network-stack-plan.html` (the path to ping, DHCP, DNS,
TCP and HTTP). The OS now finds its network card and moves raw Ethernet frames;
it has **no IP address yet** (that is N2/N3), so it cannot reach other machines.

- **PCI** (`pci.rs`): scans the bus (following bridges), reads device IDs,
  classes and base addresses. `lspci` lists the devices.
- **e1000 driver** (`e1000.rs`): Intel 82540EM, the default card in QEMU and
  libvirt (`vm1` has one). Reset, MAC address, receive and transmit descriptor
  rings in heap memory, link status. It is **polled**: the 100 Hz timer wakes
  the idle loop, which takes received frames from the card every 10 ms, so no
  interrupt code changed.
- **`net`** shows the card, MAC address, link and frame counters;
  `net test` sends one harmless broadcast frame (EtherType 0x88B5) to check the
  transmit path. Boot prints `[net]  e1000 (Intel 82540EM) 52:54:00:..., link up`.
- **Random numbers** (`rand.rs`) for later protocol IDs: RDRAND when the CPU has
  it, otherwise a generator seeded from the timestamp counter and the clock.
- A `NetDevice` trait keeps the protocol stack independent of the card.

A QEMU/KVM detail worth knowing: QEMU buffers most e1000 register writes
("coalesced MMIO") until the virtual CPU next exits to QEMU. An idle OS that
only sleeps and polls never does, so the card stopped receiving after one ring
(31 frames) until a key was pressed. The driver now reads a register after
updating the receive tail (a "write flush"), which pushes the writes through.

Tests: `net-card.scn` connects the card to the test runner with `@wire`: it
checks the exact bytes of the frame `net test` sends, injects frames (to our
address, broadcast, and one for another address that must be ignored), sends
more frames than the transmit ring holds, and injects a burst of 80 while the
OS idles (more than the receive ring holds). Self-tests cover PCI decoding,
frame building, descriptor layout and the generator.

## Networking — phase N2: ARP, IPv4, ICMP ✅

The OS now has an IP address and can `ping`, and it answers pings itself.

```
bee:/$ net static 10.0.2.15/24 10.0.2.2
address 10.0.2.15/24, gateway 10.0.2.2 (saved)
bee:/$ ping -c 2 10.0.2.2
PING 10.0.2.2: 32 data bytes
32 bytes from 10.0.2.2: seq=1 ttl=255 time=0.05 ms
32 bytes from 10.0.2.2: seq=2 ttl=255 time=0.05 ms
2 sent, 2 received, 0% loss
```

- **`beenet`** (new crate, like `beefs`): the protocol stack, with no hardware
  access. Frames go in with `receive`, frames to send come out of `transmit`,
  and time is passed in, so it runs in `cargo test` on the host.
  - **ARP** with a cache (32 entries, 60 s lifetime, oldest replaced), three
    tries a second apart, and rules against obvious spoofing (only hosts in
    our subnet; our own address used by someone else is counted as a conflict).
  - **IPv4:** one interface; the local subnet is reached directly, everything
    else through the gateway. Header checksums are verified; fragments and
    packets for other addresses are dropped.
  - **ICMP:** answers echo requests to its own address, sends them, and turns
    "destination unreachable" into a clear message.
  - Every table and queue is bounded, and the parsers are tested with
    truncated and random packets: received bytes are untrusted.
- **Address settings:** `net static IP/PREFIX [GATEWAY]`, `net off`; saved in
  `/system/net` and applied at boot. There is no automatic setup yet (DHCP is N3)
  and no names (DNS is N3): `ping` takes an IPv4 address.
- **`ping [-c N] ADDRESS`** (Ctrl+C stops) measures round trips with the CPU's
  timestamp counter; **`arp`** lists the neighbours it has learned.
- The OS answers pings while it sits at the prompt, because the idle loop
  polls the card.

Tests: 16 host unit tests in `beenet` (`make test`), including two stacks
pinging each other, gateway routing, ARP giving up, spoofed ARP, and broken
packets. `net-ip.scn` checks the exact bytes of the OS's ARP announcement, ARP
reply and echo reply on a test wire. `net-ping.scn` pings QEMU's built-in
gateway (`@net`) and checks that the address survives a reboot.

## Networking — phase N3: UDP, DHCP, DNS ✅

The OS now sets itself up and understands names:

```
[net]  e1000 (Intel 82540EM) 52:54:00:31:2f:d8, link up
[net]  asking the network for an address (DHCP)
...
[net]  address 192.168.122.57/24 (DHCP), gateway 192.168.122.1, DNS 192.168.122.1
bee:/$ ping -c 1 example.com
PING example.com (93.184.216.34): 32 data bytes
```

- **UDP** sockets in `beenet` (8 at most, bounded queues, checksums verified).
- **DHCP client:** at boot, unless other settings are saved, the OS asks the
  network for an address, gateway and DNS server, in the background, so the shell is not
  delayed. The result is shown at the next prompt. It renews the lease half-way
  through, drops the address when a lease runs out, retries after 2, 4 and 8
  seconds, and then keeps trying once a minute (a cable may be plugged in
  later). `net dhcp` switches to automatic setup and waits for the result.
- **DNS resolver:** A records over UDP with a cache (16 names, 5 s to 1 h).
  Questions go out from a random port with a random ID, and an answer is only
  accepted from the server that was asked, on that port, with that ID and the
  same question. `nslookup NAME` (alias `host`); `ping` takes names.
- **Settings** in `/system/net`: `dhcp` (the default when there is no file),
  `none`, or `static IP/PREFIX [GATEWAY]`, plus an optional `dns ADDRESS[:PORT]`
  line. `net dns ADDRESS` sets a DNS server by hand; `net dns auto` uses the
  one from DHCP. `net` shows address, how it was obtained, gateway and DNS.
- **Privacy:** the event log records that the network came up and that
  `ping` / `nslookup` ran, never the names or addresses.

Not done: DNS over TCP, DNSSEC and IPv6.

Tests: `beenet` now has 32 host unit tests: full DHCP handshakes against a
hand-built fake server (retries, refusals, renewal, expiry, odd answers), and
DNS answers including aliases, errors, forged answers and cut-off packets.
`net-dhcp.scn` gets an address from QEMU's DHCP server, resolves names through
a fake DNS server run by the test (`@dns`), and checks that settings survive a
reboot. `net-ip.scn` checks the DHCP DISCOVER frame on the wire.

## Networking — phase N4: TCP ✅

Reliable connections, the basis for HTTP and most other protocols:

```
bee:/$ nc 192.168.122.1 7 hello
connected to 192.168.122.1 port 7
hello
(the other side closed the connection)
```

- **`beenet/src/tcp.rs`:** the three-way handshake, sequence numbers and
  acknowledgements, retransmission with a measured timeout and backoff, fast
  retransmit after three duplicate ACKs, flow control (16 KiB buffers, the
  receive window, zero-window probes), orderly close in both directions
  (including "I'm done sending, keep talking"), and resets.
  - Kept simple: out-of-order segments are dropped and sent again by the peer
    (no reassembly), no selective ACK or window scaling, and a basic congestion
    rule (the window grows by one segment per ACK, collapses on a timeout).
  - Hardened: a reset only counts at exactly the expected sequence number,
    a segment acknowledging data that was never sent is ignored, and a
    segment for no connection is answered with a reset. At most 8 connections.
  - Listening for incoming connections exists in the stack (the tests use it),
    but the OS does not listen on any port: inbound stays closed until N6.
- **`nc HOST PORT`** opens a connection: lines you type are sent, what arrives
  is shown; Ctrl+D ends your side, Ctrl+C quits. **`nc HOST PORT TEXT`** sends
  one line, shows the answer and finishes. Errors are specific: "connection
  refused (nothing is listening on that port)", "connection timed out",
  "host unreachable".
- Connections that are closed finish in the background, so buffered data is
  still delivered after a command ends.

Tests: 16 host tests drive two stacks over a simulated link: one megabyte
through 10% loss with duplication and reordering arrives intact (and a much
worse link still delivers), a perfect link sends every byte exactly once, a
full receiver stalls the sender without errors, forged resets and data are
ignored, a dead peer is detected. `net-tcp.scn` runs `nc` against small
services (`@tcp`): one line each way, 300 lines in order, a refused port, a
typed conversation, half-close with Ctrl+D, and ten connections in a row.

## Networking — phase N5: HTTP, time, file transfer ✅

The commands that make the network useful:

```
bee:/$ wget http://192.168.122.1:8000/notes.txt
(the page, on the screen)
bee:/$ wget http://192.168.122.1:8000/notes.txt notes.txt
saved 1834 bytes to /notes.txt
bee:/$ ntp
clock set to 2026-11-15T10:42:07Z (it was 3 s behind; server stratum 2)
bee:/$ tftp put 192.168.122.1 notes.txt
sent 1834 bytes as notes.txt
```

- **`wget URL`** shows a page (256 KiB at most); **`wget URL FILE`** or
  **`wget URL > FILE`** saves it (8 MiB at most, the BeeFS file limit).
  HTTP/1.1 GET with `Content-Length`, chunked and close-delimited bodies; up
  to 5 redirects are followed; anything but a 2xx answer is an error.
  - **Plain HTTP only.** `https://` addresses, and sites that redirect to
    them (most of the public web), are refused with a clear message. TLS is
    an open decision in the network plan.
- **`ntp [SERVER[:PORT]]`** sets the clock with one SNTP request (default
  server `pool.ntp.org`), corrected for half the round trip. The clock is
  kept in memory: it is not written back to the machine's clock chip, so
  after a reboot the OS starts from the chip's time again.
- **`tftp get HOST[:PORT] FILE [LOCAL]`** and **`tftp put HOST[:PORT] FILE
  [REMOTE]`** copy files from and to a TFTP server (RFC 1350, 512-byte
  blocks, binary mode), for moving files between the OS and the host without
  stopping the VM. TFTP has no passwords: use it on your own network.
- Downloads go to `FILE.part` first; only a complete transfer replaces
  `FILE`, and a failed one leaves nothing behind.
- The protocol logic is in `beenet` (`http.rs`, `sntp.rs`, `tftp.rs`) without
  sockets, so it is tested on the host; the kernel moves the bytes
  (`kernel/src/net.rs`).

Tests: 21 host tests (answers cut at every byte, damaged chunks, an endless
header block, forged or stale time replies, TFTP with lost datagrams in both
directions, strangers and duplicates). `net-http.scn`, `net-time.scn` and
`net-tftp.scn` run the commands against a web server, a time server and a TFTP
server started by the test (`@http`, `@ntp`, `@tftp`); a 200 000-byte file
goes out and back by TFTP unchanged.

## Phase T: cooperative threads ✅

The kernel can now do several things side by side:

```
bee:/$ threads
 ID  NAME       STATE         RUNS  STACK USED
  0  main       running       4211  (boot stack)
  1  net        waiting       4198  3 of 256 KiB
```

- **`kernel/src/thread.rs`:** threads with their own 256 KiB stacks and a
  small context-switch routine. They are *cooperative*: the CPU changes
  thread only where one waits (for a key, the network, a timer) or ends,
  never in the middle of something. That keeps the kernel's unlocked shared
  state correct; the interrupt code is unchanged. When no thread has
  anything to do the CPU sleeps until the next interrupt.
  - Two rules for kernel code: don't keep a reference to shared state across
    a call that waits, and nothing switches while the file system is in use
    (the crash-report guard doubles as the lock).
  - Every stack has a guard value, checked at each switch; a crash in a
    thread other than `main` names the thread.
  - A thread that never waits blocks the others. Up to 16 threads.
- **The network has its own thread** (`net`), so the OS answers pings and
  keeps TCP connections alive whatever the shell is doing, for example
  while `ask` waits for the AI.
- **Console sessions** (`kernel/src/session.rs`): a shell can run in a
  thread with its output and input attached to buffers instead of the screen
  and keyboard. The file system is shared between shells; each shell has its
  own directory, history and error state. Nothing uses sessions yet except
  the self-tests: they are the groundwork for the remote shell in N6.
- `threads` (alias `ps`) lists what runs.

Tests: three kernel self-tests (three threads taking strict turns, sleeping
threads waking in deadline order while `main` keeps running, no switch
while the file system is held, and a second shell that runs commands in its
own session with its own directory). `threads.scn` sends a ping while `ask`
waits for three seconds and requires the reply within one.

## Networking — phase N6 (first part): the status page ✅

```
bee:/$ httpd on
status page on, port 80 (saved)
anyone who can reach this machine can read it, and the files in /public, without a password
```

Then open `http://ADDRESS/` (the address `net` shows) in a browser on the
same network.

- **Off by default.** `httpd on [PORT]` starts it, `httpd off` stops it, and
  the choice is saved in `/system/httpd`. While it is off the OS listens on
  no port at all.
- **What it shows:** `/` is a page with the version, time, uptime, memory,
  storage, A/B kernel slots, threads and the last 15 events (command names
  and outcomes only). `/status.json` gives the main facts to programs.
  `/files/NAME` serves the files in `/public`, and only those: no
  subdirectories, no way to name a file outside it.
- **Read-only, no password, no encryption:** GET and HEAD only. Anyone who
  can reach the machine can read it, so put in `/public` only what may be
  seen. In `vm1`'s default NAT network that means this PC only.
- **Hardened against clients:** a request must arrive within 3 seconds and
  fit in 8 KiB, a stalled download is dropped after 10 seconds, everything
  put into the page is escaped, and client addresses are never logged.
- It runs in a thread of its own (`kernel/src/httpd.rs`) and answers one
  request at a time.

Tests: host tests for the request reader (cut-off, malformed and endless
requests). `net-httpd.scn` reaches the server from the host through a
forwarded port (`@forward`, `@request`): the page and JSON, files, attempts
to escape `/public`, methods other than GET, garbage, a client that never
finishes its request, and the setting surviving a reboot.

## Networking — phase N6 (second part): the remote shell ✅

```
bee:/$ remote on
remote shell on, port 23 (saved)
password: a8fj-zewx-dr6n   (shown only now; 'remote password' makes a new one)
not encrypted: the password and all that is typed can be read on the network; use it on your own network only
```

Then, on another machine in the same network: `telnet ADDRESS` (the address
`net` shows), type the password, and you have a BeeShell of your own next
to the one on the console. `exit` or Ctrl+D ends it.

- **Off by default.** `remote on [PORT]` starts it, `remote off` stops it
  (and ends a running session); the choice is saved in `/system/remote`.
- **Not encrypted.** The password and everything typed cross the network
  as plain text. Use it on a network you trust; in `vm1`'s default NAT
  network only this PC can reach it.
- **The OS makes up the password** (12 letters and digits, about 59 bits),
  so it is never one you also use elsewhere. It is shown once, when it is
  made, and kept in `/system/remote-password`; `remote password` makes a
  new one. Dashes and capitals do not matter when typing it.
- **Guessing is slow:** one client at a time, a 2 second pause after a
  wrong password, three tries per connection, 30 seconds to log in.
- **One session at a time**, closed after 10 minutes without input. When
  the client disappears, the command it was running is stopped.
- **Only the machine's own console** can switch the service on or off or
  make a new password: a remote session can look (`remote`), not change.
- **Visible:** the console reports logins and wrong passwords
  (`[remote] login to the remote shell`), and the event log counts them.
  Client addresses are never logged, and commands are logged by name only,
  as everywhere.
- The remote shell has simple line editing (Backspace, Ctrl+C); the
  history and cursor keys of the console's shell are not there.
- `kernel/src/remote.rs` runs it in two threads: `remote` accepts and
  checks the password, `remote-shell` is the shell, in a console session.
  The Telnet details are in `beenet/src/telnet.rs`.

Tests: host tests for the Telnet decoder (options, cut-off sequences, a
subnegotiation that never ends) and a kernel self-test for the setting,
the made-up passwords and the password check. `net-remote.scn` logs in
from the host through a forwarded port (`@remote`, `@capture`): wrong and
empty passwords, a shell with its own directory next to the local one,
that the password is never echoed, that a remote session cannot change the
service, a client that vanishes in the middle of a command, the setting
and password across a reboot, a new password, and that the log holds no
password and no address. Not yet tried with a real `telnet` program.

Still open in N6: sending signals to beehive over the network, and
downloading a new kernel into the spare slot.

## Versioning

`OS_VERSION` at the top of the `Makefile` is the release, set by hand (now
**0.0.2**). `BUILD_NUMBER` goes up by one on every build that has changed
sources; when you raise `OS_VERSION`, reset `BUILD_NUMBER` to 1. Both show on
the boot banner and with `version`, e.g. `BeeCodersOs 0.0.2 b6`. A/B slots and
backlog items record the build they shipped in.

## Next phases

Following `Docs/self-improving-os-plan.html`: phase 9, AI-written apps in a
WebAssembly sandbox (new commands without touching the kernel). Other
candidates: framebuffer console (on-screen text on UEFI), a text editor, tab
completion, more layouts (Belgian Comma, French, …), physical memory manager /
paging, user programs.
