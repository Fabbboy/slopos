#!/usr/bin/env bash
# Fail the build if any function in a kernel ELF has a stack frame larger
# than STACK_SIZE_THRESHOLD bytes. Default: 2048 (2 KiB), matching Linux
# mainline's CONFIG_FRAME_WARN default on x86_64/arm64 but enforced as a hard
# failure here. SlopOS inspects the post-link ELF rather than a compile-time
# heuristic, so inline expansion, NRVO failures, and trait-object dispatch are
# all accounted for.
#
# This 2 KiB ceiling is the load-bearing enforcement of **Inv. 5'**
# (framekernel soundness invariant): an OSTD client's stack frame cannot grow
# large enough to puncture the kernel guard page in a single function entry.
# Derived from Asterinas paper §4.3 Inv. 5 + the per-task stack guard frame
# requirement.
#
# Above it sits a second limit no allowlist can raise: the 4096 B guard page
# (mm/src/memory_layout_defs.rs, KSTACK_GUARD_SIZE). The target sets
# "stack-probes": {"kind": "none"}, so a larger frame steps clean over the
# guard in one instruction — a measured cap says how big a frame is, not
# whether that size is survivable.
#
# Each variant gets its own allowlist under scripts/gates/stack/: codegen
# differs, and a union would hide a regression in whichever is looser. The
# allowlists also carry the input-sanity floors, so weakening a check is a
# diff on a tracked file. `min-records` is what stops a dropped
# `-Zemit-stack-sizes` from reading as a kernel with no large frames —
# llvm-readobj prints an empty `StackSizes [ ]` and exits 0 without it.
#
# Usage:
#     scripts/check_stack_sizes.sh --variant dev builddir/kernel-dev.elf
#     scripts/check_stack_sizes.sh --variant dev --emit-allowlist builddir/kernel-dev.elf
#     scripts/check_stack_sizes.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

THRESHOLD="${STACK_SIZE_THRESHOLD:-2048}"

# A property of the memory layout, not a policy knob.
GUARD_PAGE=4096

VARIANT=""
ELF=""
EMIT_ALLOWLIST=0
SELF_TEST=0
GATE_DATA_DIR="$SCRIPT_DIR/gates/stack"

while [ $# -gt 0 ]; do
    case "$1" in
        --variant)
            VARIANT="${2:?--variant needs a value}"
            shift 2
            ;;
        --emit-allowlist)
            EMIT_ALLOWLIST=1
            shift
            ;;
        --self-test)
            SELF_TEST=1
            shift
            ;;
        --gate-data-dir)
            GATE_DATA_DIR="${2:?--gate-data-dir needs a value}"
            shift 2
            ;;
        -*)
            echo "check_stack_sizes: unknown option $1" >&2
            exit 2
            ;;
        *)
            ELF="$1"
            shift
            ;;
    esac
done

# ---------------------------------------------------------------------------
# Allowlist file
# ---------------------------------------------------------------------------
# Parallel arrays: bash 3.2 (macOS) has no associative arrays.
ENTRY_CAP=()
ENTRY_GLOB=()
ENTRY_LINE=()
ENTRY_HITS=()
MIN_RECORDS=""
EXPECT_TEST_REGISTRY=""
ALLOWLIST_FILE=""

load_allowlist() {
    ALLOWLIST_FILE="$GATE_DATA_DIR/$VARIANT.txt"
    if [ ! -f "$ALLOWLIST_FILE" ]; then
        echo "check_stack_sizes: no allowlist for variant '$VARIANT' at $ALLOWLIST_FILE" >&2
        echo "  Every gated build variant needs its own measured allowlist. Create it with:" >&2
        echo "      scripts/check_stack_sizes.sh --variant $VARIANT --emit-allowlist <elf>" >&2
        exit 2
    fi

    local lineno=0 line key value cap glob
    while IFS= read -r line || [ -n "$line" ]; do
        lineno=$((lineno + 1))
        case "$line" in
            ''|'#'*) continue ;;
        esac
        if [ "${line#*	}" != "$line" ]; then
            cap="${line%%	*}"
            glob="${line#*	}"
            ENTRY_CAP+=("$cap")
            ENTRY_GLOB+=("$glob")
            ENTRY_LINE+=("$lineno")
            ENTRY_HITS+=(0)
            continue
        fi
        key="${line%% *}"
        value="${line#* }"
        case "$key" in
            min-records) MIN_RECORDS="$value" ;;
            expect-test-registry) EXPECT_TEST_REGISTRY="$value" ;;
            *)
                echo "check_stack_sizes: $ALLOWLIST_FILE:$lineno: unknown directive '$key'" >&2
                exit 2
                ;;
        esac
    done < "$ALLOWLIST_FILE"

    if [ -z "$MIN_RECORDS" ] || [ -z "$EXPECT_TEST_REGISTRY" ]; then
        echo "check_stack_sizes: $ALLOWLIST_FILE must set min-records and expect-test-registry" >&2
        exit 2
    fi
}

