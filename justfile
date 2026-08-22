set shell := ["bash", "-euo", "pipefail", "-c"]

cargo             := env("CARGO", "cargo")
rust_channel      := `sed -n 's/^channel[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' rust-toolchain.toml`
rust_target       := "targets/x86_64-slos.json"
userland_target   := "targets/x86_64-slos-userland.json"
kernel_rustflags  := env("KERNEL_RUSTFLAGS", "-C force-frame-pointers=yes")

build_dir        := env("BUILD_DIR", "builddir")
cargo_target_dir := build_dir / "target"
limine_dir       := "third_party/limine"
ovmf_dir         := "third_party/ovmf"
fs_image_dir     := "fs/assets"
fs_image         := fs_image_dir / "ext2.img"
fs_image_tests   := fs_image_dir / "ext2-tests.img"
fs_image_size    := env("FS_IMAGE_SIZE", "16M")
initramfs        := build_dir / "initramfs.cpio"
initramfs_tests  := build_dir / "initramfs-tests.cpio"

# One artifact per variant: a shared path lets whichever build ran last answer
# for all three — to the gates, to gdb, to the ISO.
kernel_release   := env("KERNEL_RELEASE", "0")
kernel_variant   := if kernel_release == "1" { "release" } else { "dev" }
kernel_elf       := build_dir / ("kernel-" + kernel_variant + ".elf")
kernel_elf_tests := build_dir / "kernel-tests.elf"

iso          := build_dir / "slop.iso"
iso_notests  := build_dir / "slop-notests.iso"
iso_tests    := build_dir / "slop-tests.iso"
log_file     := env("LOG_FILE", "test_output.log")

ports        := ""

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
# Emulated display adapter: virtio-vga (default), virtio-gpu-pci, or vga.
gpu                 := env("GPU", "virtio-vga")

boot_log_timeout := env("BOOT_LOG_TIMEOUT", "15")
boot_cmdline     := env("BOOT_CMDLINE", "tests=off")
test_cmdline     := "tests=on tests.shutdown=on tests.verbosity=summary boot.debug=on roulette=skip root=auto"
# `TEST_CMDLINE=…` is how `builddir/run_tests` threads filter / verbosity flags
# into the ISO at build time.
test_cmdline_effective := env("TEST_CMDLINE", test_cmdline)

debug         := env("DEBUG", "0")
debug_flag    := if debug =~ '^(1|true|on|yes)$' { "boot.debug=on" } else { "" }
boot_cmdline_effective := trim(boot_cmdline + " " + debug_flag)

userland_bins      := "init shell terminal compositor roulette halt file_manager image_viewer sysmon nmap ip keymap ss nc curl ping oops_smoke"
test_userland_bins := userland_bins + " fork_test io_capture_test heap_allocator_test image_test curl_recv_repro_test curl_e2e_test cd_test ring_test pidfd_e2e_test signalfd_test slopfut_test multishot_test tls_independence_test percore_reactor_test signal_handler_test sigwinch_default_test ctrlc_flood_test pty_flow_test mm_stress_test spin_signal_test terminal_grid_test sysmon_selection_test clipboard_test keymap_test appkit_test spawn_privilege_test seat_test stdio_stream_test shell_script_test ip_e2e_test rlimit_test session_smoke_test spawn_output_test"

[doc("Install Rust + Go toolchains and verify workspace")]
setup:
    scripts/ensure_toolchain.sh
    scripts/ensure_go.sh
    mkdir -p {{build_dir}}
    CARGO_TARGET_DIR={{cargo_target_dir}} {{cargo}} +{{rust_channel}} metadata --format-version 1 >/dev/null

_build-userland:
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}"

_build-userland-tests: _build-userland
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} USERLAND_TARGET={{userland_target}} \
        scripts/build_userland.sh "{{build_dir}}" "{{cargo_target_dir}}" --test

_fs-image: _build-userland
    FS_IMAGE_SIZE={{fs_image_size}} \
        scripts/build_fs_image.sh "{{fs_image}}" "{{build_dir}}" {{userland_bins}}

_fs-image-tests: _build-userland-tests
    FS_IMAGE_SIZE={{fs_image_size}} \
        scripts/build_fs_image.sh "{{fs_image_tests}}" "{{build_dir}}" {{test_userland_bins}}

_initramfs: _build-userland
    scripts/build_initramfs.sh "{{initramfs}}" "{{build_dir}}" {{userland_bins}}

_initramfs-tests: _build-userland-tests
    scripts/build_initramfs.sh "{{initramfs_tests}}" "{{build_dir}}" {{test_userland_bins}}

