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
boot_cmdline     := env("BOOT_CMDLINE", "itests=off")
test_cmdline     := "itests=on itests.shutdown=on itests.verbosity=summary boot.debug=on"

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
        scripts/build_iso.sh "{{iso}}" "{{build_dir}}"

_iso-notests: build
    LIMINE_DIR={{limine_dir}} \
        scripts/build_iso.sh "{{iso_notests}}" "{{build_dir}}" "{{boot_cmdline_effective}}"

_iso-tests: _fs-image-tests
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}" \
            "slopos-drivers/qemu-exit kernel/builtin-tests"
    LIMINE_DIR={{limine_dir}} \
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
    BOOT_CMDLINE="itests=off roulette=skip" just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS headless (serial only, ports= for forwarding)")]
boot-headless:
    just _iso-notests
    just _qemu-boot "interactive" "0" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot with timeout, serial log saved to test_output.log")]
boot-log: _iso-notests (_qemu-boot "logged" "0" iso_notests fs_image "BOOT_LOG_TIMEOUT=" + boot_log_timeout + " LOG_FILE=" + log_file)

[doc("Run interrupt test harness in QEMU")]
test: _iso-tests (_qemu-boot "test" "0" iso_tests fs_image_tests)

[doc("Run host-side unit tests (pixel, blend, AA, font)")]
test-host:
    #!/usr/bin/env bash
    set -euo pipefail
    # Run from /tmp to escape workspace .cargo/config.toml (build-std, custom target)
    tmp_dir=$(mktemp -d)
    trap 'rm -rf "$tmp_dir"' EXIT
    cp -r tests-host/src "$tmp_dir/src"
    cp tests-host/.cargo "$tmp_dir/.cargo" -r 2>/dev/null || true
    cat > "$tmp_dir/Cargo.toml" <<'TOML'
    [package]
    name = "slopos-tests-host"
    version = "0.1.0"
    edition = "2024"
    publish = false
    [dependencies]
    slopos-abi = { path = "SLOPOS_ROOT/abi" }
    slopos-gfx = { path = "SLOPOS_ROOT/gfx" }
    slopos-font = { path = "SLOPOS_ROOT/font" }
    [lints.rust]
    warnings = "deny"
    TOML
    repo_root="$(pwd)"
    sed -i "s|SLOPOS_ROOT|${repo_root}|g" "$tmp_dir/Cargo.toml"
    mkdir -p "$tmp_dir/.cargo"
    cat > "$tmp_dir/.cargo/config.toml" <<'CFG'
    [build]
    target = "x86_64-unknown-linux-gnu"
    CFG
    cd "$tmp_dir"
    SLOPOS_ROOT="$repo_root" cargo +{{rust_channel}} test

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

[doc("Clean build artifacts")]
clean:
    {{cargo}} +{{rust_channel}} clean --target-dir {{cargo_target_dir}} || true
    rm -f {{build_dir}}/kernel.elf

[doc("Full clean including ISOs, images, and logs")]
distclean: clean
    rm -rf {{build_dir}} {{iso}} {{iso_notests}} {{iso_tests}} {{log_file}}
    rm -f {{fs_image}} {{fs_image_tests}}
