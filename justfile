set shell := ["bash", "-euo", "pipefail", "-c"]

# ── Toolchain ────────────────────────────────────────────────────────────────

cargo             := env("CARGO", "cargo")
rust_channel      := `sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml`
rust_target       := "targets/x86_64-slos.json"
userland_target   := "targets/x86_64-slos-userland.json"
kernel_rustflags  := env("KERNEL_RUSTFLAGS", "-C force-frame-pointers=yes")

# ── Paths ────────────────────────────────────────────────────────────────────

build_dir        := env("BUILD_DIR", "builddir")
cargo_target_dir := build_dir / "target"
limine_dir       := "third_party/limine"
ovmf_dir         := "third_party/ovmf"
fs_image_dir     := "fs/assets"
fs_image         := fs_image_dir / "ext2.img"
fs_image_tests   := fs_image_dir / "ext2-tests.img"
fs_image_size    := env("FS_IMAGE_SIZE", "8M")

# ── ISO outputs ──────────────────────────────────────────────────────────────

iso          := build_dir / "slop.iso"
iso_notests  := build_dir / "slop-notests.iso"
iso_tests    := build_dir / "slop-tests.iso"
log_file     := env("LOG_FILE", "test_output.log")

ports        := ""

# ── QEMU ─────────────────────────────────────────────────────────────────────

qemu_bin     := env("QEMU_BIN", "qemu-system-x86_64")
qemu_smp     := env("QEMU_SMP", "4")
qemu_mem     := env("QEMU_MEM", "512M")
qemu_accel   := if os() == "macos" { env("QEMU_ACCEL", "hvf:tcg") } else { env("QEMU_ACCEL", "kvm:tcg") }
qemu_display := if os() == "macos" { env("QEMU_DISPLAY", "cocoa") } else { env("QEMU_DISPLAY", "auto") }
qemu_cpu     := env("QEMU_CPU", "host")

qemu_fb_width       := env("QEMU_FB_WIDTH", "1920")
qemu_fb_height      := env("QEMU_FB_HEIGHT", "1080")
qemu_fb_auto        := env("QEMU_FB_AUTO", "1")
qemu_fb_auto_policy := env("QEMU_FB_AUTO_POLICY", "primary")
qemu_fb_auto_output := env("QEMU_FB_AUTO_OUTPUT", "")
qemu_gtk_zoom       := env("QEMU_GTK_ZOOM_TO_FIT", "off")

# ── Boot / Test cmdlines ────────────────────────────────────────────────────

boot_log_timeout := env("BOOT_LOG_TIMEOUT", "15")
boot_cmdline     := env("BOOT_CMDLINE", "tests=off")
test_cmdline     := "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on roulette=skip"

debug         := env("DEBUG", "0")
debug_flag    := if debug =~ '^(1|true|on|yes)$' { "boot.debug=on" } else { "" }
boot_cmdline_effective := trim(boot_cmdline + " " + debug_flag)

# ── Userland binaries ───────────────────────────────────────────────────────

userland_bins      := "init shell compositor roulette file_manager sysmon nmap ifconfig nc curl ping"
test_userland_bins := userland_bins + " fork_test io_capture_test heap_allocator_test"

# ═════════════════════════════════════════════════════════════════════════════
#  Recipes
# ═════════════════════════════════════════════════════════════════════════════

[doc("Install Rust toolchain and verify workspace")]
setup:
    scripts/ensure_toolchain.sh
    mkdir -p {{build_dir}}
    CARGO_TARGET_DIR={{cargo_target_dir}} {{cargo}} +{{rust_channel}} metadata --format-version 1 >/dev/null

# ── Userland ─────────────────────────────────────────────────────────────────

_build-userland:
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}"

_build-userland-tests: _build-userland
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}" --test

# ── Filesystem images ───────────────────────────────────────────────────────

_fs-image: _build-userland
    FS_IMAGE_SIZE={{fs_image_size}} \
        scripts/build_fs_image.sh "{{fs_image}}" "{{build_dir}}" {{userland_bins}}

_fs-image-tests: _build-userland-tests
    FS_IMAGE_SIZE={{fs_image_size}} \
        scripts/build_fs_image.sh "{{fs_image_tests}}" "{{build_dir}}" {{test_userland_bins}}

# ── Kernel ───────────────────────────────────────────────────────────────────

[doc("Build the kernel (implies fs-image)")]
build: _fs-image
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}"

# ── ISO images ───────────────────────────────────────────────────────────────

[doc("Build default ISO")]
iso: build
    LIMINE_DIR={{limine_dir}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso}}" "{{build_dir}}"

_iso-notests: build
    LIMINE_DIR={{limine_dir}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_notests}}" "{{build_dir}}" "{{boot_cmdline_effective}}"