[doc("Build the kernel (implies fs-image)")]
build: _fs-image
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}"

# No `_fs-image` dependency: building the userland binaries is most of the wall
# clock, and a gate-only job has no use for them.
[doc("Build the kernel ELF alone, skipping the fs image — for gate-only jobs")]
build-kernel-only:
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}"

[doc("Build default ISO (honors BOOT_CMDLINE, e.g. BOOT_CMDLINE='tests=off tp.debug=on')")]
iso: build _initramfs
    KERNEL_ELF={{kernel_elf}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso}}" "{{build_dir}}" "{{boot_cmdline_effective}}"

_iso-notests: build _initramfs
    KERNEL_ELF={{kernel_elf}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_notests}}" "{{build_dir}}" "{{boot_cmdline_effective}}"

_iso-tests: _fs-image-tests _initramfs-tests
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}" \
            "slopos-testing/qemu-exit kernel/tests"
    KERNEL_ELF={{kernel_elf_tests}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs_tests}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_tests}}" "{{build_dir}}" "{{test_cmdline_effective}}"

# `tests.run=__userland_only__` is a glob that deliberately matches no kernel
# test, leaving the userland phase as the only thing exercised.
_iso-tests-userland-only: _fs-image-tests _initramfs-tests
    CARGO={{cargo}} RUST_CHANNEL={{rust_channel}} RUST_TARGET={{rust_target}} \
    KERNEL_RUSTFLAGS="{{kernel_rustflags}}" \
        scripts/build_kernel.sh "{{build_dir}}" "{{cargo_target_dir}}" \
            "slopos-testing/qemu-exit kernel/tests"
    KERNEL_ELF={{kernel_elf_tests}} LIMINE_DIR={{limine_dir}} INITRAMFS_FILE={{initramfs_tests}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
        scripts/build_iso.sh "{{iso_tests}}" "{{build_dir}}" \
            "{{test_cmdline_effective}} tests.run=__userland_only__"

_qemu-boot mode video iso fs_image *extra_env:
    QEMU_BIN={{qemu_bin}} QEMU_SMP={{qemu_smp}} QEMU_MEM={{qemu_mem}} \
    QEMU_ACCEL={{qemu_accel}} QEMU_CPU={{qemu_cpu}} QEMU_DISPLAY={{qemu_display}} \
    VIDEO={{video}} \
    QEMU_FB_WIDTH={{qemu_fb_width}} QEMU_FB_HEIGHT={{qemu_fb_height}} \
    QEMU_FB_AUTO={{qemu_fb_auto}} QEMU_FB_AUTO_POLICY={{qemu_fb_auto_policy}} \
    QEMU_FB_AUTO_OUTPUT="{{qemu_fb_auto_output}}" \
    QEMU_GTK_ZOOM_TO_FIT={{qemu_gtk_zoom}} \
    GPU={{gpu}} \
    OVMF_DIR={{ovmf_dir}} \
    {{extra_env}} \
        scripts/qemu_run.sh "{{mode}}" "{{iso}}" "{{fs_image}}"

[doc("Boot SlopOS (ports=7777,8080 to enable host↔guest forwarding)")]
boot:
    just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS skipping the Wheel of Fate (fast dev iteration)")]
boot-fast:
    BOOT_CMDLINE="{{boot_cmdline_effective}} roulette=skip" just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS with release-optimized kernel (production build)")]
boot-prod:
    BOOT_CMDLINE="{{boot_cmdline_effective}} roulette=skip" KERNEL_RELEASE=1 just _iso-notests
    just _qemu-boot "interactive" "1" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot SlopOS headless (serial only, ports= for forwarding)")]
boot-headless:
    just _iso-notests
    just _qemu-boot "interactive" "0" {{iso_notests}} {{fs_image}} {{ if ports != "" { "NET=1 NET_PORTS=" + ports } else { "" } }}

[doc("Boot with timeout, serial log saved to test_output.log")]
boot-log: _iso-notests (_qemu-boot "logged" "0" iso_notests fs_image "BOOT_LOG_TIMEOUT=" + boot_log_timeout + " LOG_FILE=" + log_file)

[doc("Prove RAM-only boot: boot the ISO with NO disk attached; assert /sbin/init comes up from the initramfs (the real-hardware path)")]
boot-ramonly:
    #!/usr/bin/env bash
    set -euo pipefail
    BOOT_CMDLINE="{{boot_cmdline_effective}} roulette=skip" just _iso-notests
    just _qemu-boot "logged" "0" {{iso_notests}} {{fs_image}} "BOOT_LOG_TIMEOUT=25 LOG_FILE={{log_file}} QEMU_NO_ROOT_DISK=1"
    echo "──────── RAM-only boot: key serial lines ────────"
    grep -E "ROOTFS:|USERLAND: launched|VFS:|ext2" "{{log_file}}" || true
    echo "─────────────────────────────────────────────────"
    if grep -q "USERLAND: launched /sbin/init" "{{log_file}}"; then
        echo "PASS: /sbin/init launched from initramfs with no disk attached"
    else
        echo "FAIL: /sbin/init did not launch — full log in {{log_file}}" >&2
        exit 1
    fi

# The `debug-*` recipes attach to a QEMU already running under `boot-debug`;
# they never rebuild.

[doc("Boot with QEMU GDB stub (:1234) + monitor socket (/tmp/slopos-monitor.sock)")]
boot-debug:
    QEMU_DEBUG=1 just boot-fast

[doc("Capture all-CPU backtraces from the running kernel (writes builddir/freeze-gdb.log)")]
debug-bt:
    @test -f {{kernel_elf}} || { echo "missing {{kernel_elf}} — run 'just iso' first" >&2; exit 1; }
    @echo "Attaching to QEMU GDB stub on :1234 — kernel must be running with 'just boot-debug'…"
    gdb -q {{kernel_elf}} \
        -ex 'set pagination off' \
        -ex 'target remote :1234' \
        -ex 'info threads' \
        -ex 'thread apply all bt 30' \
        -ex 'detach' \
        -ex 'quit' 2>&1 | tee {{build_dir}}/freeze-gdb.log
    @echo "Wrote {{build_dir}}/freeze-gdb.log"

[doc("Interactive GDB attached to the running kernel (Ctrl-D to exit)")]
debug-gdb:
    @test -f {{kernel_elf}} || { echo "missing {{kernel_elf}} — run 'just iso' first" >&2; exit 1; }
    gdb -q {{kernel_elf}} \
        -ex 'set pagination off' \
        -ex 'target remote :1234'

[doc("Connect to the QEMU monitor socket — info cpus, cpu N, info registers, …")]
debug-monitor:
    @test -S /tmp/slopos-monitor.sock || { echo "no monitor socket at /tmp/slopos-monitor.sock — boot with 'just boot-debug' first" >&2; exit 1; }
    @echo "Connecting to QEMU monitor — type 'quit' or Ctrl-D to detach…"
    socat - UNIX-CONNECT:/tmp/slopos-monitor.sock

# Record/replay needs TCG + smp=1 for icount, so it cannot capture SMP-only races.

# These assert wall-clock timer calibration, which instruction-counted time
# breaks, or are flaky under single-CPU TCG; skipping them lets the "tests" boot
# step pass so recording reaches the userland phase.
rr_skip := "slopos_core::syscall::tests::test_kill_process_group_semantics,slopos_drivers::tests::apic_timer_tests::test_lapic_timer_tick_rate_reasonable,slopos_drivers::tests::hpet_tests::test_hpet_delay_accuracy"

[doc("Record a deterministic test run to builddir/replay.bin (TCG, smp=1)")]
rr-record:
    TEST_CMDLINE="{{test_cmdline}} tests.skip={{rr_skip}}" just _iso-tests
    scripts/qemu_rr.sh record "{{iso_tests}}" "{{fs_image_tests}}"

[doc("Replay builddir/replay.bin under interactive GDB (gdbstub halted on :1234)")]
rr-replay:
    @test -f {{build_dir}}/replay.bin || { echo "no recording — run 'just rr-record' first" >&2; exit 1; }
    scripts/qemu_rr.sh replay "{{iso_tests}}" "{{fs_image_tests}}" &
    sleep 2
    -gdb -q -x scripts/gdb/slopos.gdb
    -pkill -f 'rr=replay'

[doc("Batch reverse-debug: run to fault; pass WATCH=<VA> to reverse-find its writer")]
rr-gdb WATCH='0':
    @test -f {{build_dir}}/replay.bin || { echo "no recording — run 'just rr-record' first" >&2; exit 1; }
    scripts/qemu_rr.sh replay "{{iso_tests}}" "{{fs_image_tests}}" &
    sleep 2
    -gdb -q -batch -ex 'set $watch_va = {{WATCH}}' -x scripts/gdb/find_corruptor.gdb 2>&1 | tee {{build_dir}}/rr-session.log
    -pkill -f 'rr=replay'

# Idempotent — Go's build cache makes warm rebuilds ~50ms — so every `test*`
# recipe below can depend on it unconditionally.
_build-run-tests:
    mkdir -p {{build_dir}}
    cd tools/run_tests && go build -o ../../{{build_dir}}/run_tests .

[doc("Run the SlopOS test harness — live progress bar, per-failure detail; pass a 'glob' filter as the positional argument")]
test FILTER='': _build-run-tests
    {{build_dir}}/run_tests --filter "{{FILTER}}"

[doc("Re-run only the tests that failed on the previous `just test` invocation.")]
test-rerun-failed: _build-run-tests
    {{build_dir}}/run_tests --rerun-failed

[doc("Same as `just test` but dump captured klog of every test (not only failures).")]
test-verbose FILTER='': _build-run-tests
    {{build_dir}}/run_tests --verbose --filter "{{FILTER}}"

[doc("Suppress per-test output; render only failures + summary.")]
test-quiet FILTER='': _build-run-tests
    {{build_dir}}/run_tests --quiet --filter "{{FILTER}}"

[doc("Passthrough QEMU stdout verbatim — KTAP and klog interleaved. Last-resort debugging.")]
test-raw: _build-run-tests
    {{build_dir}}/run_tests --raw

[doc("Append one JSON event per line to PATH (machine-consumable).")]
test-json PATH: _build-run-tests
    {{build_dir}}/run_tests --json "{{PATH}}"

[doc("Skip the kernel-side test phase; run only the Phase 3 userland tests.")]
test-userland-only: _iso-tests-userland-only _build-run-tests
    {{build_dir}}/run_tests --no-build --iso "{{iso_tests}}" --fs-image "{{fs_image_tests}}"

[doc("Run host-side unit tests: abi, gfx, font, keymap-core, terminal-core, shell-core, net-core, plus the slopos-ostd suite natively (same tests KernMiri interprets, seconds instead of minutes — catches assertion drift early; UB detection still needs `just check-miri`)")]
test-host:
    {{cargo}} +{{rust_channel}} test -p slopos-abi -p slopos-gfx -p slopos-font -p slopos-keymap-core -p slopos-terminal-core -p slopos-shell-core -p slopos-net-core -p slopos-chrome-core -p slopos-ostd

[doc("Run the Go-based wrapper's own unit tests (host-side, no QEMU)")]
check-tests-host:
    cd tools/run_tests && go test ./...

[doc("Count-regression guard: assert `just test` plans at least TEST_COUNT_BASELINE tests")]
check-test-count: _build-run-tests
    scripts/check_test_count.sh

[doc("Lockdep ratchet: assert the validator boots ACTIVE and no pool nears its ceiling")]
check-lockdep-headroom: _build-run-tests
    scripts/check_lockdep_headroom.sh

[doc("Quota ratchet: assert every account's peak stays under its measured cap and nothing was denied")]
check-quota-headroom: _build-run-tests
    scripts/check_quota_headroom.sh

# Two passes: `TaskOwnCell::get_ptr` hands out `*mut T` rather than `&mut T` so
# two witnesses for one task may hold live pointers into the same field, and
# whether that is legal is a raw-pointer retagging question — exactly where
# Stacked and Tree Borrows differ.
[doc("Run slopos-ostd unit + integration tests under Miri to detect UB in the OSTD critical path, under both Stacked and Tree Borrows. See tools/kernmiri/README.md.")]
check-miri:
    @rustup component list --installed --toolchain {{rust_channel}} 2>/dev/null | grep -q '^miri' || rustup component add miri --toolchain {{rust_channel}}
    {{cargo}} +{{rust_channel}} miri setup
    @echo "── KernMiri: Stacked Borrows ──"
    MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks" \
        {{cargo}} +{{rust_channel}} miri test -p slopos-ostd --no-fail-fast
    @echo "── KernMiri: Tree Borrows ──"
    MIRIFLAGS="-Zmiri-disable-isolation -Zmiri-ignore-leaks -Zmiri-tree-borrows" \
        {{cargo}} +{{rust_channel}} miri test -p slopos-ostd --no-fail-fast

[doc("Print TCB ratio: unsafe lines in slopos-ostd / total kernel Rust LoC (target Phase 1 <= 1.5%, Phase 2 <= 1.0%)")]
tcb-ratio:
    scripts/tcb_ratio.sh

[doc("Download + pin the Verus toolchain (verification/verus.toml) under third_party/verus")]
ensure-verus:
    scripts/ensure_verus.sh >/dev/null

[doc("Machine-check the OSTD critical-path proofs under verification/proofs/ on the pinned Verus toolchain. Pass a proof stem to verify one file.")]
verify FILTER='':
    scripts/verify.sh "{{FILTER}}"

[doc("Fail the build on any `async fn` in a kernel crate (AD-8/AD-9/R13). OSTD + all kernel services stay sync; async lives in userspace.")]
check-no-kernel-async:
    scripts/check_no_kernel_async.sh

# Single source of truth for the gate list: CI calls this recipe rather than
# duplicating it inline, because `check-framekernel` below also runs KernMiri
# and Verus, which are separate CI jobs.
[doc("Run the framekernel gate scripts only — no fmt, KernMiri, or Verus (requires a prior `just build`)")]
check-framekernel-gates:
    # Self-tests first: a gate whose patterns have rotted produces output nobody
    # can trust.
    scripts/check_unsafe_outside_ostd.sh --self-test
    scripts/check_alloc_dep.sh --self-test
    scripts/check_no_kernel_async.sh --self-test
    scripts/check_drop_panic_free.sh --self-test
    scripts/check_wait_predicate_purity.sh --self-test
    scripts/check_wait_result_handling.sh --self-test
    scripts/check_kernel_pml4_writer.sh --self-test
    scripts/check_task_ownership.sh --self-test
    scripts/check_process_designator.sh --self-test
    scripts/check_stack_sizes.sh --self-test
    scripts/check_kernel_softfloat.sh --self-test
    scripts/check_registry_sections.sh --self-test
    scripts/check_authority_reachability.sh --self-test
    scripts/check_lockdep_headroom.sh --self-test
    scripts/check_safe_contract_surface.sh --self-test
    scripts/check_charge_linearity.sh --self-test
    scripts/check_quota_headroom.sh --self-test
    scripts/check_vendor_pin.sh
    scripts/check_unsafe_outside_ostd.sh
    scripts/check_unsafe_expansion.sh
    scripts/check_no_kernel_async.sh
    scripts/check_alloc_dep.sh
    scripts/check_drop_panic_free.sh
    scripts/check_stack_sizes.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_kernel_softfloat.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_registry_sections.sh {{build_dir}}/kernel-dev.elf
    scripts/check_authority_reachability.sh --variant dev {{build_dir}}/kernel-dev.elf
    scripts/check_wait_predicate_purity.sh
    scripts/check_wait_result_handling.sh
    scripts/check_task_ownership.sh
    scripts/check_process_designator.sh
    scripts/check_safe_contract_surface.sh
    scripts/check_charge_linearity.sh
    scripts/tcb_ratio.sh --max 1.0

# TODO(tech-debt): no `cargo clippy -- -D warnings` gate here — there is no
# clippy config in tree and the custom `no_std` target needs plumbing first.
[doc("Run every framekernel-discipline gate: vendor pin / unsafe source + expansion / async / alloc / Drop / stack / registry sections / task ownership / TCB ratio / fmt / KernMiri / Verus (requires a prior `just build`)")]
check-framekernel: check-framekernel-gates
    {{cargo}} +{{rust_channel}} fmt --all -- --check
    just check-miri
    just verify

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

[doc("Enforce kernel allocation + stack-frame invariants against the dev kernel ELF")]
check:
    scripts/check_alloc_dep.sh
    scripts/check_stack_sizes.sh --variant dev {{build_dir}}/kernel-dev.elf

[doc("Heuristic audit: kernel `pub fn` returning large by-value types — slow, not part of `check`")]
check-return-types:
    scripts/check_return_types.sh

[doc("Audit kernel ELF for functions whose stack frame exceeds the 32 KiB task-stack budget")]
stack-audit:
    #!/usr/bin/env bash
    set -euo pipefail
    if [ ! -f "{{kernel_elf}}" ]; then
        echo "{{kernel_elf}} missing — run \`just build\` first" >&2
        exit 1
    fi
    # Anything above 8 KiB eats into the call-depth budget on a 32 KiB task stack.
    THRESHOLD="${THRESHOLD:-8192}"
    echo "Kernel functions with frame > ${THRESHOLD} bytes:"
    OBJDUMP="$(scripts/llvm_tool.sh llvm-objdump)"
    "$OBJDUMP" -d --no-show-raw-insn --x86-asm-syntax=intel \
        "{{kernel_elf}}" \
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
    rm -f {{build_dir}}/kernel-*.elf

[doc("Full clean including ISOs, images, and logs")]
distclean: clean
    rm -rf {{build_dir}} {{iso}} {{iso_notests}} {{iso_tests}} {{log_file}}
    rm -f {{fs_image}} {{fs_image_tests}} {{initramfs}} {{initramfs_tests}}
