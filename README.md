# BeeCodersOs

A from-scratch operating system for amd64 (x86_64), written in Rust and
assembly, that helps improve itself. It records what goes wrong and what
people miss, an AI turns that into small tested changes that a person
approves, and each new kernel is tried in a spare slot and rolled back
automatically if it fails.

Current version: **0.0.3** (see [Versioning](#versioning)).

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

The project's web site (os.beecoders.com) is in `website/`.

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
| Drivers for real hardware: todo list with progress | `Docs/drivers-todo.html` |
| Display driver plan (phases G1–G5: framebuffer console, full HD) | `Docs/display-driver-plan.html` |
| Beehive over the network: plan (phases H1–H4) | `Docs/beehive-network-plan.html` |
| Installing on the hard disk: plan (phases I1–I4) | `Docs/install-plan.html` |
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
(or a USB keyboard the firmware presents as one), disks on the legacy IDE
ports or on a SATA (AHCI) controller, and Intel network cards of the e1000,
e1000e (82574L) and I217 kinds; without those it runs from a RAM
disk and without a network. A disk that is not BeeFS is never written to.
A serial port is not needed: the kernel detects COM1 and COM2 and never
waits on one that is missing or stuck. What is still to be written for real
hardware, and how far it is, is in `Docs/drivers-todo.html`.
(double-check `/dev/sdX`!). Limine loads the kernel with the Multiboot 1 protocol
(`boot/limine.conf`).

> UEFI note: UEFI firmware has no VGA text mode. The kernel asks the bootloader
> for a graphics mode and draws its own console in it (the `bootfb` display
> driver, see the display sections below), so UEFI boots have a screen too.
> The boot menu's second entry, "text screen", only works in legacy BIOS boot.

Or by hand: `qemu-system-x86_64 -enable-kvm -kernel build/beecoders.elf`

### Layout

```
boot/limine.conf       Limine boot menu entry (ISO)
beefs/                 BeeFS filesystem library (no_std + alloc, shared by kernel and host)
  src/device.rs        BlockDevice trait + RamDisk
  src/fs.rs            superblock, bitmap, inodes, directories, path ops (+ unit tests)
  src/partition.rs     MBR and GPT partitions: BeeFS inside partition 2 of the boot disk, or in a GPT partition of a real disk
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
tools/beectl.py        run commands on a machine, see its screen, deploy a kernel (through beehive)
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
  src/bootargs.rs      kernel command line (Multiboot cmdline): `test`, `selftest`, `display=`, `mode=` flags
  src/testmode.rs      test mode: pass/fail exit codes for QEMU (isa-debug-exit)
  src/selftest.rs      in-kernel self-tests (`selftest` command / boot flag, health check)
  src/time.rs          PIT timer (IRQ0, 100 Hz) + CMOS real-time clock
  src/eventlog.rs      event log /system/log/events.jsonl
  src/crash.rs         crash reports saved to disk, shown on the next boot
  src/json.rs          minimal JSON (event log, crash reports, AI protocol)
  src/aibridge.rs      AI bridge: JSON lines to beehive on the host, on COM2 or over the network
  src/update.rs        `update`: fetch a kernel the host offers, put it on trial; automatic mode
  src/hivectl.rs       remote control through beehive: run commands, send the screen
  src/bootslots.rs     A/B kernel slots: arm the rollback, health check, report failed trials
  src/pci.rs           PCI: find devices, read their configuration (`lspci`)
  src/disks.rs         the machine's disks (IDE, SATA), each opened once and shared
  src/install.rs       `install`: put the OS on a disk, upgrade it, write boot and os again
  src/fat.rs           read-only FAT16, for the boot partition (boot settings, kernel slots)
  src/bootmods.rs      files the bootloader loaded with the kernel (the install image)
  src/xhci.rs          USB host controller (xHCI): rings, ports, devices; polled by the usb thread
  src/usb.rs           USB descriptors, keyboard and mouse boot protocol (`lsusb`)
  src/e1000.rs         Intel network cards: e1000, e1000e, I217 (descriptor rings; interrupt + poll)
  src/net.rs           card + beenet stack: polling, settings, TCP streams, HTTP / time / TFTP transfers
  src/thread.rs        cooperative kernel threads: stacks, switching, waiting
  src/ahci.rs          SATA disks on AHCI controllers (real hardware; QEMU `-device ahci`)
  src/httpd.rs         status page for the local network (off by default)
  src/remote.rs        remote shell with a password, Telnet style (off by default)
  src/session.rs       console sessions: a shell that is not on the real screen and keyboard
  src/rand.rs          random numbers (RDRAND, or a seeded generator)
  src/allocator.rs     kernel heap in free RAM from the Multiboot memory map
  src/console.rs       print!/println! to the screen + serial, line redraw helpers
  src/display.rs       display drivers: the interface, the list, `display=` / `mode=` flags
  src/bochs.rs         display driver for the Bochs/QEMU graphics card (sets any resolution)
  src/bootfb.rs        display driver for the framebuffer the bootloader set up (any card, fixed mode)
  src/fbcon.rs         framebuffer console: text drawn as pixels, for any resolution
  src/font.rs          the 8x16 console font (Spleen, BSD license), generated table
  src/paging.rs        page table changes after boot: write-combining for the framebuffer
  src/gfx.rs           drawing on a framebuffer: rectangles and characters, always clipped
  src/wm.rs            window system: desktop background, window frames, order and focus
  src/desk.rs          the extra windows: their shells and programs, keys for the focused window
  src/cpu.rs           the processor: name, features, how many there are (`cpu`)
  src/textedit.rs      the text editor (`edit`)
  src/filemgr.rs       the file manager (`files`)
  src/icons.rs         its icon view: grid, drawn icons, picture thumbnails
  src/web.rs           the web page viewer (`browse`); beenet/src/html.rs lays pages out
  src/keyboard.rs      PS/2 keyboard driver + scancode decoder (modifiers, dead keys)
  src/layouts.rs       keyboard layouts (US, Belgian Period)
  src/uart.rs          16550 serial port: detection, sending and receiving with limits
  src/serial.rs        serial console on COM1: output + interrupt-driven input
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

**Writing speed:** during one operation BeeFS keeps the bookkeeping blocks
it works on (bitmap, pointer tables) in memory and writes each once: before
any inode is written (so an inode never points at tables not yet on disk)
and when the operation ends. The search for a free block starts where the
last one was found, and a data block that a write fills completely is not
zeroed first. Writing a 1.4 MB file went from 11,448 block writes and 8,327
reads to 3,000 writes and 37 reads (`large_writes_do_little_bookkeeping`
checks it); on the test VM's IDE disk from more than 10 s to 4 s.

At boot the kernel mounts the first disk containing BeeFS (incrementing
`/system/boot_count` and appending to `/system/boot.log`): a disk on the
legacy IDE ports, or a SATA disk on an AHCI controller (`kernel/src/ahci.rs`,
for real hardware). On a partitioned boot disk (phase 8) it uses the BeeFS
partition; on a disk with a GPT, the first partition of type
`FE35CA6A-9309-4008-996D-254793C95A38` (the table must have correct
checksums). Without one it uses a 4 MiB RAM disk. Disks without BeeFS are only
read and **never** formatted or written. Crash reports and the A/B kernel
slots still work on IDE disks only.

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

**Commands:** `help`, `whoami`, `users`, `useradd`, `userdel`, `passwd`, `login`, `logout`, `clear`, `echo` (with `>` / `>>` redirection), `ls`, `cd`, `pwd`,
`cat`, `head`, `tail`, `wc`, `grep`, `touch`, `mkdir`, `rm`, `mv`, `cp`, `tree`, `df`, `layout`, `history`,
`mem`, `about`, `calc`, `date`, `timezone`, `uptime`, `log`, `wish`, `feedback`, `ask`, `explain`, `do`, `ai`,
`slots`, `install`, `update`, `display`, `desktop`, `terminal`, `edit`, `files`, `browse`, `sleep`, `cpu`, `monitor`, `view`, `screenshot`, `record`, `sound`, `beep`, `play`, `tune`, `lsusb`, `ai connect`, `net`, `ping`, `nslookup`, `nc`, `wget`, `ntp`, `tftp`, `httpd`, `remote`, `vnc`, `arp`, `lspci [-v]`, `threads`, `selftest`, `crash`, `reboot`, `shutdown`. `help` in the OS describes each one. Paths can be relative; `"quotes"` group words.

**Text tools** (`kernel/src/textops.rs`, self-tested; `text-tools.scn`):
`head` / `tail [-n N | -N] FILE` show the first or last lines (10 unless N
is given), `wc FILE` counts lines, words and bytes, and `grep [-i] TEXT
FILE|FOLDER` shows the lines that contain TEXT with their numbers; on a
folder it searches every file in it and below (`path:line: text`, at most
1000 lines; a binary file only says that it matches). `-i`: upper and
lower case alike.

**Banner:** the text of `/system/banner`, if it exists, is shown at boot
just before "Type 'help'" (the first 20 lines, control characters left
out): `echo Welcome > /system/banner`, or write it with `edit`. Asked for
with `wish`; `banner.scn` tests it.

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
  when no filesystem operation and no disk command was in progress, so a crash
  can't corrupt files. It goes to the disk the system runs from (IDE or SATA).
  After a panic or CPU exception the machine halts with the message on the screen, except in
  two cases where it restarts by itself 10 seconds later: a kernel on trial
  (the good slot then boots: a rollback without anyone at the machine), and a
  machine driven from the host (`ai control on` or `update auto on`) that had
  been running for at least a minute.
  `crash` crashes the kernel on purpose with a panic, `crash exception` with
  a CPU exception (to test this).
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
  previous boot. **`do TASK`** asks the AI to turn a task in plain words
  into shell commands (`do "make a folder photos with a readme in it"`;
  quotes keep `>` inside the task). They are listed, and run one after the
  other only after **y**; the first that fails stops the rest. A plan never
  runs `install`, `update`, `slots`, `reboot`, `shutdown`, `crash`, `ai`,
  `remote`, `httpd`, `net`, another `do`/`ask`/`explain`, or a command that
  changes a saved setting (`desktop off`, `cpu auto`...), even with y
  (`kernel/src/doplan.rs`; the answer is untrusted: at most 10 commands of
  200 characters, no control characters). Like `ask`, only the outcome is
  logged, never the task or the plan; tested by a self-test and
  `ai-do.scn`. Keys typed while the AI answers are kept for what comes
  next (`input::unread`), so a `y` typed early is not lost.
  `ai` shows the bridge status; `ai off` / `ai on` is the kill
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
  `hello`/`welcome`, `ask`/`explain`/`do` -> `ack`, `line`..., `done` (or `error`),
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
  classes and base addresses. `lspci` lists the devices; `lspci -v` adds each
  device's capability list (MSI, MSI-X, PCI Express, power management).
- **e1000 driver** (`e1000.rs`): Intel 82540EM, the default card in QEMU and
  libvirt (`vm1` has one). Reset, MAC address, receive and transmit descriptor
  rings in heap memory, link status. The card **interrupts** when a frame
  arrives (old interrupt controller, the line the firmware assigned), and is
  also polled every 10 ms by the 100 Hz timer, so nothing depends on the
  interrupt. A line that keeps firing for another device is switched off
  after 5000 unclaimed interrupts in a row; the boot flag `net=poll` turns
  the interrupt off. `net` says which is in use (`receive:`).
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

Still open in N6: downloading a new kernel into the spare slot (phase H4 of
`Docs/beehive-network-plan.html`, not built). Signals to beehive over the
network: see the next section.

## Beehive over the network (phases H1–H3) ✅ in QEMU, not yet tried on the HP

```
host$ tools/beehive/beehive.py pair hp
hp is paired. On that machine, type:

    ai connect HOST:4560 hp k7m2q-x9fpa-3hbdw-e5tzn

host$ make beehive BEEHIVE_ARGS="--listen 192.168.1.10:4560"

bee:/$ ai connect 192.168.1.10 hp k7m2q-x9fpa-3hbdw-e5tzn
connecting to beehive as machine 'hp' (saved)
not encrypted: people on your network can read questions and answers, but not use or change them
connected (anthropic claude-opus-5-5)
```

A machine without the AI serial port (any real one) reaches beehive over the
local network. `ask`, `explain`, `wish`, crash and health reports work as they
do over the serial port. Plan: `Docs/beehive-network-plan.html`.

- **Off by default on both sides.** beehive listens only with
  `--listen ADDRESS[:PORT]`; the OS connects only after `ai connect`. The OS
  connects out: nothing new listens on the OS.
- **Pairing.** `beehive.py pair NAME` makes a key for one machine and prints
  the command to type on it. `beehive.py unpair NAME` takes it away,
  `beehive.py machines` lists them. Keys are in beehive's state folder
  (`machines.json`, readable by you only) and in `/system/hive` on the machine.
- **The key is never sent.** Each side proves it knows the key with a keyed
  hash (HMAC-SHA-256) over two fresh random numbers, and every line after
  that is sealed the same way with its place in the row. Someone else on the
  network cannot use beehive, pretend to be it, or change, repeat or drop a
  message unnoticed. The code is `beenet/src/hive.rs` and `sha256.rs` (tested
  on the host against the published values) and the same in `beehive.py`.
- **Not encrypted.** People on the same network can read questions, answers
  and wishes. Do not type secrets into `ask`.
- **beehive's door:** at most 8 connections, 10 s to finish the proof, short
  lines only before it, five wrong tries lock an address out for a minute,
  and unknown names look exactly like wrong keys. Each machine has its own
  daily share of the request budget (`--max-requests-per-machine`).
- **The `hive` thread** in the OS keeps the connection: it reconnects when
  beehive restarts or the network comes back (waiting 2 s, doubling up to a
  minute), asks "still there?" every 30 s, and delivers wishes that were
  filed while beehive was away. An `ask` without a connection says so at once.
- **`ai`** shows the link; **`ai off`** closes it and keeps it closed;
  **`ai disconnect`** removes it and its key. `ai connect` and `ai disconnect`
  work from the machine's own console only.
- **Privacy:** the event log records "hive connected / lost", never the
  address or the key.
- When a network link is set up it is used instead of COM2.
- **Tests:** `tests/test_beehive.py` (the door: pairing, wrong keys, changed,
  repeated and unsealed lines, lockout, budget), unit tests in `beenet`,
  scenarios `ai-network*.scn` (`@ai-net`, `@hive stop|start`): the whole path
  in QEMU, a beehive restart, and a reboot of the OS.
- **Not done:** the link's state on the status page (that page is protected);
  a new kernel over the link (H4). On the HP the key is lost at every restart
  until BeeFS lives on a disk there (it runs from a RAM disk).

## Accounts: `useradd`, `login`, `whoami` ✅ in QEMU

A machine without accounts works as it always did: no login, everyone may
do everything. `useradd NAME [admin]` makes an account (the password is
typed twice and never shown); the first one is always an admin, and the
console is logged in as it so nobody is locked out. From then on the
console asks for a name and password at boot and after `logout`, and
commands that change the system itself need the group `admin`
(`users::needs_admin`: `install`, `update`, `reboot`, `shutdown`,
`useradd`, `userdel`, switching `vnc`, `remote`, `httpd`, `net`, `ai`,
`desktop`, `display`, `cpu`, `layout`, `timezone`, someone else's
`passwd`); showing their state is for everyone. `whoami`, `users`,
`passwd [NAME]`, `userdel NAME` (never the last admin). Accounts are lines
in `/system/users` (`name:groups:salt:hash`): no password is stored, only
SHA-256 run 20,000 times over a fresh salt and the password; damaged lines
give nobody anything. Logins are logged as "ok" or "wrong", never with a
name. Terminal windows run as whoever is logged in at the console; the AI
host's control shell keeps admin rights (`ai control on` at the console
allowed it); the remote shell keeps its own password and has no admin
rights once there are accounts.

**Who may change which files** follows the folder (`users::owner_blocking`):
your home folder `/home/NAME` is yours, `/system` belongs to the admins,
everything else is shared; admins may change everything, and anyone may
read. It is checked by every shell command that writes (`echo >`, `cp`,
`mv`, `rm`, `mkdir`, `touch`, `wget`, `tftp get`, `screenshot`) and by the
editor and the file manager, which act for whoever is logged in at the
console: "PATH: belongs to NAME (anyone may read it, only they may change
it)". Owners per file would need room in BeeFS's inodes (its on-disk
format is protected). Tests: a self-test (the file, passwords, what needs
an admin, who may change what) and `users.scn` (first account, an ordinary
one, refusals, files in homes and /system, logging out and in, a wrong
password).

## Remote desktop: `vnc` ✅ in QEMU

```
bee:/$ vnc on
remote desktop (VNC) on, port 5900 (saved)
password: 4fqfqhgu   (shown only now; 'vnc password' makes a new one)
not encrypted: the screen and all that is typed can be seen on the network; use it on your own network only
```

A VNC server (`kernel/src/vnc.rs`, protocol in `beenet/src/rfb.rs`): any
VNC viewer (TigerVNC, Remmina, `gvncviewer`, macOS Screen Sharing) shows
the screen and works the keyboard and mouse. Off by default; `vnc on
[PORT]`, `vnc off` and `vnc password` work only on the machine's own
console (a remote session or the AI host can't switch it on), like the
remote shell. The password is made up by the OS (8 characters, the most
VNC Authentication uses) and never crosses the network: VNC Authentication
answers a random challenge with DES (checked against openssl's answers).
One viewer at a time, a 2 s pause after a wrong password, 30 s to log in,
30 minutes without input ends a session; logins are counted (`vnc`),
addresses never logged. **Not encrypted**: what is on the screen and what
is typed can be seen on the network.

The viewer gets the screen as drawn in memory, read on every processor,
in Raw rectangles of the 64x64 tiles that changed since its last update
(at most 20 updates a second), in any true-color format of 8, 16 or 32
bits it asks for. Its keys are typed as on this machine's keyboard
(`keyboard::push_key`; no layout applies, the viewer sends characters;
Control+letter, Alt+Tab), its mouse moves the desktop's pointer. Protocol
versions 3.3, 3.7 and 3.8. Tests: `rfb` unit tests (DES known answers,
messages in pieces, a 4 GB clipboard and 65,535 encodings bounded,
pixel formats, keysyms), a self-test (keys, changed tiles) and `vnc.scn`
(switching, saving, the RFB greeting on a real connection). End to end, a
viewer logged in to a QEMU VM, got the 1920x1080 screen, and typed a
command that the console ran.

## Driven from the host: kernels over the link, remote control ✅ in QEMU

```
host$ tools/beectl.py deploy hp
kernel b52 is on offer (1078 KiB): machines get it with `update`, ...
  fetching kernel b52 (1078 KiB)
  writing the new kernel (b52) into slot b
kernel b52 is in slot b, on trial at the next start: reboot
hp: b52 passed its health check and is now the good slot

host$ tools/beectl.py run hp "df"
host$ tools/beectl.py screenshot hp screen.ppm
```

What lets a new build reach a machine nobody sits at, and lets the developer
(or an AI working for them) see what it does there. Both go through beehive,
over the link the machine already has; both are off until switched on at the
machine's own console.

- **`update`** asks beehive for the kernel it has on offer
  (`beehive.py offer`), downloads it, checks its SHA-256, writes it into the
  spare kernel slot and marks it on trial. The A/B rollback decides at the
  next boot, and the machine reports "healthy" or "failed" back to beehive.
  A build that failed on a machine is not fetched there again.
- **`update auto on`** (saved): the machine asks every minute by itself and
  restarts to try what it fetched.
- **`ai control on`** (saved): beehive may run shell commands on the machine
  and fetch its screen, as text or as a picture (`tools/beectl.py run`,
  `text`, `screenshot`). The commands run in a shell of their own, like a
  remote session: what only the console may do (`install`, `ai connect`,
  these switches) stays refused.
- **Typing from the host:** `tools/beectl.py keys MACHINE "TEXT"` types in
  the window that has the focus, as on the keyboard: into another terminal
  window or the editor. Keys by name go in braces: `{enter}`, `{ctrl+s}`,
  `{alt+tab}`, `{up}` ... The console's own window takes no keys from afar,
  so what only the console may do stays out of reach this way too. With
  `beectl.py text`, which shows the text of every window, a program in a
  window can be driven and checked from the host.
- **`tools/beectl.py deploy MACHINE`** does a whole deploy: offer, tell the
  machine to update and restart, wait for its report.
- **`ai off`** stops all of it.
- **Trust:** whoever is at the other end of the link can do this. Over the
  network that is the beehive the machine was paired with (every line is
  sealed with the pairing key); there is no further signature on a kernel.
- Over a VM's serial link a kernel takes most of a minute (the port's speed);
  over the network a few seconds.
- **Tests:** scenarios `update.scn`, `update-auto.scn` (boot disk, network
  link: fetch, trial, health report; the machine restarting by itself) and
  `remote-control.scn`; `tests/test_beehive.py`; self-tests.

## Display — phase G1: a graphics console, full HD in QEMU ✅

```
[disp] driver: bochs, mode: 1920x1080, 32 bits per pixel, text: 240x67 characters (8x16 font)
bee:/$ display mode 1280x720
driver: bochs
mode: 1280x720, 32 bits per pixel
text: 160x45 characters (8x16 font)
```

The first phase of `Docs/display-driver-plan.html`. The console no longer needs
VGA text mode: with a display driver it draws its own letters on a graphics
framebuffer, at any resolution.

- **Display drivers** (`kernel/src/display.rs`): a driver finds its hardware,
  sets a mode and hands out a framebuffer. Drivers are tried in order; a new
  one is a file plus one line in `DRIVERS`. When none fits the machine, the
  80x25 VGA text screen stays, as before.
- **`bochs` driver** (`kernel/src/bochs.rs`): the graphics card of QEMU
  (`-vga std`, the default), also QXL (`vm1`), virtio-vga and VirtualBox. It
  sets 1920x1080 at boot, or what fits the card's video memory.
- **Framebuffer console** (`kernel/src/fbcon.rs`): keeps every character cell in
  RAM, draws only the cells that change, and never reads video memory. Same
  colors and characters as the text screen (code page 437), drawn with the
  Spleen 8x16 font (`kernel/src/font.rs`). No heap, no waiting: it works
  during a panic too.
- **`display`** shows driver, resolution and size in characters;
  **`display mode WIDTHxHEIGHT`** changes the resolution (the text on screen
  stays);
  **`display test`** shows color bars until Enter, then tells how long drawing
  the whole screen took; **`display speed`** measures the same without
  showing bars or waiting for a key (so it can be asked through `beectl.py`).
- **Boot flags:** `display=text` keeps the text screen (the way back if graphics
  go wrong), `display=bochs` insists on a driver, `mode=1280x720` picks the
  resolution.
- **Tests:** self-tests draw into a framebuffer in RAM (every pixel format, odd
  sizes, bad values). Scenarios `display*.scn` take real screenshots through
  QEMU (`@screenshot`), on the standard card, on QXL, without a supported card
  (`@vga`) and with `display=text` (`@args`).


## Display — phase G2: the bootloader's framebuffer (real hardware, UEFI) ✅

```
[disp] driver: bootfb, mode: 1024x768, 24 bits per pixel, text: 114x39 characters (8x16 font), desktop: on
```

Graphics on machines we have no graphics-card driver for, which is every real
one. The bootloader sets the mode and the kernel draws in it.

- **The request** (`kernel/src/boot.s`): the Multiboot header asks for
  1920x1080 with 32 bits per pixel. Limine gets it from the firmware (VBE in
  legacy BIOS boot, GOP in UEFI boot), or the nearest mode the firmware has.
- **`bootfb` driver** (`kernel/src/bootfb.rs`): reads where the framebuffer is
  from the Multiboot info and checks it before use: a pixel format we can
  draw (32, 24 or 16 bits), lines that hold their pixels, everything below
  4 GiB. If it fails a check, the boot log says what the bootloader offered.
  The mode is fixed: `display mode` says so.
- **Order:** `bochs` first (it can change the mode), then `bootfb`, then the
  text screen. `display=bootfb` insists on the bootloader's mode.
- **First boot lines:** in a graphics mode there is no text screen, so what
  is printed before the driver starts is kept and shown once it has.
- **Way back:** the ISO's boot menu now waits 3 seconds and has a second
  entry, "text screen", for the 80x25 screen (legacy BIOS boot only). Under
  UEFI `display=text` cannot work; the kernel says so and uses graphics.
- **Tests:** a self-test feeds the parser good, wrong, cut-off and random
  bootloader data. `make check` boots the ISO two more times with a graphics
  card only the bootloader can set up (BIOS: 1024x768 with 24 bits; UEFI:
  800x600) and checks the desktop on a screenshot.
- QEMU's own `-kernel` loader cannot set a mode and prints
  `multiboot knows VBE. we don't` at every start: harmless, the `bochs`
  driver takes over there.

Seen working on the HP (EliteDesk 800 G1): it boots to the desktop at the
monitor's own resolution. Not done: a framebuffer above 4 GiB (reported, not
used), and the A/B boot disk has no "text screen" menu entry.

## Display — phase G3: larger font, saved settings, fast drawing ✅

```
bee:/$ display font 2
driver: bochs
mode: 1920x1080, 32 bits per pixel
text: 110x28 characters (16x32 font)
desktop: on
video memory: write-combining
(saved)
```

- **`display font 1|2|auto`**: 2 draws every character at double size (16x32
  pixels). `auto`, the default, picks double on screens higher than full HD.
- **Saved:** `display mode` and `display font` write `/system/display`
  (lines `mode WIDTHxHEIGHT` and `font 1|2|auto`) and are applied at the next
  boot. A `mode=` boot flag wins over the saved resolution; the `bootfb`
  driver keeps the bootloader's mode.
- **Write-combining** (`kernel/src/paging.rs`): the framebuffer's pages are
  marked so the CPU sends pixel writes to the graphics card in bursts instead
  of one by one. On real hardware that makes drawing many times faster; in a
  VM it changes nothing. `display` shows `video memory: write-combining`, or
  `plain` and why. The boot flag `wc=off` switches it off.
- **Measuring:** `display test` and `display speed` print how long drawing the whole screen takes; `display speed` also shows the part spent copying to video memory.
- **The screen in memory:** everything is drawn on a copy of the screen in
  normal memory, and only the parts that changed are copied to the video
  memory (`console.rs`). A prompt, typed text or a word without a line end
  appears at once; whole lines from a running command go to the screen in
  batches, at most every 20 ms (the timer sees to the last ones), so a long
  listing scrolls in jumps instead of drawing every line. What the mouse
  pointer covered comes back from the copy. A crashing kernel shows every
  change at once. The kernel heap may now take up to 1 GiB of the free RAM
  (it was 64 MiB): the copy needs 8 MiB at full HD. Lines-at-a-time drawing
  of rectangles and characters (`gfx.rs`) took the HP from 16.1 ms to 9.4 ms
  for a whole 1280x1024 screen.

## Graphical desktop: terminals in windows ✅

```
bee:/$ desktop
desktop: on, the terminal window has 221x56 characters ('desktop off' ends it)
bee:/$ desktop off
desktop off (saved)
```

With a graphics mode (see the display section above) the OS starts as a
desktop: a light blue background and one window, the terminal. `desktop off`
gives the terminal the whole screen again, `desktop on` brings the desktop
back; the text in the terminal stays. The choice is saved in
`/system/desktop` and used at the next boot. On the text screen there is no
desktop.

- **Drawing** (`kernel/src/gfx.rs`): filled rectangles and characters on the
  framebuffer. Every position is checked, so nothing is written outside it.
- **Window system** (`kernel/src/wm.rs`): a desktop holds windows, back to
  front; each has a border, a title bar with its title, and a content area its
  owner draws in. The top window has the focus (a blue title bar; others gray).
- **Terminal:** the console is the first window; everything in it works as
  before: colors, scrolling, line editing.
- **More windows:** `terminal` opens one more terminal window (80x25
  characters, at most four) with a shell of its own (`kernel/src/desk.rs`; a
  simple one for now: no colors, history or cursor keys). Windows may
  overlap. Each window's owner draws on a picture in memory (`gfx::Surface`)
  and the desktop is put together from those, back to front. The window on
  top has the focus and gets the keyboard; **Alt+Tab** gives it to the next
  window, a **click** on a window brings it to the top. `exit` in the window
  or a click on its **close box** ends its shell and the window. The
  console's own window cannot be closed. Changing the resolution or font, or
  `desktop off`, closes the extra windows. What the serial port types always
  goes to the console.
- **Text editor:** `edit FILE` opens a file (or a new one) in a window of
  its own (`kernel/src/textedit.rs`). Type to insert; arrows, Home, End,
  Page Up and Page Down move; Enter keeps the indent of the line; Tab goes to
  the next multiple of four; Ctrl+K cuts the rest of the line. **Ctrl+S**
  saves, **Ctrl+Q** leaves (asked twice when changes are not saved). The
  status line at the top shows the name, the place of the cursor and what
  just happened. Long lines scroll sideways. Text files up to 1 MiB; a file
  is written back with a line end after every line. Closing the window
  with its close box leaves without saving. An editor counts as one of the
  four extra windows.
- **File manager:** `files [FOLDER]` lists a folder in a window of its own
  (`kernel/src/filemgr.rs`): folders first, then files with their size.
  Arrows, Page Up/Down, Home and End move the bar; **Enter** opens a folder,
  a picture (`.png`, `.gif`, `.bmp`, `.ppm`) in the viewer, a `.wav` file on
  the sound card (on a thread of its own, so the window goes on), and any
  other file in the text editor (a new window); **e** opens any file in the
  editor; **Backspace** goes up, with
  the bar on the folder just left. **n** makes a folder and **r** renames
  (the name is typed in the status line; Escape drops it), **d** or Delete
  deletes after a `y` (a folder only when it is empty), **u** reads the
  folder again, **q** leaves. A **click** on an entry puts the bar there, a
  second click opens it. **v** switches to the icon view
  (`kernel/src/icons.rs`): a grid drawn in the window's pixels, with icons
  for folders, text, sounds and pictures, and a real thumbnail (at most
  64x64, each pixel the average of those it covers) for every picture up to
  2 megapixels and 8 MB; the files are read one by one and then decoded and
  shrunk on every processor at once. Arrows move in the grid, a click
  chooses and a second click opens; `v` goes back to the list
  (`console::pane_draw` draws pixels, `pane_show` brings the text back).
  `desktop-icons.scn` checks the icons' pixels at 800x600.
- **Reading pages like a browser shows them:** a page that marks its
  content with `<main>` (Wikipedia does) is shown from there: menus,
  headers and footers around it are left out, and `m` switches to the whole
  page and back (`html::render_with`'s `main_only`, `Page::main_only`).
  Elements a browser would not show are left out with all they hold
  (`Tag::hidden`): `hidden`, `aria-hidden="true"`, `display: none`, and the
  usual class names of site controls (drop-down menus, `noprint`,
  `mw-editsection`, `navbox`, text only for screen readers). A tag's end is
  the first `>` outside a quoted attribute value (`tag_end`), so attributes
  that hold `>` (Wikipedia's `data-mw`) no longer spill onto the page.
  Wikipedia's article on honey bees now starts on the first screen, with
  its photo on the second; before, menus and 98 language names came first.
  DuckDuckGo and Mojeek were tried for searching over HTTPS: both answer
  automated clients with a challenge, so searching stays with FrogFind.
- **Pictures in web pages:** after a page is shown, `browse` loads its
  `<img>` pictures (up to 16; from the web, also over HTTPS, or from the
  disk for a page on the disk), in the formats the OS reads (JPEG, PNG,
  GIF, BMP; SVG and WebP are skipped, as are pictures over 16 megapixels),
  shrinks each to fit the window (at most 320 pixels high, each pixel the
  average of those it covers: `image::shrunk`) and lays the page out again
  with empty lines kept for it (`html::render_with`, `Page::images`); the
  pictures are drawn into those lines after the text (`console::pane_draw`),
  cut off at the status and help lines while scrolling. A picture that
  could not be loaded keeps its alt text. `web-pictures.scn` checks the
  pixels of a picture and the alt text of a missing one.
- **Web page viewer:** `browse URL|FILE|WORDS` shows a page in a window of its
  own (`kernel/src/web.rs`): an `http://` page from the web (the `http://`
  may be left out) or an HTML or text file from the disk. The page is shown
  as text, laid out for the window's width by `beenet/src/html.rs`
  (headings, paragraphs, lists, preformatted blocks, links; no styles,
  scripts or pictures, an image shows its `alt` text). Arrows, Page Up/Down,
  Space, Home and End scroll; **Tab** picks the next link (it is scrolled
  into view), **Enter** or a **click** follows it, **Backspace** goes back,
  **g** asks for an address, **r** loads the page again, **q** leaves. Links
  of a page on the disk lead to files on the disk. `https://` pages cannot
  be opened directly yet (no TLS); pages are at most 1 MiB.
  **Searching:** words instead of an address (`browse honey bees`, or
  after **g**: with a space, or without a dot) search on FrogFind
  (frogfind.com), a search engine for simple browsers. Choosing an
  `https://` link says it cannot be opened; **f** then reads it through
  FrogFind's reader, which hands the page out as plain HTML over http (a
  typed `https://` address goes there too). Both are the user's choice:
  FrogFind sees what is searched for and which pages are read through it.
  On the HP this finds things and shows Wikipedia articles. A page is untrusted data:
  the layout is bounded (20,000 lines, 2,000 links) and has host tests with
  input cut off anywhere. Tests: `html` unit tests in `beenet`, a self-test
  for keys, links, history and addresses, and `browse.scn` (files, links,
  back, a page from the test web server). On the HP it shows
  http://info.cern.ch, the first web site, and follows its links.
- **Pictures:** `screenshot FILE` saves the screen (as drawn in memory)
  as a PNG if FILE ends in `.png` (RGB, compressed on every processor:
  each line gets the best of the five PNG filters, and 16 pieces of lines
  are compressed at once by `beenet/src/deflate.rs`, LZ77 with the fixed
  Huffman codes, each piece ending on a whole byte so they join into one
  stream; on the HP a 1280x1024 screen is 53 KiB in 30 ms, against 3.8 MB
  and 220 ms as BMP; host tests check every way of cutting data into
  pieces, and ImageMagick and Python's zlib read the files), otherwise
  as a 24-bit BMP; `view FILE` shows a PNG, BMP (uncompressed, 24 or 32
  bits) or binary PPM (P6) picture in a window (`kernel/src/view.rs`), as large
  as fits and at most twice its size, in the middle on a dark background.
  The lines are scaled on every processor in use (`console::pane_picture`);
  a window that shows a picture is not covered again by its text. Files
  are untrusted: `kernel/src/image.rs` checks every size, refuses
  pictures over 4096 x 4096 pixels, and has a self-test with files cut
  off at every byte. PNG is read by `beenet/src/png.rs` with its own
  DEFLATE decompressor (`beenet/src/inflate.rs`): every color type, bit
  depths 1-16, all five filters, Adam7 interlacing, palette transparency
  (transparent pixels are blended onto black). Host tests compare every
  color type with ImageMagick's pixels, and feed it files cut off and
  flipped at every byte; the PNGs it writes are read by ImageMagick. A
  1280x1024 wallpaper is 22 KB as PNG instead of 3.9 MB as BMP. `pictures.scn` saves the screen at 800x600, views it
  on a 4-processor VM and checks the pixels of the window.
- **JPEG photos:** `view`, the file manager's thumbnails and the wallpaper
  also read JPEG (`beenet/src/jpeg.rs`): baseline and progressive JPEG
  with Huffman coding, restart markers, grayscale or YCbCr in 4:4:4, 4:2:2 or 4:2:0
  (colors from the nearest chroma sample), up to 8192 pixels a side and 24
  megapixels. Decoding has two halves: the coefficients are read in one
  pass, then each row of 8x8 (or 16x16) blocks is turned into pixels
  (an inverse DCT in whole numbers, its cosine table computed beforehand,
  and the color conversion) on whichever processor is free
  (`image::decode_jpeg`). A progressive JPEG comes in several scans (often
  ten): DC first, then bands of AC coefficients, each refined bit by bit
  later, single components in their own block order, runs of blocks with
  nothing more to add (EOB runs); every scan adds to the same coefficients,
  and the pixels are made once at the end. Lossless, arithmetic-coded and
  CMYK JPEGs are refused with a clear message. Host tests compare ImageMagick-made JPEGs in every layout
  (also with restart markers, and a 160x120 photo; and progressive ones,
  with restart markers and gray too) with libjpeg's own
  pixels: never more than 4 apart; cut and damaged files fail cleanly; the
  self-test checks that the processors together give exactly the pixels of
  the one-by-one decoder. On the HP a 1923x1599 photo of a honey bee
  (2.4 MB, fetched from Wikimedia over HTTPS) is read and shown in 0.44 s,
  and a 1200x800 progressive photo of honey from Unsplash in 40 ms.
- **Video:** `play FILE.avi` (and Enter on an `.avi` in the file manager)
  plays Motion-JPEG AVI videos in a window (`kernel/src/video.rs`):
  `beenet/src/avi.rs` reads the RIFF headers (size, time per frame, the
  video's compression, PCM sound) and where each frame is; each frame is a
  JPEG decoded on every processor and shown at its time, frames that would
  come too late are skipped, and the sound plays on the sound card on a
  thread of its own (space pauses both, `q` stops both). Host tests read an
  ffmpeg-made clip (`beenet/testdata/clip.avi`: 10 frames, 440 Hz), cut and
  damaged files fail cleanly. On the HP a 480x320 video of honey with a
  melody (`https://os.beecoders.com/media/honey.avi`, 3.2 MB, made with
  ffmpeg from an Unsplash photo) plays at its 24 frames a second with sound.
  The first version froze the HP: after the last frame the player waited
  for a time already past, which returns at once, so it never let the other
  threads run (threads switch only when one waits); now it waits only for a
  key after the end. QEMU showed the same freeze with the old line and none
  with the fix.
- **GIF and screen recordings:** `view` also shows GIF pictures and plays
  animated GIFs at the speed the file asks for, round and round (space
  pauses, `n` steps a frame, `q` leaves). `beenet/src/gif.rs` reads
  GIF87a/89a (LZW, global and local color tables, transparency,
  interlacing, all three ways of clearing a frame); frames stay as the
  rectangles of color indices the file holds, and a `Player` paints them
  one by one, so a long animation costs about a byte per changed pixel.
  `screenshot FILE.gif` saves one picture, and `record FILE.gif SECONDS`
  (1-60, Ctrl+C stops early) films the screen at five pictures a second:
  each picture is turned into a fixed 252-color palette on every processor
  in use, only the rectangle that changed since the picture before is
  stored, and a picture without changes makes the one before last longer.
  On the HP, 5 s of the 1280x1024 desktop with the monitor open is 35 KB
  (11 different pictures out of 25). Host tests compare every frame of
  ImageMagick-made GIFs (offsets, transparency, every disposal, interlaced)
  with ImageMagick's own `-coalesce`, round-trip LZW around every code
  width change and table reset, and feed cut and flipped files;
  `pictures.scn` records the screen with the monitor open and plays it.
- **Sound (HD Audio):** `sound` shows the sound card, its codecs, every
  pin and the way found from a converter (DAC) to each speaker, headphone
  and line-out pin; `sound volume 0-100` sets the DACs' amplifiers;
  `beep [HZ|NOTE [MS]]` plays a sine tone (notes like `a4`, `c#5`), and
  `play FILE.wav` a PCM WAV file (8 or 16 bits, mono or stereo, any rate:
  converted to 48 kHz stereo). `tune c5 e5 g5 c6:500 r:100` plays notes
  (`NOTE[:MS]`, 250 ms by default, `r` rests) and `tune save FILE.wav
  NOTES` writes them as a WAV file, which `play` and the file manager play
  (on the HP: a 1.9 s tune, 357 KiB). `kernel/src/hda.rs` is only touched by
  these commands: it resets the controller, talks to the codecs through
  the CORB/RIRB rings, finds the ways (`sound::route`, breadth first
  through mixers and selectors, skipping digital converters), unmutes and
  powers them, and plays one output stream from a buffer list, polled (the
  card's interrupt line stays off). `kernel/src/sound.rs` holds what needs
  no hardware (WAV reading, tones, notes, routes, connection lists), with
  self-tests. Checked by hand on QEMU's `intel-hda` with its output
  written to a WAV file: `beep 440 1000` comes out as 1.000 s at 439.7 Hz,
  and the volume scales it. On the HP (8086:8c20, Realtek ALC221) the
  speaker, the headphone jack and the line out are found and play. It took
  one more step: Intel's output streams always ask for virtual channel 1
  when the card has it on, and the HP's firmware switches VC1 on in the
  card but leaves it off on the chipset's own link (RCBA + 0x2020), so the
  streams were never served (the command rings use VC0 and worked).
  `match_vc1` reads both (the card's through the ACPI MCFG window) and
  switches the card's VC1 off when the link's is off. If a stream still
  does not move, `beep` reports its registers. `sound.scn` checks the
  commands on a machine without a card.
- **Wallpaper:** `desktop wallpaper FILE` puts a JPEG, PNG, GIF, BMP or PPM picture behind
  the windows, scaled to cover the whole screen (cut off evenly where its
  shape differs; made on every processor in use, and again when the screen
  changes size); `desktop wallpaper none` brings back the plain color. The
  path is saved in `/system/wallpaper` and the picture loaded at boot.
  Self-tested (covering, drawing behind windows); `wallpaper.scn` uses a
  screenshot as wallpaper and checks its pixels at two screen sizes and
  after a reboot.
- **Clock and time zone:** the desktop shows the local date and time at
  the right end of its row of buttons ("Wed 18 Nov 20:13"); a small thread
  (`kernel/src/clock.rs`) changes it when the minute does. `timezone
  UTC|CET|+HH:MM|-HH:MM` sets the zone (saved in `/system/timezone`;
  UTC without one); `CET` follows the EU's summer time (CEST from the last
  Sunday of March to the last Sunday of October, 01:00 UTC). `date` then
  shows local time with UTC after it. A self-test covers the rules (the
  2026 switches to the second) and where the clock fits; `timezone.scn`
  checks the setting across a reboot and the clock on the screen.
- **System monitor:** `monitor` opens a window (`kernel/src/monitor.rs`)
  that shows every second the share of time each extra processor spent on
  jobs (processor 0 runs the kernel itself and is not measured yet: that
  needs the scheduler to count its idle time), the kernel heap, network
  traffic per second and in total, and every thread with its state. **q**
  leaves. Even a quiet desktop keeps the extra processors a few percent
  busy: they draw the screen. A self-test covers the bars and lines;
  `monitor.scn` reads the window from the host on a 4-processor VM.
- `sleep SECONDS` waits (used by the tests while another window works).
- The boot flags `desktop=off` and `desktop=on` decide for one boot, whatever
  was saved. The screen is set up before the disk is read, so a machine with
  the desktop switched off shows it for a moment while booting.
- **Mouse** (USB, see the USB section): a pointer appears when the mouse
  first moves, and the window on top can be dragged by its title bar; it
  stays completely on the screen. `desktop` shows where the pointer is.
  **Buttons** at the top of the desktop start programs with a click:
  Terminal, Files (the file manager on the home folder), Notes (the
  editor on `notes.txt` there) and Web (the web page viewer on
  `start.html` there: written the first time with links to the first web
  site, FrogFind, 68k.news and NeverSSL, sites that still work without
  https; after that it is the user's page and is left alone). A click **inside a window** goes to its
  program as the character under the pointer: the file manager chooses and
  opens, the editor puts its cursor there.
- **Tests:** self-tests for the layout at every screen size and for drawing on
  a framebuffer in RAM; scenarios `desktop*.scn` check real pixels of the
  screen (`@screenshot WxH pixels X,Y=RRGGBB ...`); `desktop-windows.scn`
  types in a second window, changes focus with Alt+Tab and the mouse, and
  closes windows; `desktop-edit.scn` edits and saves files;
  `desktop-files.scn` drives the file manager from the host,
  `desktop-launch.scn` uses the buttons and clicks in windows,
  `desktop-web.scn` the Web button and its start page. Through
  `beectl.py text` the host sees the text of every window.

## Processors: `cpu`, `cpu start`, `cpu test`, `cpu auto`

```
[cpu]  4 of 4 processors in use ('cpu auto off' to use one)
...
bee:/$ cpu
processor: Intel(R) Core(TM) i5-4590 CPU @ 3.30GHz
count: 4, 4 in use
clock: 3292 MHz
can do: sse2 sse3 ssse3 sse4.1 sse4.2 avx avx2 aes rdrand
bee:/$ cpu test
hashing 64 MiB on 1 processor: ... ms
hashing 64 MiB on 4: ... ms
same result; 3.9 times as fast
```

`kernel/src/cpu.rs` asks the processor what it is (CPUID) and counts the
machine's processors (cores and hardware threads) in the ACPI table "APIC".
The table is firmware data: every length is checked, a table that is cut off
gives the entries that are whole.

The other processors start at boot. `cpu auto off` (saved in `/system/cpu`)
keeps them waiting from the next boot on, `cpu auto on` undoes it, and the
boot flag `smp=off` / `smp=on` wins over what was saved; `cpu start` starts
them by hand. A kernel that cannot start them on some machine is caught by
the A/B trial boot like any other.

`kernel/src/smp.rs` starts the other processors (INIT and
STARTUP through the local APIC; a small trampoline at 0x8000 takes each one
from 16-bit to 64-bit mode with the kernel's page tables, descriptor tables
and memory types). The kernel itself (threads, drivers, files) still runs on
the first processor; the others only do `smp::run(count, job)`: the pieces of
a job, handed out to every processor, for work on memory only. Waiting
processors sleep in MONITOR/MWAIT (or spin in a VM) and take no interrupts.
`cpu test` hashes the same 64 MiB on one processor and on all, checks the
results match and shows the speed-up. The screen uses them too: redrawing
the console spreads its lines over every processor in use, and the copy to
video memory goes in bands of 32 lines (on the HP, a full redraw at
1280x1024 takes 7.0 ms on four instead of 12.7 ms on one; `display speed`
shows how much of it is the copy). `smp::run` may be called from anywhere:
while one job is handed out, another (from the timer interrupt, or from a
piece) runs on its own processor alone. `smp::Disjoint` wraps what pieces
share when each touches only a part of its own. Tests: self-tests (names, features,
tables cut off anywhere; every piece of a job runs exactly once), `cpu.scn`
`cpu-many.scn`, `cpu-auto.scn` and `display-many.scn` (a 4-processor VM: `@cpus 4`).

## USB: keyboard and mouse (xHCI) ✅ in QEMU, not yet tried on the HP

```
[usb]  xHCI controller 1b36:000d, 8 ports, 2 devices ('lsusb' shows them)
bee:/$ lsusb
xHCI controller 1b36:000d, 8 ports
port 5: 0627:0001 keyboard, high speed (in use, 14 reports)
port 6: 0627:0001 mouse, high speed (in use, 2 reports)
```

- **xHCI driver** (`kernel/src/xhci.rs`): takes the controller over from the
  firmware, resets it, sets up its rings, and gives every device on the
  machine's own ports an address. It runs in its own thread (`usb`) and looks
  at the controller's event ring every timer tick (10 ms): no interrupts, so
  the protected interrupt code is untouched. Plugging in and pulling out
  while running works. On Intel 7/8/9-series chipsets (the HP) it moves the
  ports from the old USB 2 controller to xHCI.
- **Keyboard** (`kernel/src/usb.rs`): reports in the boot protocol are turned
  into the scancodes a PS/2 keyboard sends, so layouts, dead keys and line
  editing work unchanged. Held keys repeat (after 0.5 s, 25 times a second).
- **Mouse**: boot protocol; moves the pointer on the desktop and drags the
  terminal window.
- **`lsusb`** lists the controller and the devices, with how many reports each
  one sent, or why a device is not used.
- The boot waits for the first look at the ports (at most 3 s), so a USB
  keyboard works when the prompt appears.
- **Not supported:** hubs (also the ones inside keyboards and monitors),
  sticks and disks, tablets and other devices without the boot protocol,
  EHCI-only machines, controllers with registers above 4 GiB.
- **Way back:** the boot flag `usb=off` leaves the controller to the firmware
  (which then keeps imitating a PS/2 keyboard in legacy BIOS boot). The ISO's
  boot menu has an entry for it: "without the USB driver".
- **Tests:** self-tests for descriptors (also cut short and lying about
  lengths), key and mouse reports, rings and requests, the pointer and
  dragging. Scenarios `usb*.scn` attach QEMU's USB keyboard, mouse, tablet,
  hub and stick, type on the USB keyboard and move the mouse through QEMU
  (`@usb`, `@keys`, `@mouse`, `@button`) and check the screen.

## Installing on the hard disk ✅ (works on the HP)

```
bee:/$ install
This install medium carries BeeCodersOs b47.
  sata0   465 GiB  "WDC WD5000AAKX": partitions of another system (2 partitions, type 07 27)
         install sata0  would refuse; with --erase: erase it and install
Nothing was changed.
bee:/$ install sata0 --erase
Disk sata0: 465 GiB, "WDC WD5000AAKX"
  now:   partitions of another system (2 partitions, type 07 27)
  after: BeeCodersOs b47: boot 16 MiB, os 256 MiB, data 4 GiB
EVERYTHING on this disk will be erased. There is no undo.
To go on, type exactly:  erase 465 GiB WDC WD5000AAKX
> erase 465 GiB WDC WD5000AAKX
  ...
Installed on sata0. Take out the install medium and type: reboot
```

Started from the ISO or the USB stick, `install` puts the OS on one of the
machine's own disks. Plan and reasons: `Docs/install-plan.html`.

| Partition | Size | Holds | Upgrade | `--reinstall` |
|---|---|---|---|---|
| 1 boot | 16 MiB, FAT16 | bootloader (BIOS and UEFI), boot settings, both kernel slots | new kernel into the spare slot | written again |
| 2 os | up to 256 MiB, BeeFS | the system's files, mounted as `/` (`/system`: settings, logs) | kept | formatted |
| 3 data | the rest, up to 4 GiB for now, BeeFS | your files, mounted at `/data` | kept | kept |

- **`install`** alone lists the disks, what is on each, and what would
  happen. It changes nothing.
- **`install DISK`** on an empty disk installs; on a disk with BeeCodersOs it
  **upgrades**: the new kernel goes into the spare slot and is on trial at
  the next start; if it crashes or fails its self-tests, the old kernel boots
  again (the A/B rollback of phase 8). Both ask for `yes`.
- **`install DISK --reinstall`** writes boot and os again and keeps data.
- **`install DISK --erase`** is the only way to touch a disk that holds
  anything else. It shows what is there and wants a sentence naming the
  disk's size and model, typed out.
- **Safety:** only from the machine's own console, never without a disk named,
  never at boot. The first sector (boot code and partition table) is cleared
  first and written last, so an interrupted install leaves a disk "without a
  partition table", which `install` recognises and does again.
- **How:** the boot area is not built by the kernel. `make iso` cuts the
  first 17 MiB off a boot disk made by `tools/bootdisk.py`
  (`build/install.img`), the bootloader loads it into memory next to the
  kernel, and `install` copies it to the disk, adds the partition table for
  that disk and formats the two BeeFS partitions. Kernel slots are files of
  a fixed 4 MiB, so an upgrade only overwrites blocks the slot already has.
- **Two file systems:** paths under `/data` go to the data partition,
  everything else to os (`kernel/src/storage.rs`). `df` shows both; `mv`
  between them copies. The console and remote shells start in `/data`.
- **A/B slots on SATA:** the slot code now looks on every disk, not only IDE,
  so rollback works on real hardware.
- **Tests:** self-tests (layout, what is on a disk, damaged FAT chains,
  memory kept from the heap, path routing); scenarios `install*.scn` on SATA
  disks (install, boot, upgrade, a crashing upgrade rolled back, reinstall,
  erase, a foreign disk left byte for byte unchanged, an interrupted
  install); and the real ISO installs to a disk and boots it, under legacy
  BIOS and UEFI.

Limits: MBR partition table (disks up to 2 TiB); the data partition stops at
4 GiB because BeeFS is not built for large partitions yet; under UEFI the
install medium needs about 160 MiB of memory; on QEMU's emulated IDE disk an
install takes half a minute (SATA: two seconds); crash reports are still only
saved on IDE disks (`crash.rs` is protected and looks there only). `install`
does not run from an installed system: an upgrade needs a newer medium.

## Versioning

`OS_VERSION` at the top of the `Makefile` is the release, set by hand (now
**0.0.3**). `BUILD_NUMBER` goes up by one on every build that has changed
sources; when you raise `OS_VERSION`, reset `BUILD_NUMBER` to 1. Both show on
the boot banner and with `version`, e.g. `BeeCodersOs 0.0.3 b6`. A/B slots and
backlog items record the build they shipped in.

## Networking — phase N7: HTTPS (TLS 1.3) ✅

The goal is HTTPS (TLS 1.3) for `wget` and `browse`: almost every site is
HTTPS-only now. The first part is the cryptography for the cipher suite
every TLS 1.3 server must offer, TLS_AES_128_GCM_SHA256 with X25519 key
exchange, in `beenet/src/crypto.rs`: X25519 (RFC 7748; a constant-time
Montgomery ladder on five 51-bit limbs), HMAC-SHA256, HKDF and TLS's
HKDF-Expand-Label (RFC 2104, 5869, 8446), AES-128 (its S-box computed at
compile time) and GCM. Host tests use the published vectors: RFC 7748
(including its 1000-round test), RFC 4231, RFC 5869, FIPS-197, and the GCM
test cases, plus every changed byte of a sealed message being refused; the
self-test `crypto for TLS` runs the same vectors in the kernel (passes on
the HP). AES and GHASH are written for clarity, not hardened against timing
attacks from the same machine.

The second part is the TLS 1.3 client, `beenet/src/tls.rs`, socket-free like
the rest of beenet: the ClientHello (server name, X25519 key share, TLS 1.3
only), the key schedule, encrypted records, the server's flight
(EncryptedExtensions, Certificate, CertificateVerify, Finished, whose MAC is
checked), our Finished, then application data both ways; tickets are set
aside, close_notify ends the stream. Host tests replay the whole
"Simple 1-RTT Handshake" of RFC 8448 (`beenet/testdata/rfc8448-1rtt.txt`)
and get every byte the RFC shows: secrets, our Finished record, data
records; also fed one byte at a time, with changed records refused and cut
or damaged hellos failing cleanly. In the kernel, `net::Conn` puts TLS over
a TCP stream; `Url` knows `https://` (port 443), so `wget https://...`,
redirects to https and `browse` all work. Keys come from
`rand::secret32` (RDRAND and timestamp-counter jitter through SHA-256). On
the HP the 854 KB Wikipedia page on honey bees arrives in 0.7 s.

**Certificates (third part).** `beenet/src/x509.rs` reads X.509
certificates (DER: names, validity, public key, subjectAltName,
basicConstraints) and follows the server's chain to one of the 118 root
certificates in `beenet/certs/roots.der` (the Mozilla set as the host's
`/etc/ssl/certs` has it, one DER certificate after the other; a root's
cross-signed twin counts as the root). It checks the host name (one `*.`
label at most), the validity dates against the clock, that only CAs sign,
and every RSA signature: `beenet/src/rsa.rs` (PKCS#1 v1.5 and PSS over
SHA-256/384/512; big numbers in 64-bit limbs) and `beenet/src/sha512.rs`
(SHA-384/512, constants computed from the primes). The TLS client then
checks the server's CertificateVerify signature with its key. A wrong name,
a date out of range, a forged signature or a non-CA signer refuses the
connection. ECDSA (fourth part): `beenet/src/ecdsa.rs` checks signatures
on P-256 and P-384 (Montgomery arithmetic in 64-bit limbs, Jacobian points
for a = -3, u1*G + u2*Q in one pass); the curve constants were checked
in Python first (G on the curve, n*G the point at infinity). Certificates
signed with ECDSA, and servers that sign the handshake with
ecdsa_secp256r1_sha256 or ecdsa_secp384r1_sha384, are checked like RSA ones.
A chain that ends at an unknown root, or needs another kind of signature,
is **not checked** (still encrypted), and `wget` and `browse` say so:

    (HTTPS: the certificate checks out (signed up to Sectigo Public Server Authentication Root R46))
    (HTTPS: encrypted, but the certificate was not checked: issued by a certificate authority this OS does not know)

Host tests: OpenSSL-made PKCS#1 and PSS signatures (and
damaged ones), the FIPS SHA-384/512 examples, real chains of example.com,
github.com and www.google.com saved with their time
(`beenet/testdata/certs`), checked at that time; wrong names, ten years
later and twenty years before, a forged byte, an empty root set; and RFC
8448's CertificateVerify (RSA-PSS) checked in the handshake trace. The
self-test `certificates` runs an RSA and an ECDSA chain in the kernel. The
ECDSA tests: OpenSSL-made P-256 and P-384 signatures (with SHA-256 and
SHA-384 on either curve), every byte changed, every length cut, the wrong
key, curve or hash; real ECDSA chains of en.wikipedia.org,
os.beecoders.com and news.ycombinator.com. On the HP, Wikipedia's page on
honey bees (854 KB, ECDSA up to ISRG Root X2) checks out in 0.7 s.

## Next phases

Following `Docs/self-improving-os-plan.html`: phase 9, AI-written apps in a
WebAssembly sandbox (new commands without touching the kernel). Other
candidates: the next display phases (`Docs/display-driver-plan.html`), a text editor, tab
completion, more layouts (Belgian Comma, French, …), physical memory manager /
paging, user programs.
