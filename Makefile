# Convenience targets for building, booting, and testing SlopOS

.PHONY: setup build iso iso-notests iso-tests boot boot-log test clean distclean

BUILD_DIR ?= builddir
CROSS_FILE ?= metal.ini
QEMU_BIN ?= qemu-system-x86_64

ISO := $(BUILD_DIR)/slop.iso
ISO_NO_TESTS := $(BUILD_DIR)/slop-notests.iso
ISO_TESTS := $(BUILD_DIR)/slop-tests.iso
LOG_FILE ?= test_output.log

BOOT_LOG_TIMEOUT ?= 15
BOOT_CMDLINE ?= itests=off
TEST_CMDLINE ?= itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on
VIDEO ?= 0

DEBUG ?= 0
DEBUG_CMDLINE :=
ifneq ($(filter 1 true on yes,$(DEBUG)),)
DEBUG_CMDLINE += boot.debug=on
endif
BOOT_CMDLINE_EFFECTIVE := $(strip $(BOOT_CMDLINE) $(DEBUG_CMDLINE))

LIMINE_DIR := third_party/limine
LIMINE_REPO := https://github.com/limine-bootloader/limine.git
LIMINE_BRANCH := v5.x-branch-binary

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
	cp limine.cfg "$$ISO_ROOT/boot/limine.cfg"; \
	if [ -n "$$CMDLINE" ]; then \
		printf 'CMDLINE=%s\n' "$$CMDLINE" >> "$$ISO_ROOT/boot/limine.cfg"; \
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

$(BUILD_DIR)/build.ninja:
	@meson setup $(BUILD_DIR) --cross-file=$(CROSS_FILE)

setup: $(BUILD_DIR)/build.ninja

build: $(BUILD_DIR)/build.ninja
	@meson compile -C $(BUILD_DIR)

iso: build
	@meson configure $(BUILD_DIR) -Denable_builtin_tests=false
	@meson compile -C $(BUILD_DIR)
	@$(call build_iso,$(ISO),)

iso-notests: build
	@meson configure $(BUILD_DIR) -Denable_builtin_tests=false
	@meson compile -C $(BUILD_DIR)
	@$(call build_iso,$(ISO_NO_TESTS),$(BOOT_CMDLINE_EFFECTIVE))

iso-tests: build
	@meson configure $(BUILD_DIR) -Denable_builtin_tests=true
	@meson compile -C $(BUILD_DIR)
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
	if [ "$${QEMU_ENABLE_ISA_EXIT:-0}" != "0" ]; then \
		EXTRA_ARGS=" -device isa-debug-exit,iobase=0xf4,iosize=0x01"; \
	fi; \
	DISPLAY_ARGS="-display none -vga std"; \
	if [ "$${VIDEO:-0}" != "0" ]; then \
		DISPLAY_ARGS="-display gtk -vga std"; \
	fi; \
	if [ "$${QEMU_VIRGL:-0}" != "0" ]; then \
		DISPLAY_ARGS="-display gtk,gl=on -vga none"; \
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
	if [ "$${QEMU_ENABLE_ISA_EXIT:-0}" != "0" ]; then \
		EXTRA_ARGS=" -device isa-debug-exit,iobase=0xf4,iosize=0x01"; \
	fi; \
	DISPLAY_ARGS="-nographic -vga std"; \
	if [ "$${VIDEO:-0}" != "0" ]; then \
		DISPLAY_ARGS="-display gtk -vga std"; \
	fi; \
	if [ "$${QEMU_VIRGL:-0}" != "0" ]; then \
		DISPLAY_ARGS="-display gtk,gl=on -vga none"; \
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
	@meson compile -C $(BUILD_DIR) --clean || true

distclean: clean
	@rm -rf $(BUILD_DIR) $(ISO) $(ISO_NO_TESTS) $(ISO_TESTS) $(LOG_FILE)
