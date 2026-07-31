#!/usr/bin/env bash
set -euo pipefail

# Single-writer gate for the kernel master PML4.
#
# Every kernel-half page-table write goes through
# `slopos_ostd::mm::vm_space::CursorMut` under the `KERNEL_VM_SPACE`
# lock, reached from `slopos_mm::kernel_mappings`. A second writer over
# the same physical PML4 — a raw descent that allocates its own
# intermediates and stores its own entries — cannot be serialised
# against the cursor, because the cursor's exclusivity comes from the
# borrow checker over one `VmSpace` object and a raw descent names no
# object at all. Two such writers racing lose leaves and leak the tables
# that held them; a CPU whose page then resolves to nothing takes a
# fault it cannot service.
#
# So the gate is a name check, not a behaviour check: if these symbols
# come back, the discipline is gone whether or not the reintroduced code
# happens to be correct today.
#
# Runs from `just build` (cheap: a handful of greps over `*.rs`) and
# from `just check-framekernel-gates`.
#
# Usage:
#     scripts/check_kernel_pml4_writer.sh
#     scripts/check_kernel_pml4_writer.sh --self-test

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
cd "$REPO_ROOT"

# Kernel crates only. Userland, slibc and the host-side tooling are out
# of scope for the framekernel discipline.
KERNEL_CRATES=(
    abi acpi boot core drivers font fs gfx hermetic karch kernel kernel-services
    mm net pidfd ring sched service-core signalfd slopos-ostd slopos-rt video
    windowing ktesting
)

fail=0

report() {
    echo "check_kernel_pml4_writer: FAIL — $1" >&2
    fail=1
}

# Search every kernel crate's Rust sources for $1, printing file:line.
scan() {
    local pattern="$1"
    local dir
    for dir in "${KERNEL_CRATES[@]}"; do
        [ -d "$dir" ] || continue
        grep -rn --include='*.rs' -E "$pattern" "$dir" 2>/dev/null || true
    done
}

# ---------------------------------------------------------------------------
# 1. `mm::paging` exports no kernel-half mapping surface.
#
# `paging` is the read side: `virt_to_phys`, `is_mapped`,
# `get_page_size`, `kernel_pml4_phys`, `walk_phys`. A `map_page_4kb` or
# `unmap_page` there is a second writer by construction — it descends
# the master with no lock and allocates its own intermediates.
# ---------------------------------------------------------------------------
hits="$(scan '\b(map_page_4kb|map_page_4kb_in|unmap_page|unmap_page_4kb_in|prune_empty_tables|alloc_page_table|split_pdpt_huge|split_pd_huge)\b' | grep -v '^slopos-ostd/' || true)"
if [ -n "$hits" ]; then
    report "a raw kernel-half mapping symbol is back outside slopos-ostd:"
    printf '%s\n' "$hits" >&2
    echo "      Kernel-half writes go through slopos_mm::kernel_mappings," >&2
    echo "      which drives CursorMut under the KERNEL_VM_SPACE lock." >&2
fi

# ---------------------------------------------------------------------------
# 2. `page_table_defs` exports no page-table write accessor.
#
# `entry_at` reads one entry as a relaxed atomic and is shared by the
# walker. A `set_entry_at` / `zero_table_at` / `unlink_child` beside it
# is a raw store into an arbitrary HHDM-reachable frame, available to
# every `#![forbid(unsafe_code)]` consumer in the tree.
# ---------------------------------------------------------------------------
hits="$(scan '\b(set_entry_at|zero_table_at|table_empty_at)\b' || true)"
# `unlink_child` is also the name of an unrelated task-family helper in
# sched, so scope that one to the module it would come back in.
hits="$hits$(grep -rn --include='*.rs' -E '\bunlink_child\b' mm/src/paging 2>/dev/null || true)"
if [ -n "$hits" ]; then
    report "a page-table write accessor is back in mm::paging::page_table_defs:"
    printf '%s\n' "$hits" >&2
fi

# ---------------------------------------------------------------------------
# 3. The kernel-master generation protocol stays deleted.
#
# All 256 kernel-half PML4 entries are linked at boot
# (`prepopulate_kernel_half`), so a top-level entry can never appear
# after `VmSpace::new`, and the one-shot `copy_kernel_half` at
# construction is correct forever. Reintroducing a generation counter
# would put an atomic load and a 2 KiB memcpy back on the context-switch
# path to track a transition that cannot happen.
# ---------------------------------------------------------------------------
hits="$(scan '\b(KERNEL_MASTER_GEN|bump_kernel_master_gen|resync_kernel_half_if_stale)\b' || true)"
if [ -n "$hits" ]; then
    report "the kernel-master generation protocol is back:"
    printf '%s\n' "$hits" >&2
    echo "      prepopulate_kernel_half makes it unnecessary; see vm_space.rs." >&2
fi

# ---------------------------------------------------------------------------
# Self-test: the gate has to be able to fail. Plant each banned symbol
# in a scratch kernel-crate file and confirm the scan finds it.
# ---------------------------------------------------------------------------
if [ "${1:-}" = "--self-test" ]; then
    probe="mm/src/__pml4_gate_self_test.rs"
    trap 'rm -f "$REPO_ROOT/$probe"' EXIT INT TERM
    for sym in map_page_4kb set_entry_at bump_kernel_master_gen; do
        printf 'fn %s() {}\n' "$sym" > "$probe"
        if ! scan "\\b$sym\\b" | grep -q "$probe"; then
            echo "check_kernel_pml4_writer: SELF-TEST FAIL — scan missed $sym" >&2
            exit 1
        fi
    done
    rm -f "$probe"
    trap - EXIT INT TERM
    echo "check_kernel_pml4_writer: self-test OK (all three scans detect a planted symbol)"
fi

if [ "$fail" -ne 0 ]; then
    exit 1
fi

echo "check_kernel_pml4_writer: OK — the kernel master PML4 has one writer"
