#!/usr/bin/env bash
# Fail if a process-keyed table entry point takes a bare `u32` process id.
#
# A `u32` is a number. It says nothing about whether the process it names
# still exists, and process ids are recycled, so a stale one silently
# designates whichever process holds that number *now*. Every such parameter
# is a confused-deputy surface: the kernel is the deputy, and it services the
# call against a stranger's address space or a stranger's open files.
#
# The designators that replaced them carry a generation:
#
#   slopos_ostd::process::ProcessId  — a live process, both halves consistent
#   slopos_fs::fileio::FdTable       — Kernel, or one process's descriptors
#
# Both can only be built from a live process, so a stale one fails the
# generation check instead of resolving to the slot's new occupant. That is
# the property this gate keeps: it is cheap to reintroduce a `pid: u32`
# parameter, and nothing else would notice until a recycled id landed on it.
#
# ---------------------------------------------------------------------------
# The two checks
# ---------------------------------------------------------------------------
#
#   1. No `pub fn` in the scanned modules takes a process-id-shaped `u32`
#      parameter. Matches the parameter names that have meant "process id" in
#      this tree — `process_id`, `pid`, `parent_id`, `src_process_id`,
#      `dst_process_id`, `parent_process_id`, `child_process_id`, `caller_pid`
#      — in binding position with type `u32`.
#
#   2. No lock-free scan for a matching id. The lookups these replaced walked
#      all `MAX_PROCESSES` slots comparing `process_id`; the replacement is a
#      slot index, so a fresh `.process_id ==` inside a loop over the tables
#      is the old shape growing back.
#
# Scope is deliberately narrow: `mm/src/process_vm.rs` and `fs/src/fileio/`,
# the two modules that own process-keyed tables. A `u32` pid elsewhere is
# often correct — `getpid` returns one, the syscall ABI speaks them, and the
# PCR carries one across the syscall boundary. What must not happen is a
# *table lookup* keyed on one.
#
# Deliberately accepted, and asserted in the self-test:
#   - `fn id(self) -> u32` and friends: converting a designator back to its
#     number for a log line or a syscall return is the point of having one.
#   - private helpers, which cannot be reached from another crate.
#   - `INVALID_PROCESS_ID` as a *return* value or a field sentinel.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_process_designator "$@"

# The modules that own a process-keyed table, relative to the scan root.
SCAN_PATHS='mm/src/process_vm.rs fs/src/fileio'

# Parameter names that have meant "a process id" in this tree.
PID_NAMES='process_id|pid|parent_id|src_process_id|dst_process_id|parent_process_id|child_process_id|caller_pid|target_pid'

# ---------------------------------------------------------------------------
# Check 1 — a public entry point taking a bare pid.
#
# Matches a parameter in binding position: `<name>: u32`, optionally preceded
# by `&`/`mut`. Restricted to files that hold a process-keyed table, and to
# `pub fn` signatures, which is what another crate can reach.
# ---------------------------------------------------------------------------
scan_pub_pid_params() {
    local root="$1" path
    cd "$root"
    for path in $SCAN_PATHS; do
        [ -e "$path" ] || continue
        # `-A2` because a wrapped signature puts the parameter on its own
        # line; the `pub fn` and the parameter are then two lines apart.
        find "$path" -name '*.rs' -type f 2>/dev/null | sort | while IFS= read -r file; do
            awk -v file="$file" -v names="$PID_NAMES" '
                /^[[:space:]]*pub fn / { inpub = 1; depth = 0 }
                inpub {
                    line = $0
                    if (match(line, "(" names ")[[:space:]]*:[[:space:]]*u32")) {
                        printf "pubpid\t%s:%d: %s\n", file, NR, substr(line, 1, 100)
                        inpub = 0
                        next
                    }
                    # A signature ends at the return arrow or the opening brace.
                    if (line ~ /\)[[:space:]]*(->|\{)/ || line ~ /\);[[:space:]]*$/) { inpub = 0 }
                }
            ' "$file"
        done
    done
}