# ---------------------------------------------------------------------------
# ELF inspection
# ---------------------------------------------------------------------------

# A kernel/tests image links thousands of 104-byte registry entries; dev and
# release link a handful. Catches a --variant stated as tests against a
# non-tests kernel and vice versa; dev-vs-release is covered by min-records
# and the mandatory-use rule instead.
TEST_REGISTRY_ENTRY_SIZE=104
TEST_REGISTRY_MANY=100

check_test_registry() {
    local span_hex entries

    # For the self-test's synthetic objects, which are not kernel images.
    [ "$EXPECT_TEST_REGISTRY" = "any" ] && return 0

    span_hex="$("$OBJDUMP" -h "$ELF" | awk '$2 == ".test_registry" { print $3; exit }')"
    if [ -z "$span_hex" ]; then
        echo "check_stack_sizes: $ELF has no .test_registry section — this is not a" >&2
        echo "  SlopOS kernel image (link.ld brackets that section unconditionally)." >&2
        exit 2
    fi
    entries=$(( 16#$span_hex / TEST_REGISTRY_ENTRY_SIZE ))

    case "$EXPECT_TEST_REGISTRY" in
        many)
            if [ "$entries" -lt "$TEST_REGISTRY_MANY" ]; then
                echo "check_stack_sizes: --variant $VARIANT expects a kernel/tests image, but" >&2
                echo "  $ELF registers only $entries test(s). You are gating the wrong binary." >&2
                exit 2
            fi
            ;;
        few)
            if [ "$entries" -ge "$TEST_REGISTRY_MANY" ]; then
                echo "check_stack_sizes: --variant $VARIANT expects a non-tests image, but" >&2
                echo "  $ELF registers $entries tests — that is the kernel/tests build." >&2
                echo "  You are gating the wrong binary; the tests kernel is kernel-tests.elf." >&2
                exit 2
            fi
            ;;
        *)
            echo "check_stack_sizes: $ALLOWLIST_FILE: expect-test-registry must be few|many|any" >&2
            exit 2
            ;;
    esac
}

# One "<size><TAB><symbol>" row per over-threshold record, plus a total.
# Hex parsed by hand: stock BSD awk has no strtonum.
read_stack_sizes() {
    "$READOBJ" --stack-sizes "$ELF" \
        | awk -v t="$THRESHOLD" '
            function hex_digit(c) {
                c = tolower(c);
                return index("0123456789abcdef", c) - 1;
            }
            function hex_to_dec(s,    i, d, value) {
                sub(/^0[xX]/, "", s);
                value = 0;
                for (i = 1; i <= length(s); i++) {
                    d = hex_digit(substr(s, i, 1));
                    if (d < 0) {
                        return -1;
                    }
                    value = value * 16 + d;
                }
                return value;
            }
            # Field-exact: a bare /Size:/ also matches the `AddressSize:`
            # header line and inflates the count.
            $1 == "Functions:" {
                fns = $0;
                sub(/.*\[/, "", fns);
                sub(/\].*/, "", fns);
            }
            $1 == "Size:" {
                records++;
                size = hex_to_dec($2);
                if (size > t) {
                    n = split(fns, a, /, */);
                    for (i = 1; i <= n; i++) {
                        if (a[i] != "") {
                            printf "%d\t%s\n", size, a[i];
                        }
                    }
                }
            }
            END { printf "records\t%d\n", records + 0 }'
}

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

