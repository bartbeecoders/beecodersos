# BeeCodersOs build system
#   make            build the kernel  -> build/beecoders.elf
#   make run        boot in QEMU with KVM (window + terminal on serial)
#   make run-nokvm  boot in QEMU with plain emulation (TCG)
#   make run-serial headless, serial output only (Ctrl-A X to quit)
#   make iso        bootable hybrid BIOS/UEFI ISO -> build/beecoders.iso
#   make run-iso    boot the ISO in QEMU (BIOS)
#   make run-iso-uefi boot the ISO in QEMU (UEFI via OVMF; serial output only)
#   make run-vm     rebuild the ISO and power-cycle an ISO-booted libvirt VM ($(VM))
#                   (vm1 boots from the A/B boot disk now: use make deploy)
#   make disk       create the BeeFS data disk build/disk.img (kept between runs)
#   make test       run the BeeFS unit tests on the host
#   make beehive    run the AI bridge daemon on the host (Claude; see tools/beehive/)
#   make report     what the OS recorded: crashes, missing commands, wishes, ...
#   make evolve     improvement loop: signals -> backlog -> coding agent -> review
#   make bootdisk   A/B boot disk build/beecoders.img (kept: it holds the VM's data)
#   make run-disk   boot the boot disk in QEMU
#   make deploy     put the new kernel in vm1's spare slot, watch it, roll back if needed
#   make check      all automated tests: unit tests (BeeFS, host tools), in-kernel
#                   self-tests, shell scenarios, A/B slots and ISO boots (BIOS + UEFI)
#   make clean      remove build output; keeps the disk images (they hold files)
#
# Versioning: OS_VERSION is set by hand below. The build number lives in the
# BUILD_NUMBER file and is bumped automatically whenever a build sees changed
# sources (not after `make clean`, and not when nothing changed).

OS_VERSION := 0.0.2
BUILD_NUMBER_FILE := BUILD_NUMBER
BUILD_STAMP := build/.build-stamp
SOURCES    := $(shell find kernel/src beefs/src boot -type f) \
              kernel/build.rs kernel/linker.ld kernel/Cargo.toml beefs/Cargo.toml

KERNEL_DIR := kernel
KERNEL_ELF := $(KERNEL_DIR)/target/x86_64-unknown-none/release/beecoders_kernel
IMAGE      := build/beecoders.elf
QEMU       := qemu-system-x86_64
DISK       := build/disk.img
DISK_MB    := 16
BEEFS_TOOL := tools/beefs-tool/target/release/beefs-tool
DISK_FLAGS := -drive file=$(DISK),format=raw,if=ide,index=0,media=disk
# Serial console on this terminal; signal=off passes Ctrl+C to BeeCodersOs.
SERIAL     := -chardev stdio,id=serial0,signal=off -serial chardev:serial0
# COM2 = AI bridge: a TCP server on localhost that beehive connects to.
# (The libvirt VM uses port 4555, see README; beehive serves both.)
AI_PORT    := 4556
AI_SERIAL  := -chardev socket,id=ai,host=127.0.0.1,port=$(AI_PORT),server=on,wait=off -serial chardev:ai
QEMU_FLAGS := -kernel $(IMAGE) -m 128M $(SERIAL) $(AI_SERIAL) $(DISK_FLAGS)

.PHONY: all kernel tool run run-nokvm run-serial iso run-iso run-iso-uefi run-vm disk test check beehive report evolve bootdisk new-bootdisk run-disk deploy clean

all: $(IMAGE)

# Bump the build number when sources changed since the last build.
$(BUILD_STAMP): $(SOURCES)
	@mkdir -p build
	@if [ -f $@ ]; then \
	   echo $$(( $$(cat $(BUILD_NUMBER_FILE)) + 1 )) > $(BUILD_NUMBER_FILE); \
	   echo "build number -> $$(cat $(BUILD_NUMBER_FILE))"; \
	 fi
	@touch $@

