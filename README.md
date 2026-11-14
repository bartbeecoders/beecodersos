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
kernel/
  linker.ld            kernel loaded at 1 MiB
  src/boot.s           Multiboot header, CPU checks, page tables (4 GiB), 32→64-bit switch
  src/interrupts.s/.rs IDT, exception handlers, 8259 PIC, IRQ dispatch
  src/main.rs          kernel_main, panic handler
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
`mem`, `about`, `reboot`, `shutdown`. Paths can be relative; `"quotes"` group words.

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

## Next phases

To be decided — candidates: framebuffer console (on-screen text on UEFI), timer +
clock (`date`, `uptime`), a text editor, tab completion, more layouts
(Belgian Comma, French, …), physical memory manager / paging, user programs.