run_gate() {
    load_allowlist

    if [ -z "$ELF" ]; then
        echo "check_stack_sizes: no ELF given" >&2
        exit 2
    fi
    if [ ! -f "$ELF" ]; then
        echo "check_stack_sizes: missing $ELF (run \`just build\` first)" >&2
        exit 2
    fi

    READOBJ="$("$SCRIPT_DIR/llvm_tool.sh" llvm-readobj)"
    OBJDUMP="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objdump)"

    check_test_registry

    local raw records
    raw="$(read_stack_sizes)"
    records="$(printf '%s\n' "$raw" | sed -n 's/^records	//p')"
    candidates="$(printf '%s\n' "$raw" | grep -v '^records	' | sort -rn || true)"

    if [ "${records:-0}" -lt "$MIN_RECORDS" ]; then
        echo "check_stack_sizes: only ${records:-0} .stack_sizes record(s) in $ELF" >&2
        echo "  (min-records $MIN_RECORDS in $ALLOWLIST_FILE) — refusing to report OK." >&2
        echo "  Either -Zemit-stack-sizes stopped reaching the build (see" >&2
        echo "  scripts/build_kernel.sh) or this is not the $VARIANT kernel." >&2
        exit 2
    fi

    if [ "$EMIT_ALLOWLIST" -eq 1 ]; then
        emit_allowlist "$candidates" "$records"
        return 0
    fi

    local offenders="" over_guard="" allowed_bytes=0 allowed_hits=0 largest_cap=0
    local size fn i matched cap
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        size="${line%%	*}"
        fn="${line#*	}"

        if [ "$size" -gt "$GUARD_PAGE" ]; then
            over_guard="$over_guard$size	$fn"$'\n'
            continue
        fi

        matched=0
        i=0
        while [ "$i" -lt "${#ENTRY_GLOB[@]}" ]; do
            cap="${ENTRY_CAP[$i]}"
            # Unquoted: the value is a glob.
            if [[ "$fn" == ${ENTRY_GLOB[$i]} ]]; then
                ENTRY_HITS[$i]=$(( ENTRY_HITS[i] + 1 ))
                if [ "$size" -le "$cap" ]; then
                    matched=1
                    allowed_hits=$(( allowed_hits + 1 ))
                    allowed_bytes=$(( allowed_bytes + size ))
                    [ "$cap" -gt "$largest_cap" ] && largest_cap="$cap"
                fi
                break
            fi
            i=$(( i + 1 ))
        done
        [ "$matched" -eq 1 ] && continue
        offenders="$offenders$size	$fn"$'\n'
    done <<< "$candidates"

    local fail=0

    if [ -n "$over_guard" ]; then
        echo "check_stack_sizes: frame(s) above the ${GUARD_PAGE} B stack guard page:" >&2
        printf '%s' "$over_guard" | while IFS= read -r line; do
            [ -z "$line" ] && continue
            printf '  %10d B  %s\n' "${line%%	*}" "${line#*	}" >&2
        done
        echo >&2
        echo "  The target builds with \"stack-probes\": {\"kind\": \"none\"}, so a frame" >&2
        echo "  this large steps over the guard page in one instruction. These cannot" >&2
        echo "  be allowlisted — move the large locals off the stack." >&2
        echo >&2
        fail=1
    fi

    if [ -n "$offenders" ]; then
        printf 'check_stack_sizes: function(s) exceed STACK_SIZE_THRESHOLD=%s bytes:\n' \
            "$THRESHOLD" >&2
        printf '%s' "$offenders" | while IFS= read -r line; do
            [ -z "$line" ] && continue
            printf '  %10d B  %s\n' "${line%%	*}" "${line#*	}" >&2
        done
        echo >&2
        echo "  (decode names with: cargo install rustfilt; <name> | rustfilt)" >&2
        echo "  Known >${THRESHOLD} B frames go in $ALLOWLIST_FILE with a measured cap." >&2
        echo >&2
        fail=1
    fi

    local stale=""
    i=0
    while [ "$i" -lt "${#ENTRY_GLOB[@]}" ]; do
        if [ "${ENTRY_HITS[$i]}" -eq 0 ]; then
            stale="$stale  $ALLOWLIST_FILE:${ENTRY_LINE[$i]}: ${ENTRY_GLOB[$i]}"$'\n'
        fi
        i=$(( i + 1 ))
    done
    if [ -n "$stale" ]; then
        echo "check_stack_sizes: allowlist entr(ies) matching no over-threshold frame:" >&2
        printf '%s' "$stale" >&2
        echo "  The frame shrank below ${THRESHOLD} B, or the symbol was renamed. Either way" >&2
        echo "  the exemption is dead and must be deleted — an allowlist nobody prunes" >&2
        echo "  stops describing the binary, and a stale entry is how a mis-stated" >&2
        echo "  --variant would otherwise pass." >&2
        echo >&2
        fail=1
    fi

    [ "$fail" -ne 0 ] && exit 1

    printf 'check_stack_sizes: OK — variant=%s, %d records, %d frame(s) over %s B\n' \
        "$VARIANT" "$records" "$allowed_hits" "$THRESHOLD"
    printf '  covered by %d/%d allowlist entries (%d B allowlisted, largest cap %d B, guard page %d B)\n' \
        "${#ENTRY_GLOB[@]}" "${#ENTRY_GLOB[@]}" "$allowed_bytes" "$largest_cap" "$GUARD_PAGE"
}