_iso-tests: _fs-image-tests
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}" \
            "slopos-testing/qemu-exit kernel/tests"
    LIMINE_DIR={{limine_dir}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_tests}}" "{{build_dir}}" "{{test_cmdline}}"

# ── QEMU boot ───────────────────────────────────────────────────────────────

_qemu-boot mode video iso fs_image *extra_env:
    QEMU_BIN={{qemu_bin}} QEMU_SMP={{qemu_smp}} QEMU_MEM={{qemu_mem}} \
    QEMU_ACCEL={{qemu_accel}} QEMU_CPU={{qemu_cpu}} QEMU_DISPLAY={{qemu_display}} \
    VIDEO={{video}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
    QEMU_GTK_ZOOM_TO_FIT={{qemu_gtk_zoom}} \
    OVMF_DIR={{ovmf_dir}} \
    {{extra_env}} \
        scripts/qemu_run.sh "{{mode}}" "{{iso}}" "{{fs_image}}"

[doc("Boot SlopOS (ports=7777,8080 to enable host↔guest forwarding)")]
boot:
    just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS skipping the Wheel of Fate (fast dev iteration)")]
boot-fast:
    BOOT_CMDLINE="tests=off roulette=skip" just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS with release-optimized kernel (production build)")]
boot-prod:
    BOOT_CMDLINE="tests=off roulette=skip" KERNEL_RELEASE=1 just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS headless (serial only, ports= for forwarding)")]
boot-headless:
    just _iso-notests
    just _qemu-boot "interactive" "0" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot with timeout, serial log saved to test_output.log")]
boot-log: _iso-notests (_qemu-boot "logged" "0" iso_notests fs_image "BOOT_LOG_TIMEOUT=" + boot_log_timeout + " LOG_FILE=" + log_file)

[doc("Run interrupt test harness in QEMU")]
test: _iso-tests (_qemu-boot "test" "0" iso_tests fs_image_tests)

[doc("Run unit tests for abi, gfx, and font crates on the host")]
test-host:
    {{cargo}} +{{rust_channel}} test -p slopos-abi -p slopos-gfx -p slopos-font

# ── Utilities ────────────────────────────────────────────────────────────────

[doc("Show detected QEMU framebuffer resolution")]
show-qemu-resolution:
    #!/usr/bin/env bash
    set -euo pipefail
    detected="$(QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
        QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
        QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/detect_qemu_resolution.sh)"
    w="${detected%% *}"
    h="${detected##* }"
    echo "Configured framebuffer mode: ${w} x ${h}"
    if [ "{{qemu_fb_auto}}" = "0" ]; then
        echo "Auto-detection disabled (QEMU_FB_AUTO=0)."
    fi

[doc("Check formatting")]
fmt:
    {{cargo}} +{{rust_channel}} fmt --all -- --check

[doc("Enforce kernel allocation + stack-frame invariants against the current kernel.elf")]
check:
    scripts/check_alloc_dep.sh
    scripts/check_stack_sizes.sh {{build_dir}}/kernel.elf

[doc("Heuristic audit: kernel `pub fn` returning large by-value types — slow, not part of `check`")]
check-return-types:
    scripts/check_return_types.sh

[doc("Audit kernel ELF for functions whose stack frame exceeds the 32 KiB task-stack budget")]
stack-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{build_dir}}/kernel.elf" ]; then
        echo "kernel.elf missing — run \`just build\` first" >&2
        exit 1
    fi
    # Anything above 8 KiB eats into the call-depth budget on a 32 KiB
    # task stack.  See `clippy.toml` for the matching compile-time rail.
    THRESHOLD="${THRESHOLD:-8192}"
    echo "Kernel functions with frame > ${THRESHOLD} bytes:"
    objdump -d --no-show-raw-insn --disassembler-options=intel \
        "{{build_dir}}/kernel.elf" 2>/dev/null \
      | awk -v t="${THRESHOLD}" '
          /^ffffffff[0-9a-f]+ <.*>:/ { fn=$0 }
          /sub[[:space:]]+rsp,0x/ {
              m=$0; sub(/.*sub[[:space:]]+rsp,0x/,"",m); v=strtonum("0x" m);
              if (v>t) printf "%8d  %s\n", v, fn;
          }' \
      | sort -rn

[doc("Clean build artifacts")]
clean:
    {{cargo}} +{{rust_channel}} clean --target-dir {{cargo_target_dir}} || true
    rm -f {{build_dir}}/kernel.elf

[doc("Full clean including ISOs, images, and logs")]
distclean: clean
    rm -rf {{build_dir}} {{iso}} {{iso_notests}} {{iso_tests}} {{log_file}}
    rm -f {{fs_image}} {{fs_image_tests}}
