# BeeCodersOs

A from-scratch operating system for amd64 (x86_64), written in Rust and assembly.

## Phase 1 — Hello World kernel ✅

Boots in QEMU/KVM and shows `Hello World, from BeeCodersOs` on screen (VGA text
mode) and on the serial port (COM1).

### Requirements

- Rust (stable) with the bare-metal target: `rustup target add x86_64-unknown-none`
  (`kernel/rust-toolchain.toml` requests it automatically)
- `qemu-system-x86_64`, `make`, `objcopy` (binutils)
- For the ISO: `limine` and `xorriso` (Arch: `sudo pacman -S --needed limine libisoburn`),
  and `edk2-ovmf` for UEFI testing in QEMU

No NASM or GCC cross-compiler needed: the assembly is built by Rust's LLVM
assembler and linked with the bundled `rust-lld`.

### Build & run

```sh
make             # -> build/beecoders.elf
make run         # QEMU + KVM: VGA window, plus the terminal on this console
make run-nokvm   # pure emulation
make run-serial  # headless, serial only (quit: Ctrl-A then X)
```

### Versioning

The OS version is set by hand as `OS_VERSION` at the top of the `Makefile`.
The build number lives in `BUILD_NUMBER` and goes up by one on every build
that has changed sources. The version shows on the boot banner and with the
`version` shell command, for example `BeeCodersOs 0.0.1 b1`.

### Bootable ISO

```sh
make iso           # -> build/beecoders.iso (hybrid BIOS + UEFI, Limine bootloader)
make run-iso       # boot the ISO in QEMU (BIOS)
make run-iso-uefi  # boot the ISO in QEMU with OVMF (UEFI)
```

Write it to a USB stick for real hardware: `sudo dd if=build/beecoders.iso of=/dev/sdX bs=4M status=progress`
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
tools/beefs-tool/      host CLI to create/inspect/edit BeeFS disk images
tests/boot_test.py     boot tests: drives the kernel in headless QEMU over serial
tests/scenarios/       shell scenario tests (*.scn)
tools/beehive/         beehive: host daemon for the AI bridge (Claude / local models)
tools/bee_report.py    report of what the OS recorded (crashes, missing commands, wishes)
tools/evolve/          improvement loop: signals -> backlog -> coding agent -> review
tools/bootdisk.py      A/B boot disk: FAT16 boot partition (Limine, two kernels) + BeeFS
tools/deploy.py        deploy to the libvirt VM's spare slot, with a watchdog
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
  src/selftest.rs      in-kernel self-tests (`selftest` command / boot flag)
  src/time.rs          PIT timer (IRQ0, 100 Hz) + CMOS real-time clock
  src/eventlog.rs      event log /system/log/events.jsonl
  src/crash.rs         crash reports saved to disk, shown on the next boot
  src/json.rs          minimal JSON (event log, crash reports, AI protocol)
  src/aibridge.rs      AI bridge: JSON lines on COM2 to beehive on the host
  src/bootslots.rs     A/B kernel slots: arm the rollback, health check, report failed trials
  src/allocator.rs     kernel heap in free RAM from the Multiboot memory map
  src/console.rs       print!/println! to VGA + serial, line redraw helpers
  src/keyboard.rs      PS/2 keyboard driver + scancode decoder (modifiers, dead keys)
  src/layouts.rs       keyboard layouts (US, Belgian Period)
  src/serial.rs        16550 UART (COM1) output + interrupt-driven input
  src/input.rs         unified key events, sleeps (hlt) until input arrives
  src/editor.rs        line editor with history
  src/shell.rs         BeeShell commands
  src/storage.rs       mount BeeFS disk or fall back to a RAM disk
  src/ata.rs           ATA/IDE disk driver (PIO, LBA28, polling)
  src/acpi.rs          ACPI tables (FADT/DSDT), power button (SCI), S5 power-off, reset
  src/power.rs         reboot / shutdown (ACPI first, then fallbacks)
  src/vga.rs           80x25 VGA text writer (UTF-8 → code page 437), hardware cursor
  src/ring.rs          lock-free queue for IRQ → main hand-off
  src/port.rs          in/out port I/O
```

### Boot flow

1. QEMU (Multiboot v1 loader) loads the kernel at 1 MiB, enters `_start` in 32-bit protected mode.
2. `boot.s` checks the Multiboot magic, CPUID and long-mode support (prints `ERR: M/C/L` on failure).
3. It identity-maps the first 4 GiB using 2 MiB pages, enables PAE + EFER.LME + paging,
   loads a 64-bit GDT and far-jumps into 64-bit mode.
4. `kernel_main(magic, multiboot_info)` prints the greeting, puts the heap in free RAM,
   sets up interrupts and the keyboard, mounts storage and starts the terminal.

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
`/system/boot_count` and appending to `/system/boot.log`); without one it uses a
4 MiB RAM disk. Disks without BeeFS are **never** formatted automatically.

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
`selftest`, `crash`, `reboot`, `shutdown`. Paths can be relative; `"quotes"` group words.

**Under the hood:** interrupts are enabled (IDT + remapped PIC; IRQ1 keyboard,
IRQ4 serial). Handlers queue raw bytes; the shell decodes them and sleeps with
`hlt` while waiting, so the VM uses no CPU when idle. CPU exceptions print a
register dump instead of silently rebooting.

### Running in virt-manager (libvirt)

Create a VM from `build/beecoders.iso` (OS type: "Generic" / other, BIOS firmware,
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
  restart the VM to pick up the new build.

## Phase 4 — Automated tests ✅

One command checks the whole OS without anyone watching a screen:

```sh
make check    # BeeFS unit tests + kernel self-tests + shell scenarios + ISO boots
```

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
- **Host report:** `make report` (or `tools/bee_report.py --disk build/disk.img --beehive`)
  reads the disk (VM off) and beehive's signals:

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
python3 tools/evolve/evolve.py approve 1 --deploy   # merge, bump the build, rebuild ISO, restart vm1
python3 tools/evolve/evolve.py reject 1 --reason "..." [--retry]
```

1. **Harvest:** wishes, feedback, missing commands and crash reports are read
   from beehive (live, `~/.local/state/beehive`) and/or a BeeFS disk
   (`--disk build/disk.img`). Signals already seen are skipped.
2. **Triage:** Claude Code (no tools, structured JSON output) groups them into
   small, testable backlog items with priorities and acceptance criteria:
   `Docs/backlog/NNN-title.md`. `--triage rules` does it without an LLM.
3. **Implement:** the top open item goes to a Claude Code agent in its own git
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
   with that build, and commits the backlog. `--deploy` restarts `vm1` with
   it. A review that needs attention can only be approved with `--force`.

Tests: `tests/test_evolve.py` runs the whole loop in a throwaway repository
with a fake agent (merging, the gates, refusing to merge into a dirty tree).

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

## Versioning

`OS_VERSION` in the `Makefile` is the release (now **0.0.2**). `BUILD_NUMBER`
counts builds within it; when you raise `OS_VERSION`, reset `BUILD_NUMBER` to 1.

## Next phases

Following `Docs/self-improving-os-plan.html`: phase 9, AI-written apps in a
WebAssembly sandbox (new commands without touching the kernel). Other candidates: framebuffer console (on-screen text on UEFI), a text editor, tab completion, more layouts
(Belgian Comma, French, …), physical memory manager / paging, user programs.