emit_allowlist() {
    local candidates="$1" records="$2"
    printf '# check_stack_sizes allowlist — variant: %s\n#\n' "$VARIANT"
    printf '# Emitted from %s (%d records). Review every line before committing:\n' "$ELF" "$records"
    printf '# an emitted entry records what the frame *is*, not that it is acceptable.\n\n'
    printf 'min-records %d\n' $(( records / 2 ))
    printf 'expect-test-registry few\n\n'
    printf '%s\n' "$candidates" | while IFS= read -r line; do
        [ -z "$line" ] && continue
        printf '%s\t*%s*\n' "${line%%	*}" "${line#*	}"
    done
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
# The gate has to be able to fail. Builds objects with known frames via
# `llc` and drives *this script* over them as a subprocess, so tool
# resolution, directive parsing, the guard-page ceiling, the mandatory-use
# rule and the exit codes are all exercised rather than a copy of them.
run_self_test() {
    local fixture_root llc probe_o nosec_o fail=0
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)"
    fixture_root="$(mktemp -d)"
    trap 'rm -rf "$fixture_root"' EXIT INT TERM

    # Each alloca becomes an exact frame: 3000 and 2200 sit between the
    # threshold and the guard page, 24 below it, 4104 past it.
    cat > "$fixture_root/probe.ll" <<'FIXTURE'
target triple = "x86_64-unknown-none"
declare void @sink(ptr)
define void @mid_frame() {
  %b = alloca [3000 x i8], align 16
  call void @sink(ptr %b)
  ret void
}
define void @low_frame() {
  %b = alloca [2200 x i8], align 16
  call void @sink(ptr %b)
  ret void
}
define void @small_frame() {
  %b = alloca [16 x i8], align 16
  call void @sink(ptr %b)
  ret void
}
FIXTURE
    cat > "$fixture_root/guard.ll" <<'FIXTURE'
target triple = "x86_64-unknown-none"
declare void @sink(ptr)
define void @guard_jumper() {
  %b = alloca [4096 x i8], align 16
  call void @sink(ptr %b)
  ret void
}
FIXTURE
    probe_o="$fixture_root/probe.o"
    guard_o="$fixture_root/guard.o"
    nosec_o="$fixture_root/nosec.o"
    "$llc" -mtriple=x86_64-unknown-none -stack-size-section -filetype=obj \
        -o "$probe_o" "$fixture_root/probe.ll"
    "$llc" -mtriple=x86_64-unknown-none -stack-size-section -filetype=obj \
        -o "$guard_o" "$fixture_root/guard.ll"
    "$llc" -mtriple=x86_64-unknown-none -filetype=obj \
        -o "$nosec_o" "$fixture_root/probe.ll"

    write_allowlist() {
        {
            printf 'min-records 1\n'
            printf 'expect-test-registry any\n'
            printf '%s\n' "$@"
        } > "$fixture_root/selftest.txt"
    }

    # Run the gate and assert exit code + a substring of its output.
    expect() {
        local label="$1" want_exit="$2" want_text="$3" elf="$4"
        local out status
        set +e
        out="$("$0" --variant selftest --gate-data-dir "$fixture_root" "$elf" 2>&1)"
        status=$?
        set -e
        if [ "$status" -ne "$want_exit" ]; then
            echo "check_stack_sizes --self-test: $label — expected exit $want_exit, got $status" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            fail=1
            return
        fi
        if [ -n "$want_text" ] && ! printf '%s\n' "$out" | grep -qF "$want_text"; then
            echo "check_stack_sizes --self-test: $label — output did not mention '$want_text'" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            fail=1
            return
        fi
        echo "  $label: ok"
    }

    echo "check_stack_sizes: self-test against synthesised objects"

    # An over-threshold frame is rejected, and a sub-threshold one is not
    # even mentioned.
    write_allowlist
    expect "over-threshold frame is rejected" 1 "mid_frame" "$probe_o"
    if "$0" --variant selftest --gate-data-dir "$fixture_root" "$probe_o" 2>&1 \
        | grep -q "small_frame"; then
        echo "check_stack_sizes --self-test: a sub-threshold frame was reported" >&2
        fail=1
    else
        echo "  sub-threshold frame stays silent: ok"
    fi

    # The gate has to be able to pass, or the rejections above prove only
    # that it always fails.
    write_allowlist "$(printf '3000\t*mid_frame*')" "$(printf '2200\t*low_frame*')"
    expect "measured caps are accepted" 0 "OK" "$probe_o"

    # A cap one byte short still fails.
    write_allowlist "$(printf '2999\t*mid_frame*')" "$(printf '2200\t*low_frame*')"
    expect "a cap one byte short still fails" 1 "mid_frame" "$probe_o"

    # No cap buys off a frame past the guard page.
    write_allowlist "$(printf '9999\t*guard_jumper*')"
    expect "guard-page frame is not allowlistable" 1 "above the ${GUARD_PAGE} B stack guard page" "$guard_o"

    # A dead entry fails and names the line to delete.
    write_allowlist "$(printf '3000\t*mid_frame*')" "$(printf '2200\t*low_frame*')" \
        "$(printf '9999\t*no_such_symbol*')"
    expect "stale allowlist entry is rejected" 1 "no_such_symbol" "$probe_o"

    # The second is the exact fail-open this gate had: llvm-readobj prints an
    # empty list and exits 0, so the offender list came out empty and the
    # gate printed its strongest pass message.
    {
        printf 'min-records 99999\n'
        printf 'expect-test-registry any\n'
    } > "$fixture_root/selftest.txt"
    expect "record floor is enforced" 2 "refusing to report OK" "$probe_o"
    write_allowlist
    expect "an ELF with no .stack_sizes is rejected" 2 "refusing to report OK" "$nosec_o"

    # A missing tool is a build failure, not a fallback to a host binary.
    if "$SCRIPT_DIR/llvm_tool.sh" llvm-not-a-real-tool >/dev/null 2>&1; then
        echo "check_stack_sizes --self-test: llvm_tool.sh resolved a nonexistent tool" >&2
        fail=1
    else
        echo "  missing tool fails closed: ok"
    fi

    rm -rf "$fixture_root"
    trap - EXIT INT TERM

    if [ "$fail" -ne 0 ]; then
        echo "check_stack_sizes: SELF-TEST FAILED — the gate cannot be trusted to reject" >&2
        exit 1
    fi
    echo "check_stack_sizes: self-test OK"
}

if [ "$SELF_TEST" -eq 1 ]; then
    run_self_test
    exit 0
fi

if [ -z "$VARIANT" ]; then
    echo "check_stack_sizes: --variant is required (dev | release | tests)" >&2
    echo "  It selects the measured allowlist, and a mis-stated variant fails" >&2
    echo "  rather than passing on the wrong one." >&2
    exit 2
fi

run_gate
