# NOLX -- top level build.
#
#   make check      host-side unit tests for every crate (no kernel, no QEMU)
#   make kernel     build the kernel ELF for x86_64-unknown-none
#   make bootcheck  assemble + link + validate the boot layer (needs no Rust at all)
#   make fscheck    regenerate NOLXFS layouts, run layout tests, check the conformance DB
#   make report     regenerate docs/testing/CONFORMANCE.md
#   make iso        build/nolx.iso, bootable with GRUB
#   make run        boot under QEMU with the serial console on stdio
#   make mkfs       build tools/mkfs-nolxfs for the host
#   make fmt lint   formatting and lints
#
# `bootcheck` and `fscheck` are the checks that run anywhere -- a container with binutils
# and Python is enough. That is deliberate: the two most safety-critical, hardest-to-debug
# pieces (the boot trampoline and the on-disk layout) are verifiable without a toolchain.

SHELL      := /bin/bash
BUILD      := build
TARGET     := x86_64-unknown-none
KERNEL_ELF := $(BUILD)/nolx.elf
ISO        := $(BUILD)/nolx.iso
QEMU       ?= qemu-system-x86_64
QEMU_ARGS  ?=
PYTHON     ?= python3

# The kernel links with its own script. Pass the absolute path so the build does not
# depend on the working directory cargo happens to pick.
KERNEL_RUSTFLAGS := -C link-arg=-T$(CURDIR)/kernel/linker.ld -C link-arg=--build-id=none

.PHONY: all check kernel bootcheck fscheck report iso run run-kernel mkfs fmt lint clean help \
        $(BUILD)

all: check bootcheck fscheck kernel

$(BUILD):
	@mkdir -p $(BUILD)

## ---------------------------------------------------------------- checks

# T1: host-side unit tests. Every crate in crates/ is `no_std` in the kernel and `std`
# under cfg(test), so this exercises real kernel logic on a laptop.
# NB: no --workspace here. The kernel is a no_std staticlib that cannot build a test
# harness; `cargo test` with no arguments uses default-members, which excludes it.
check:
	cargo test --lib --bins
	cargo test --tests

# Boot layer: assemble, link against the real linker script, validate the Multiboot
# header. Needs binutils and Python only.
bootcheck: | $(BUILD)
	bash scripts/check-boot-layer.sh $(BUILD)

# On-disk format: the generator must be idempotent and the layout invariants must hold.
fscheck:
	$(PYTHON) tools/nolxfs-codegen/gen.py --check
	$(PYTHON) tools/nolxfs-codegen/test_layout.py
	$(PYTHON) tools/conformance-report.py --check

report:
	$(PYTHON) tools/conformance-report.py

## ---------------------------------------------------------------- build

kernel: | $(BUILD)
	rustup target list --installed | grep -q '^$(TARGET)$$' || \
		rustup target add $(TARGET)
	RUSTFLAGS="$(KERNEL_RUSTFLAGS)" \
		cargo build --release --target $(TARGET) -p nolx-kernel
	cp target/$(TARGET)/release/nolx-kernel $(KERNEL_ELF)
	@echo "kernel: $(KERNEL_ELF)"
	@rustc --version --verbose > $(BUILD)/buildinfo.txt
	@echo "built: $$(date -u +%Y-%m-%dT%H:%M:%SZ)" >> $(BUILD)/buildinfo.txt

mkfs: | $(BUILD)
	cargo build --release -p mkfs-nolxfs
	cp target/release/mkfs-nolxfs $(BUILD)/mkfs.nolxfs

iso: kernel | $(BUILD)
	mkdir -p $(BUILD)/iso/boot/grub
	cp $(KERNEL_ELF) $(BUILD)/iso/boot/nolx.elf
	cp scripts/grub/grub.cfg $(BUILD)/iso/boot/grub/grub.cfg
	grub-mkrescue -o $(ISO) $(BUILD)/iso
	@echo "iso: $(ISO)"

## ---------------------------------------------------------------- run

# Primary path: boot the GRUB ISO. GRUB 2 loads the ELF64 kernel through its Multiboot
# loader, which is the same path real hardware takes.
run: iso
	$(QEMU) -cdrom $(ISO) -m 512M -serial stdio -display none $(QEMU_ARGS)

# Convenience path: hand a flat image straight to QEMU with -kernel. Uses the Multiboot
# a.out kludge fields in the header. If your QEMU rejects it, use `make run`.
run-kernel: kernel | $(BUILD)
	objcopy -O binary $(KERNEL_ELF) $(BUILD)/nolx.bin
	$(QEMU) -kernel $(BUILD)/nolx.bin -m 512M -serial stdio -display none $(QEMU_ARGS)

## ---------------------------------------------------------------- hygiene

fmt:
	cargo fmt --all

lint:
	cargo fmt --all -- --check
	cargo clippy --all-targets -- -D warnings
	@echo "-- checking that every unsafe block states its invariant"
	@scripts/check-unsafe.sh

clean:
	rm -rf $(BUILD) target

help:
	@grep -E '^#   make' $(MAKEFILE_LIST) | sed 's/^#   /  /'
