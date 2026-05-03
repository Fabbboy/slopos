#!/usr/bin/env bash
# Audit kernel `pub fn` / `pub(crate) fn` return types for value-returns
# of large structs that would re-introduce the stack-frame bug class.
#
# Heuristic-based (regex over .rs files, not full AST):
#   - Skip return types matched by SAFE_RETURN_RE: `Result`, `Option`,
#     `KBox`, `KVec`, `KArc`, `KVecDeque`, `KBTreeMap`, `PinBox`,
#     `Pin<KBox<...>>`, primitives, `()`, references, raw pointers,
#     known small newtypes / enums.
#   - List everything else for human review.
#
# Advisory — pin-init + KBox structurally forbid the pattern in
# migrated subsystems; this script catches new regressions before the
# ELF stack-sizes gate has a chance to.
#
# Exit code is informational: the script does not fail on hits because
# many are obvious small newtypes (tuples of integers, single-field
# enums) that are tedious to whitelist. The ELF `check_stack_sizes.sh`
# gate is the load-bearing enforcement; this script's value is in the
# printed list, not its exit code. Set `STRICT=1` to make the script
# fail on any hit (CI strict-mode).

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

# Kernel crates only — userland is exempt.
KERNEL_DIRS=(
    "$REPO_ROOT/abi/src"
    "$REPO_ROOT/boot/src"
    "$REPO_ROOT/core/src"
    "$REPO_ROOT/drivers/src"
    "$REPO_ROOT/font/src"
    "$REPO_ROOT/fs/src"
    "$REPO_ROOT/gfx/src"
    "$REPO_ROOT/kernel/src"
    "$REPO_ROOT/karch/src"
    "$REPO_ROOT/kernel-services/src"
    "$REPO_ROOT/mm/src"
    "$REPO_ROOT/net/src"
    "$REPO_ROOT/sched/src"
    "$REPO_ROOT/slopos-ostd/src"
    "$REPO_ROOT/sync/src"
    "$REPO_ROOT/utils/src"
    "$REPO_ROOT/video/src"
)

# Return-type denylist — anything matching is treated as "small/safe":
#  - The `slopos_ostd::mm::heap` wrapper types (KBox/KVec/KArc/...).
#  - `Result<...>` and `Option<...>` (callers unwrap, the inner type
#    has its own gate; wrapping itself is small).
#  - Primitives: bool, i*, u*, usize, isize, f32, f64, char, c_int.
#  - References, pointers, slices: &T, &mut T, *const T, *mut T, [T].
#  - `()`, `!`, `Self`, function pointers.
#  - `impl Trait` / `dyn Trait` (not by-value when used as return).
# Anything matching this regex is treated as a known-small return.
# Heuristic — adjust as new small newtypes are added. The wrapper
# types (KBox/KVec/KArc/KVecDeque/KBTreeMap/PinBox) are all safe
# containers exposed by the kernel allocation surface.
SAFE_RETURN_RE='^(Result<|Option<|Result$|Option$|.*Result(<|$)|.*Error(<|$)|KBox<|KVec<|KArc<|KVecDeque<|KBTreeMap<|PinBox<|Pin<KBox|Pin<PinBox|Pin<&|bool$|i8$|i16$|i32$|i64$|i128$|isize$|u8$|u16$|u32$|u64$|u128$|usize$|f32$|f64$|char$|c_int$|c_uint$|c_void$|c_char$|str$|\(\)$|!$|Self$|&|\*const|\*mut|\[|impl |dyn |fn\(|core::|alloc::|.*Flags$|.*Kind$|.*Status$|.*Mode$|.*Token$|.*Handle(<|$)|.*Index(<|$)|.*Id(<|$)|.*Addr(<|$)|.*Format$|.*Type$|Color32$|EncodedPixel$|DamageRect$|FusedPollResult$|MemoryRegion$|BootInfo$|TestResult$|MaybeUninit<|NonNull<|HMetrics$|SeqNum$|Port$|MemfdHandle$|VideoBackend$|RstAction$|RegionPurpose$|Protection$|RegionBacking$|DepResult$|HeapStats$|ParseDhcpOptResult$|RouteEntry$|InterruptFrame$|RawWindowHandle$|RawDisplayHandle$|WindowHandle<|DisplayHandle<|SigSet$|SigDefault$|SigInfo$|UserSigaction$|FileKind$|FileType$|FileStat$)'

declare -a OFFENDERS=()

for dir in "${KERNEL_DIRS[@]}"; do
    [ -d "$dir" ] || continue
    while IFS= read -r -d '' file; do
        # Skip test-only files.
        case "$file" in
            */tests/*|*/tests.rs|*/test_*) continue ;;
        esac
        while IFS= read -r match; do
            line_no="${match%%:*}"
            rest="${match#*:}"
            # Strip leading whitespace and the visibility prefix.
            sig="$(echo "$rest" | sed -E 's/^[[:space:]]*//; s/^pub(\([a-z()]*\))?[[:space:]]+//; s/^const[[:space:]]+//; s/^unsafe[[:space:]]+//; s/^async[[:space:]]+//')"
            # Pick out the return type (` -> TYPE` up to `{` or `where`).
            ret="$(echo "$sig" | sed -nE 's/.*->[[:space:]]*([^{[:space:]][^{]*)\{?.*/\1/p' | head -1 | sed -E 's/[[:space:]]+$//; s/where.*$//; s/[[:space:]]+$//')"
            [ -z "$ret" ] && continue
            # Strip trailing ` ` and `;`.
            ret="$(echo "$ret" | sed -E 's/;[[:space:]]*$//; s/[[:space:]]+$//')"
            if [[ "$ret" =~ $SAFE_RETURN_RE ]]; then
                continue
            fi
            OFFENDERS+=("$file:$line_no: -> $ret")
        done < <(grep -nE '^\s*(pub(\([a-z()]*\))?\s+)?(unsafe\s+|async\s+|const\s+)*fn\s+[A-Za-z0-9_]+.*->' "$file" || true)
    done < <(find "$dir" -name '*.rs' -print0)
done

if [ "${#OFFENDERS[@]}" -gt 0 ]; then
    printf 'check_return_types: %d candidate(s) returning non-wrapped types:\n' \
        "${#OFFENDERS[@]}" >&2
    printf '  %s\n' "${OFFENDERS[@]}" >&2
    echo >&2
    echo "  Heuristic hits — verify each is actually small (≤ 128 B)." >&2
    echo "  Add the type to SAFE_RETURN_RE in this script if it's a small newtype." >&2
    echo "  ELF check_stack_sizes.sh is the load-bearing gate; this is advisory." >&2
    if [ "${STRICT:-0}" = "1" ]; then
        exit 1
    fi
    exit 0
fi

echo "check_return_types: OK — no large by-value returns flagged"