# ---------------------------------------------------------------------------
# Check 2 — a lock-free scan for a matching id.
#
# The shape being kept out: a loop over the process tables comparing a slot's
# `process_id` against a caller-supplied one. Matched as a `.process_id` read
# on the same line as an equality, inside a file that also loops over the
# table bound.
# ---------------------------------------------------------------------------
scan_pid_lookups() {
    local root="$1" path
    cd "$root"
    for path in $SCAN_PATHS; do
        [ -e "$path" ] || continue
        find "$path" -name '*.rs' -type f 2>/dev/null | sort | while IFS= read -r file; do
            awk -v file="$file" '
                /for .*(0\.\.MAX_PROCESSES|PROCESS_TABLES\.iter\(\)|PROCESS_VMS\.iter\(\))/ {
                    inloop = 1; loopstart = NR; brace = 0
                }
                inloop {
                    # A `process_id` read compared for equality inside the
                    # loop is the scan shape — whether the read is a bare
                    # field or an atomic `.load(...)`.
                    if ($0 ~ /process_id/ && $0 ~ /==/) {
                        printf "pidscan\t%s:%d: %s\n", file, NR, substr($0, 1, 100)
                        inloop = 0
                        next
                    }
                    if (NR - loopstart > 12) { inloop = 0 }
                }
            ' "$file"
        done
    done
}

run_scan() {
    local root="$1"
    {
        scan_pub_pid_params "$root"
        scan_pid_lookups "$root"
    } | sort -u
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_process_designator

    # A planted public entry point taking a bare pid — the thing this exists
    # to reject. Two of them, in the two scanned trees.
    cat > "$(gate_fixture mm/src/process_vm.rs)" <<'RS'
pub fn process_vm_get_stack_top(process_id: u32) -> u64 { 0 }
pub fn process_vm_ok(process: ProcessId) -> u64 { 0 }
fn private_helper(pid: u32) -> u64 { 0 }
impl FdTable {
    pub fn id(self) -> u32 { self.id }
}
RS
    cat > "$(gate_fixture fs/src/fileio/fdops.rs)" <<'RS'
pub fn file_close_fd(
    pid: u32,
    fd: c_int,
) -> i32 { 0 }
pub fn file_read_fd(table: FdTable, fd: c_int) -> i32 { 0 }
RS
    # A planted lock-free scan.
    cat > "$(gate_fixture fs/src/fileio/mod.rs)" <<'RS'
pub(super) fn slot_for_pid(pid_arg: u32) -> Option<&'static FileTableSlot> {
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) == pid_arg {
            return Some(slot);
        }
    }
    None
}
RS
    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT")"
    gate_expect pubpid 2 "one per scanned tree"
    gate_expect pidscan 1 "the lock-free scan shape"

    # Negatives: the designator-taking forms, the private helper, and the
    # `id()` accessor must all stay silent.
    gate_expect_silent 'process_vm_ok|file_read_fd|private_helper|pub fn id' \
        "designator params, private helpers, and the id() accessor"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real scan
# ---------------------------------------------------------------------------
GATE_FINDINGS="$(run_scan "$REPO_ROOT")"

if [ -n "$GATE_FINDINGS" ]; then
    echo "check_process_designator: process-keyed entry points must not take a bare pid:" >&2
    printf '%s\n' "$GATE_FINDINGS" | sed 's/^[a-z]*\t/  /' >&2
    cat >&2 <<'MSG'

  A `u32` process id is a number, not a designator: ids recycle, so a stale
  one resolves to whichever process holds that number now. Take
  `slopos_ostd::process::ProcessId` (or `slopos_fs::fileio::FdTable`, which
  also names the kernel's own table) instead — both carry a generation and
  fail closed on a rebound slot.
MSG
    exit 1
fi

echo "check_process_designator: OK — no process-keyed entry point takes a bare pid,"
echo "check_process_designator: and no lock-free pid scan remains"