kernel: $(BUILD_STAMP)
	cd $(KERNEL_DIR) && BEECODERS_VERSION=$(OS_VERSION) \
	   BEECODERS_BUILD=$$(cat ../$(BUILD_NUMBER_FILE)) cargo build --release
	@echo "BeeCodersOs $(OS_VERSION) b$$(cat $(BUILD_NUMBER_FILE))"

# QEMU's Multiboot loader only accepts 32-bit ELF files, so repackage the
# 64-bit kernel as ELF32 (the code itself is unchanged).
$(IMAGE): kernel
	@mkdir -p build
	objcopy -I elf64-x86-64 -O elf32-i386 $(KERNEL_ELF) $(IMAGE)

run: $(IMAGE) | $(DISK)
	$(QEMU) $(QEMU_FLAGS) -enable-kvm -cpu host

run-nokvm: $(IMAGE) | $(DISK)
	$(QEMU) $(QEMU_FLAGS)

run-serial: $(IMAGE) | $(DISK)
	$(QEMU) -kernel $(IMAGE) -m 128M -nographic -serial mon:stdio $(AI_SERIAL) $(DISK_FLAGS)

# --- Bootable ISO (Limine) ------------------------------------------------
# Limine binaries: Arch package `limine` installs them in /usr/share/limine.
# Override with e.g. `make iso LIMINE_DIR=path/to/limine-binary`.
LIMINE_DIR := /usr/share/limine
ISO        := build/beecoders.iso
ISO_ROOT   := build/iso_root
OVMF_CODE  := /usr/share/edk2/x64/OVMF_CODE.4m.fd

iso: $(ISO)

$(ISO): $(IMAGE) boot/limine.conf
	rm -rf $(ISO_ROOT)
	mkdir -p $(ISO_ROOT)/boot/limine $(ISO_ROOT)/EFI/BOOT
	cp $(IMAGE) $(ISO_ROOT)/boot/beecoders.elf
	cp boot/limine.conf $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/limine-bios.sys $(LIMINE_DIR)/limine-bios-cd.bin \
	   $(LIMINE_DIR)/limine-uefi-cd.bin $(ISO_ROOT)/boot/limine/
	cp $(LIMINE_DIR)/BOOTX64.EFI $(LIMINE_DIR)/BOOTIA32.EFI $(ISO_ROOT)/EFI/BOOT/
	@# Replace rather than overwrite: libvirt takes ownership of attached ISOs.
	rm -f $(ISO)
	xorriso -as mkisofs -R -r -J -b boot/limine/limine-bios-cd.bin \
	   -no-emul-boot -boot-load-size 4 -boot-info-table -hfsplus \
	   -apm-block-size 2048 --efi-boot boot/limine/limine-uefi-cd.bin \
	   -efi-boot-part --efi-boot-image --protective-msdos-label \
	   $(ISO_ROOT) -o $(ISO) 2>/dev/null
	limine bios-install $(ISO)
	@echo "ISO ready: $(ISO)"

run-iso: $(ISO) | $(DISK)
	$(QEMU) -cdrom $(ISO) -m 128M $(SERIAL) $(AI_SERIAL) -enable-kvm -cpu host $(DISK_FLAGS)

run-iso-uefi: $(ISO) | $(DISK)
	$(QEMU) -cdrom $(ISO) -m 128M $(SERIAL) -enable-kvm -cpu host $(DISK_FLAGS) \
	   -drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE)

# --- libvirt VM (virt-manager) -------------------------------------------------
# A running VM keeps the old (deleted) ISO open, and a guest reboot doesn't
# re-open it, so do a full power-off/start and re-insert the fresh ISO.
VM      := vm1
VIRSH   := virsh -c qemu:///system

run-vm: $(ISO)
	-$(VIRSH) destroy $(VM) 2>/dev/null
	$(VIRSH) change-media $(VM) hdb $(abspath $(ISO)) --update --config --force
	$(VIRSH) start $(VM)

# --- BeeFS data disk --------------------------------------------------------
tool:
	cd tools/beefs-tool && cargo build --release

