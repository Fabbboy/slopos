# Convenience targets for building, booting, and testing SlopOS (Rust rewrite)

.PHONY: setup build build-userland iso iso-notests iso-tests boot boot-log test clean distclean

BUILD_DIR ?= builddir
CARGO ?= cargo
RUST_TOOLCHAIN_FILE ?= rust-toolchain.toml
RUST_CHANNEL ?= $(shell sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' $(RUST_TOOLCHAIN_FILE))
RUST_TARGET_JSON ?= targets/x86_64-slos.json
CARGO_TARGET_DIR ?= $(BUILD_DIR)/target
QEMU_BIN ?= qemu-system-x86_64

ISO := $(BUILD_DIR)/slop.iso
ISO_NO_TESTS := $(BUILD_DIR)/slop-notests.iso
ISO_TESTS := $(BUILD_DIR)/slop-tests.iso
LOG_FILE ?= test_output.log

BOOT_LOG_TIMEOUT ?= 15
BOOT_CMDLINE ?= itests=off
TEST_CMDLINE ?= itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on
VIDEO ?= 0
VIRGL ?= 0
QEMU_DISPLAY ?= auto

DEBUG ?= 0
DEBUG_CMDLINE :=
ifneq ($(filter 1 true on yes,$(DEBUG)),)
DEBUG_CMDLINE += boot.debug=on
endif
BOOT_CMDLINE_EFFECTIVE := $(strip $(BOOT_CMDLINE) $(DEBUG_CMDLINE))

LIMINE_DIR := third_party/limine
LIMINE_REPO := https://github.com/limine-bootloader/limine.git
LIMINE_BRANCH := v8.x-branch-binary

OVMF_DIR := third_party/ovmf
OVMF_CODE := $(OVMF_DIR)/OVMF_CODE.fd
OVMF_VARS := $(OVMF_DIR)/OVMF_VARS.fd
SYSTEM_OVMF_DIR := /usr/share/OVMF
OVMF_CODE_URL := https://raw.githubusercontent.com/retrage/edk2-nightly/master/bin/RELEASEX64_OVMF_CODE.fd
OVMF_VARS_URL := https://raw.githubusercontent.com/retrage/edk2-nightly/master/bin/RELEASEX64_OVMF_VARS.fd

define ensure_limine
	if [ ! -d $(LIMINE_DIR) ]; then \
		echo "Cloning Limine bootloader..." >&2; \
		git clone --branch=$(LIMINE_BRANCH) --depth=1 $(LIMINE_REPO) $(LIMINE_DIR); \
	fi; \
	if [ ! -f $(LIMINE_DIR)/limine-bios.sys ] || [ ! -f $(LIMINE_DIR)/BOOTX64.EFI ]; then \
		echo "Building Limine..." >&2; \
		$(MAKE) -C $(LIMINE_DIR) >/dev/null; \
	fi;
endef

define ensure_ovmf
	mkdir -p $(OVMF_DIR); \
	if [ ! -f $(OVMF_CODE) ]; then \
		if [ -f $(SYSTEM_OVMF_DIR)/OVMF_CODE.fd ]; then \
			cp "$(SYSTEM_OVMF_DIR)/OVMF_CODE.fd" "$(OVMF_CODE)"; \
		elif [ -f $(SYSTEM_OVMF_DIR)/OVMF_CODE_4M.fd ]; then \
			cp "$(SYSTEM_OVMF_DIR)/OVMF_CODE_4M.fd" "$(OVMF_CODE)"; \
		else \
			if ! command -v curl >/dev/null 2>&1; then \
				echo "curl required to download OVMF firmware" >&2; \
				exit 1; \
			fi; \
			curl -L --fail --progress-bar "$(OVMF_CODE_URL)" -o "$(OVMF_CODE)"; \
		fi; \
	fi; \
	if [ ! -f $(OVMF_VARS) ]; then \
		if [ -f $(SYSTEM_OVMF_DIR)/OVMF_VARS.fd ]; then \
			cp "$(SYSTEM_OVMF_DIR)/OVMF_VARS.fd" "$(OVMF_VARS)"; \
		elif [ -f $(SYSTEM_OVMF_DIR)/OVMF_VARS_4M.fd ]; then \
			cp "$(SYSTEM_OVMF_DIR)/OVMF_VARS_4M.fd" "$(OVMF_VARS)"; \
		else \
			if ! command -v curl >/dev/null 2>&1; then \
				echo "curl required to download OVMF firmware" >&2; \
				exit 1; \
			fi; \
			curl -L --fail --progress-bar "$(OVMF_VARS_URL)" -o "$(OVMF_VARS)"; \
		fi; \
	fi;
endef

define ensure_rust_toolchain
	if ! command -v rustup >/dev/null 2>&1; then \
		echo "rustup is required to install the pinned nightly toolchain" >&2; \
		exit 1; \
	fi; \
	if [ -z "$(RUST_CHANNEL)" ]; then \
		echo "Failed to read Rust channel from $(RUST_TOOLCHAIN_FILE)" >&2; \
		exit 1; \
	fi; \
	if ! rustup toolchain list | grep -q "^$(RUST_CHANNEL)"; then \
		rustup toolchain install $(RUST_CHANNEL) --component=rust-src --component=rustfmt --component=clippy --component=llvm-tools-preview; \
	fi; \
	if ! rustup target list --toolchain $(RUST_CHANNEL) --installed | grep -q "^x86_64-unknown-none"; then \
		rustup target add x86_64-unknown-none --toolchain $(RUST_CHANNEL); \
	fi;
endef

define build_kernel
	set -e; \
	FEATURES="$(1)"; \
	mkdir -p $(BUILD_DIR); \
	rm -f $(BUILD_DIR)/kernel $(BUILD_DIR)/kernel.elf; \
	$(call ensure_rust_toolchain) \
	CARGO_TARGET_DIR=$(CARGO_TARGET_DIR) \
	$(CARGO) +$(RUST_CHANNEL) build \
	  -Zbuild-std=core,alloc \
	  -Zunstable-options \
	  --target $(RUST_TARGET_JSON) \
	  --package kernel \
	  --bin kernel \
	  $(if $$FEATURES,--features "$$FEATURES",) \
	  --artifact-dir $(BUILD_DIR); \
	if [ -f $(BUILD_DIR)/kernel ]; then \
		mv "$(BUILD_DIR)/kernel" "$(BUILD_DIR)/kernel.elf"; \
	fi;
endef

define build_iso
	set -e; \
	OUTPUT="$(1)"; \
	CMDLINE="$(2)"; \
	KERNEL="$(BUILD_DIR)/kernel.elf"; \
	if [ ! -f "$$KERNEL" ]; then \
		echo "Kernel not found at $$KERNEL. Run make build first." >&2; \
		exit 1; \
	fi; \
	$(call ensure_limine) \
	STAGING=$$(mktemp -d); \
	TMP_OUTPUT="$$OUTPUT.tmp"; \
	trap 'rm -rf "$$STAGING"; rm -f "$$TMP_OUTPUT"' EXIT INT TERM; \
	ISO_ROOT="$$STAGING/iso_root"; \
	mkdir -p "$$ISO_ROOT/boot" "$$ISO_ROOT/EFI/BOOT"; \
	cp "$$KERNEL" "$$ISO_ROOT/boot/kernel.elf"; \
	cp limine.conf "$$ISO_ROOT/boot/limine.conf"; \
	if [ -n "$$CMDLINE" ]; then \
		printf '    cmdline: %s\n' "$$CMDLINE" >> "$$ISO_ROOT/boot/limine.conf"; \
	fi; \
	cp $(LIMINE_DIR)/limine-bios.sys "$$ISO_ROOT/boot/"; \
	cp $(LIMINE_DIR)/limine-bios-cd.bin "$$ISO_ROOT/boot/"; \
	cp $(LIMINE_DIR)/limine-uefi-cd.bin "$$ISO_ROOT/boot/"; \
	cp $(LIMINE_DIR)/BOOTX64.EFI "$$ISO_ROOT/EFI/BOOT/"; \
	cp $(LIMINE_DIR)/BOOTIA32.EFI "$$ISO_ROOT/EFI/BOOT/" 2>/dev/null || true; \
	ISO_DIR=$$(dirname "$$OUTPUT"); \
	mkdir -p "$$ISO_DIR"; \
	xorriso -as mkisofs \
	  -V 'SLOPOS' \
	  -b boot/limine-bios-cd.bin \
	  -no-emul-boot \
	  -boot-load-size 4 \
	  -boot-info-table \
	  -eltorito-alt-boot \
	  -e boot/limine-uefi-cd.bin \
	  -no-emul-boot \
	  -isohybrid-gpt-basdat \
	  "$$ISO_ROOT" \
	  -o "$$TMP_OUTPUT"; \
	$(LIMINE_DIR)/limine bios-install "$$TMP_OUTPUT" 2>/dev/null || true; \
	mv "$$TMP_OUTPUT" "$$OUTPUT"; \
	trap - EXIT INT TERM; \
	rm -rf "$$STAGING"
endef

setup:
	@$(call ensure_rust_toolchain)
	@mkdir -p $(BUILD_DIR)
	@CARGO_TARGET_DIR=$(CARGO_TARGET_DIR) $(CARGO) +$(RUST_CHANNEL) metadata --format-version 1 >/dev/null

build-userland:
	@set -e; \
	$(call ensure_rust_toolchain) \
	mkdir -p $(BUILD_DIR); \
	CARGO_TARGET_DIR=$(CARGO_TARGET_DIR) \
	$(CARGO) +$(RUST_CHANNEL) build \
	  -Zbuild-std=core,alloc \
	  -Zunstable-options \
	  --target targets/x86_64-slos-userland.json \
	  --package slopos-userland \
	  --bin roulette \
	  --bin compositor \
	  --bin shell \
	  --bin file_manager \
	  --features standalone-bin \
	  --no-default-features \
	  --release; \
	if [ -f $(CARGO_TARGET_DIR)/x86_64-slos-userland/release/roulette ]; then \
		cp "$(CARGO_TARGET_DIR)/x86_64-slos-userland/release/roulette" "$(BUILD_DIR)/roulette.elf"; \
	fi; \
	if [ -f $(CARGO_TARGET_DIR)/x86_64-slos-userland/release/compositor ]; then \
		cp "$(CARGO_TARGET_DIR)/x86_64-slos-userland/release/compositor" "$(BUILD_DIR)/compositor.elf"; \
	fi; \
	if [ -f $(CARGO_TARGET_DIR)/x86_64-slos-userland/release/shell ]; then \
		cp "$(CARGO_TARGET_DIR)/x86_64-slos-userland/release/shell" "$(BUILD_DIR)/shell.elf"; \
	fi; \
	if [ -f $(CARGO_TARGET_DIR)/x86_64-slos-userland/release/file_manager ]; then \
		cp "$(CARGO_TARGET_DIR)/x86_64-slos-userland/release/file_manager" "$(BUILD_DIR)/file_manager.elf"; \
	fi; \
	echo "Userland binaries built: $(BUILD_DIR)/roulette.elf $(BUILD_DIR)/compositor.elf $(BUILD_DIR)/shell.elf $(BUILD_DIR)/file_manager.elf"

build: build-userland
	@$(call build_kernel)

iso: build
	@$(call build_iso,$(ISO),)

iso-notests: build
	@$(call build_iso,$(ISO_NO_TESTS),$(BOOT_CMDLINE_EFFECTIVE))

iso-tests:
	@$(call build_kernel,slopos-drivers/qemu-exit)
	@$(call build_iso,$(ISO_TESTS),$(TEST_CMDLINE))

boot: iso-notests
	@set -e; \
	$(call ensure_ovmf) \
	ISO="$(ISO_NO_TESTS)"; \
	if [ ! -f "$$ISO" ]; then \
		echo "ISO not found at $$ISO" >&2; \
		exit 1; \
	fi; \
	OVMF_VARS_RUNTIME=$$(mktemp "$(OVMF_DIR)/OVMF_VARS.runtime.XXXXXX.fd"); \
	cleanup(){ rm -f "$$OVMF_VARS_RUNTIME"; }; \
	trap cleanup EXIT INT TERM; \
	cp "$(OVMF_VARS)" "$$OVMF_VARS_RUNTIME"; \
	EXTRA_ARGS=""; \
	VIRGL_ARGS=""; \
	QEMU_VIRGL=$${QEMU_VIRGL:-$(VIRGL)}; \
	QEMU_DISPLAY=$${QEMU_DISPLAY:-$(QEMU_DISPLAY)}; \
	if [ "$${QEMU_ENABLE_ISA_EXIT:-0}" != "0" ]; then \
		EXTRA_ARGS=" -device isa-debug-exit,iobase=0xf4,iosize=0x01"; \
	fi; \
	DISPLAY_ARGS="-display none -vga std"; \
	USB_ARGS="-usb -device usb-tablet"; \
	HAS_SDL=0; \
	if $(QEMU_BIN) -display help 2>/dev/null | grep -q '^sdl$$'; then \
		HAS_SDL=1; \
	fi; \
	if [ "$${VIDEO:-0}" != "0" ]; then \
		if [ "$$QEMU_DISPLAY" = "sdl" ]; then \
			DISPLAY_ARGS="-display sdl,grab-mod=lctrl-lalt -vga std"; \
		elif [ "$$QEMU_DISPLAY" = "gtk" ]; then \
			DISPLAY_ARGS="-display gtk,grab-on-hover=on,zoom-to-fit=on -vga std"; \
		elif [ "$${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$$HAS_SDL" = "1" ]; then \
			DISPLAY_ARGS="-display sdl,grab-mod=lctrl-lalt -vga std"; \
		else \
			DISPLAY_ARGS="-display gtk,grab-on-hover=on,zoom-to-fit=on -vga std"; \
		fi; \
	fi; \
	if [ "$${QEMU_VIRGL:-0}" != "0" ]; then \
		if [ "$$QEMU_DISPLAY" = "sdl" ]; then \
			DISPLAY_ARGS="-display sdl,gl=on,grab-mod=lctrl-lalt -vga none"; \
		elif [ "$$QEMU_DISPLAY" = "gtk" ]; then \
			DISPLAY_ARGS="-display gtk,gl=on,grab-on-hover=on,zoom-to-fit=on -vga none"; \
		elif [ "$${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$$HAS_SDL" = "1" ]; then \
			DISPLAY_ARGS="-display sdl,gl=on,grab-mod=lctrl-lalt -vga none"; \
		else \
			DISPLAY_ARGS="-display gtk,gl=on,grab-on-hover=on,zoom-to-fit=on -vga none"; \
		fi; \
		VIRGL_ARGS=" -device virtio-gpu-gl-pci,multifunction=on"; \
	fi; \
	echo "Starting QEMU in interactive mode (Ctrl+C to exit)..."; \
		$(QEMU_BIN) \
	  -machine q35,accel=tcg \
	  -m 512M \
	  -drive if=pflash,format=raw,readonly=on,file="$(OVMF_CODE)" \
	  -drive if=pflash,format=raw,file="$$OVMF_VARS_RUNTIME" \
	  -device ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3 \
	  -drive if=none,id=cdrom,media=cdrom,readonly=on,file="$$ISO" \
	  -device ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0 \
	  -boot order=d,menu=on \
	  -serial stdio \
	  -monitor none \
	  $$DISPLAY_ARGS \
	  $$USB_ARGS \
	  $$EXTRA_ARGS \
	  $$VIRGL_ARGS \
	  $${QEMU_PCI_DEVICES:-}

boot-log: iso-notests
	@set -e; \
	$(call ensure_ovmf) \
	ISO="$(ISO_NO_TESTS)"; \
	if [ ! -f "$$ISO" ]; then \
		echo "ISO not found at $$ISO" >&2; \
		exit 1; \
	fi; \
	OVMF_VARS_RUNTIME=$$(mktemp "$(OVMF_DIR)/OVMF_VARS.runtime.XXXXXX.fd"); \
	cleanup(){ rm -f "$$OVMF_VARS_RUNTIME"; }; \
	trap cleanup EXIT INT TERM; \
	cp "$(OVMF_VARS)" "$$OVMF_VARS_RUNTIME"; \
	EXTRA_ARGS=""; \
	VIRGL_ARGS=""; \
	QEMU_VIRGL=$${QEMU_VIRGL:-$(VIRGL)}; \
	QEMU_DISPLAY=$${QEMU_DISPLAY:-$(QEMU_DISPLAY)}; \
	if [ "$${QEMU_ENABLE_ISA_EXIT:-0}" != "0" ]; then \
		EXTRA_ARGS=" -device isa-debug-exit,iobase=0xf4,iosize=0x01"; \
	fi; \
	DISPLAY_ARGS="-nographic -vga std"; \
	USB_ARGS="-usb -device usb-tablet"; \
	HAS_SDL=0; \
	if $(QEMU_BIN) -display help 2>/dev/null | grep -q '^sdl$$'; then \
		HAS_SDL=1; \
	fi; \
	if [ "$${VIDEO:-0}" != "0" ]; then \
		if [ "$$QEMU_DISPLAY" = "sdl" ]; then \
			DISPLAY_ARGS="-display sdl,grab-mod=lctrl-lalt -vga std"; \
		elif [ "$$QEMU_DISPLAY" = "gtk" ]; then \
			DISPLAY_ARGS="-display gtk,grab-on-hover=on,zoom-to-fit=on -vga std"; \
		elif [ "$${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$$HAS_SDL" = "1" ]; then \
			DISPLAY_ARGS="-display sdl,grab-mod=lctrl-lalt -vga std"; \
		else \
			DISPLAY_ARGS="-display gtk,grab-on-hover=on,zoom-to-fit=on -vga std"; \
		fi; \
	fi; \
	if [ "$${QEMU_VIRGL:-0}" != "0" ]; then \
		if [ "$$QEMU_DISPLAY" = "sdl" ]; then \
			DISPLAY_ARGS="-display sdl,gl=on,grab-mod=lctrl-lalt -vga none"; \
		elif [ "$$QEMU_DISPLAY" = "gtk" ]; then \
			DISPLAY_ARGS="-display gtk,gl=on,grab-on-hover=on,zoom-to-fit=on -vga none"; \
		elif [ "$${XDG_SESSION_TYPE:-x11}" = "wayland" ] && [ "$$HAS_SDL" = "1" ]; then \
			DISPLAY_ARGS="-display sdl,gl=on,grab-mod=lctrl-lalt -vga none"; \
		else \
			DISPLAY_ARGS="-display gtk,gl=on,grab-on-hover=on,zoom-to-fit=on -vga none"; \
		fi; \
		VIRGL_ARGS=" -device virtio-gpu-pci,virgl=on"; \
	fi; \
	echo "Starting QEMU with $(BOOT_LOG_TIMEOUT)s timeout (logging to $(LOG_FILE))..."; \
	set +e; \
	timeout "$(BOOT_LOG_TIMEOUT)s" $(QEMU_BIN) \
	  -machine q35,accel=tcg \
	  -m 512M \
	  -drive if=pflash,format=raw,readonly=on,file="$(OVMF_CODE)" \
	  -drive if=pflash,format=raw,file="$$OVMF_VARS_RUNTIME" \
	  -device ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3 \
	  -drive if=none,id=cdrom,media=cdrom,readonly=on,file="$$ISO" \
	  -device ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0 \
	  -boot order=d,menu=on \
	  -serial stdio \
	  -monitor none \
	  $$DISPLAY_ARGS \
	  $$USB_ARGS \
	  $$EXTRA_ARGS \
	  $$VIRGL_ARGS \
	  $${QEMU_PCI_DEVICES:-} \
	  2>&1 | tee "$(LOG_FILE)"; \
	status=$$?; \
	set -e; \
	trap - EXIT INT TERM; \
	rm -f "$$OVMF_VARS_RUNTIME"; \
	if [ $$status -eq 124 ]; then \
		echo "QEMU terminated after $(BOOT_LOG_TIMEOUT)s timeout" | tee -a "$(LOG_FILE)"; \
	fi; \
	exit $$status

test: iso-tests
	@set -e; \
	$(call ensure_ovmf) \
	ISO="$(ISO_TESTS)"; \
	if [ ! -f "$$ISO" ]; then \
		echo "ISO not found at $$ISO" >&2; \
		exit 1; \
	fi; \
	OVMF_VARS_RUNTIME=$$(mktemp "$(OVMF_DIR)/OVMF_VARS.runtime.XXXXXX.fd"); \
	cleanup(){ rm -f "$$OVMF_VARS_RUNTIME"; }; \
	trap cleanup EXIT INT TERM; \
	cp "$(OVMF_VARS)" "$$OVMF_VARS_RUNTIME"; \
	echo "Starting QEMU for interrupt test harness..."; \
	set +e; \
	$(QEMU_BIN) \
	  -machine q35,accel=tcg \
	  -m 512M \
	  -drive if=pflash,format=raw,readonly=on,file="$(OVMF_CODE)" \
	  -drive if=pflash,format=raw,file="$$OVMF_VARS_RUNTIME" \
	  -device ich9-ahci,id=ahci0,bus=pcie.0,addr=0x3 \
	  -drive if=none,id=cdrom,media=cdrom,readonly=on,file="$$ISO" \
	  -device ide-cd,bus=ahci0.0,drive=cdrom,bootindex=0 \
	  -boot order=d,menu=on \
	  -serial stdio \
	  -monitor none \
	  -nographic \
	  -vga std \
	  -usb -device usb-tablet \
	  -device isa-debug-exit,iobase=0xf4,iosize=0x01; \
	status=$$?; \
	set -e; \
	trap - EXIT INT TERM; \
	rm -f "$$OVMF_VARS_RUNTIME"; \
	if [ $$status -eq 1 ]; then \
		echo "Interrupt tests passed."; \
	elif [ $$status -eq 3 ]; then \
		echo "Interrupt tests reported failures." >&2; \
		exit 1; \
	else \
		echo "Unexpected QEMU exit status $$status" >&2; \
		exit $$status; \
	fi

clean:
	@$(CARGO) +$(RUST_CHANNEL) clean --target-dir $(CARGO_TARGET_DIR) || true
	@rm -f $(BUILD_DIR)/kernel.elf

distclean: clean
	@rm -rf $(BUILD_DIR) $(ISO) $(ISO_NO_TESTS) $(ISO_TESTS) $(LOG_FILE)