# Created only when missing, so files written by the OS survive between runs.
# `make disk` (re)creates it on demand.
$(DISK):
	$(MAKE) disk

disk: tool
	@mkdir -p build
	$(BEEFS_TOOL) $(DISK) mkfs $(DISK_MB)
	$(BEEFS_TOOL) $(DISK) write /welcome.txt "Welcome to BeeFS on BeeCodersOs!"
	$(BEEFS_TOOL) $(DISK) mkdir /home/bee

test:
	cd beefs && cargo test

# --- AI bridge and self-observation (phases 5-6) ------------------------------
# beehive answers `ask` / `explain` in the OS and collects its signals.
# Default backend: Claude via the Anthropic SDK (uv installs it on first run).
# Other backends: make beehive BEEHIVE_ARGS="--backend claude-code"   (Claude Code CLI login)
#                 make beehive BEEHIVE_ARGS="--backend grok"          (grok CLI login)
#                 make beehive BEEHIVE_ARGS="--backend openai --base-url http://localhost:11434/v1 --model llama3.2"
BEEHIVE_ARGS ?=
beehive:
	uv run tools/beehive/beehive.py $(BEEHIVE_ARGS)

# Reads the VM's disk (the boot disk if there is one, else build/disk.img;
# the VM should be off) and beehive's signals. Other disk: make report REPORT_DISK=...
REPORT_DISK ?= $(if $(wildcard $(BOOTDISK)),$(BOOTDISK),$(DISK))
report: tool
	python3 tools/bee_report.py --disk $(REPORT_DISK) --beehive

# --- A/B boot disk (phase 8) ---------------------------------------------------------
# Partition 1: FAT16 with Limine and kernel-a.elf / kernel-b.elf; partition 2: BeeFS.
# A new kernel goes into the spare slot and only becomes the default after it
# booted and passed its health check; otherwise the good slot boots again.
BOOTDISK := build/beecoders.img

# Created only when missing: it holds the VM's files. `make new-bootdisk` replaces it.
bootdisk: $(IMAGE) tool
	@[ -f $(BOOTDISK) ] || python3 tools/bootdisk.py create $(BOOTDISK) --kernel $(IMAGE)
	@python3 tools/bootdisk.py status $(BOOTDISK)

new-bootdisk: $(IMAGE) tool
	python3 tools/bootdisk.py create $(BOOTDISK) --kernel $(IMAGE)

run-disk: bootdisk
	$(QEMU) -m 128M $(SERIAL) $(AI_SERIAL) -enable-kvm -cpu host \
	   -drive file=$(BOOTDISK),format=raw,if=ide,index=0,media=disk

# Deploy to the libvirt VM (first time: python3 tools/deploy.py setup-vm).
deploy: $(IMAGE) bootdisk
	python3 tools/deploy.py

# --- Improvement loop (phase 7) ----------------------------------------------------
# Signals -> Docs/backlog -> coding agent (own worktree) -> make check -> review -> approve.
#   make evolve                       harvest signals, let the agent work on the top item
#   python3 tools/evolve/evolve.py status | review N | approve N [--deploy] | reject N
evolve:
	python3 tools/evolve/evolve.py run $(EVOLVE_ARGS)

# Everything an AI or human change must pass. Exit status 0 = all good.
# Single suites: tests/boot_test.py selftest|scenarios|iso|iso-uefi [-v]
check: $(IMAGE) $(ISO) tool
	cd beefs && cargo test --quiet
	python3 tests/test_beehive.py
	python3 tests/test_evolve.py
	python3 tests/test_bootdisk.py
	python3 tests/boot_test.py all

clean:
	cd $(KERNEL_DIR) && cargo clean
	cd tools/beefs-tool && cargo clean
	cd beefs && cargo clean
	@# Keep the disk images: they hold the VMs' files (delete them by hand if you mean it).
	find build -mindepth 1 -maxdepth 1 ! -name beecoders.img ! -name disk.img -exec rm -rf {} + 2>/dev/null || true
