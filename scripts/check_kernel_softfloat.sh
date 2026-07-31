#!/usr/bin/env bash
# Fail the build if the kernel ELF touches XCR0-managed register state
# outside the sanctioned save/restore.
#
# Why this is load-bearing: a syscall or exception (page fault, IRQ) that
# enters from userland does NOT save the caller's FPU/vector state — that
# only happens on a full context switch (xsave/xrstor in the scheduler). If
# the kernel disturbs any of that state in such a path, it clobbers the
# interrupted user task's live registers and the restarted user instruction
# reads garbage. The classic symptom is a userland AVX `vmovups` memset that
# demand-faults mid-fill and ends up with stale zeros (garbage glyphs after a
# terminal resize).
#
# The registers at risk are the ones XCR0 enumerates, not just the vector
# ones: x87 and MMX share one physical file with XMM under XCR0 bit 0 (MMn
# *is* the mantissa of STn), saved by the same `xsave64`/`xrstor64` pair, so
# a stray `fldz` corrupts user state by exactly the mechanism a stray
# `movups` does. `xsave`/`xrstor` are in scope too — an unreviewed one
# overwrites the whole file at once.
#
# The soft-float guarantee comes from `targets/x86_64-slos.json`
# (`features: ...,-sse,...,+soft-float` + `rustc-abi: x86-softfloat`), not
# from `.cargo/config.toml`: a `RUSTFLAGS` env var fully overrides
# `target.*.rustflags`. And hand-written `asm!` is not subject to target
# features at all, which is what makes this scan the only thing that would
# catch a hand-rolled `emms`.
#
# Each variant gets its own allowlist under scripts/gates/vector/, keyed by
# enclosing symbol with a measured occurrence budget. The tests build's xsave
# conformance helpers are one entry there rather than a whole-binary
# exemption, so a vector instruction anywhere *else* still fails.
#
# Usage:
#     scripts/check_kernel_softfloat.sh --variant dev builddir/kernel-dev.elf
#     scripts/check_kernel_softfloat.sh --variant dev --emit-allowlist builddir/kernel-dev.elf
#     scripts/check_kernel_softfloat.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"

VARIANT=""
ELF=""
EMIT_ALLOWLIST=0
SELF_TEST=0
GATE_DATA_DIR="$SCRIPT_DIR/gates/vector"

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
            echo "check_kernel_softfloat: unknown option $1" >&2
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
ENTRY_BUDGET=()
ENTRY_CLASS=()
ENTRY_GLOB=()
ENTRY_LINE=()
ENTRY_HITS=()
MIN_RETURNS=""
MIN_SYMBOLS=""
EXPECT_TEST_REGISTRY=""
ALLOWLIST_FILE=""

load_allowlist() {
    ALLOWLIST_FILE="$GATE_DATA_DIR/$VARIANT.txt"
    if [ ! -f "$ALLOWLIST_FILE" ]; then
        echo "check_kernel_softfloat: no allowlist for variant '$VARIANT' at $ALLOWLIST_FILE" >&2
        echo "  Every gated build variant needs its own measured allowlist. Create it with:" >&2
        echo "      scripts/check_kernel_softfloat.sh --variant $VARIANT --emit-allowlist <elf>" >&2
        exit 2
    fi

    local lineno=0 line key value rest
    while IFS= read -r line || [ -n "$line" ]; do
        lineno=$((lineno + 1))
        case "$line" in
            ''|'#'*) continue ;;
        esac
        if [ "${line#*	}" != "$line" ]; then
            ENTRY_BUDGET+=("${line%%	*}")
            rest="${line#*	}"
            ENTRY_CLASS+=("${rest%%	*}")
            ENTRY_GLOB+=("${rest#*	}")
            ENTRY_LINE+=("$lineno")
            ENTRY_HITS+=(0)
            continue
        fi
        key="${line%% *}"
        value="${line#* }"
        case "$key" in
            min-returns) MIN_RETURNS="$value" ;;
            min-symbols) MIN_SYMBOLS="$value" ;;
            expect-test-registry) EXPECT_TEST_REGISTRY="$value" ;;
            *)
                echo "check_kernel_softfloat: $ALLOWLIST_FILE:$lineno: unknown directive '$key'" >&2
                exit 2
                ;;
        esac
    done < "$ALLOWLIST_FILE"

    if [ -z "$MIN_RETURNS" ] || [ -z "$MIN_SYMBOLS" ] || [ -z "$EXPECT_TEST_REGISTRY" ]; then
        echo "check_kernel_softfloat: $ALLOWLIST_FILE must set min-returns, min-symbols" >&2
        echo "  and expect-test-registry" >&2
        exit 2
    fi
}

TEST_REGISTRY_ENTRY_SIZE=104
TEST_REGISTRY_MANY=100

check_test_registry() {
    local span_hex entries

    # For the self-test's synthetic objects, which are not kernel images.
    [ "$EXPECT_TEST_REGISTRY" = "any" ] && return 0

    span_hex="$("$OBJDUMP" -h "$ELF" | awk '$2 == ".test_registry" { print $3; exit }')"
    if [ -z "$span_hex" ]; then
        echo "check_kernel_softfloat: $ELF has no .test_registry section — this is not" >&2
        echo "  a SlopOS kernel image (link.ld brackets that section unconditionally)." >&2
        exit 2
    fi
    entries=$(( 16#$span_hex / TEST_REGISTRY_ENTRY_SIZE ))

    case "$EXPECT_TEST_REGISTRY" in
        many)
            if [ "$entries" -lt "$TEST_REGISTRY_MANY" ]; then
                echo "check_kernel_softfloat: --variant $VARIANT expects a kernel/tests image," >&2
                echo "  but $ELF registers only $entries test(s). Wrong binary." >&2
                exit 2
            fi
            ;;
        few)
            if [ "$entries" -ge "$TEST_REGISTRY_MANY" ]; then
                echo "check_kernel_softfloat: --variant $VARIANT expects a non-tests image, but" >&2
                echo "  $ELF registers $entries tests — that is the kernel/tests build." >&2
                exit 2
            fi
            ;;
        *)
            echo "check_kernel_softfloat: $ALLOWLIST_FILE: expect-test-registry must be few|many|any" >&2
            exit 2
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Disassembly scan
# ---------------------------------------------------------------------------
#
# `llvm-objdump -d` emits exactly three line shapes: one file header, one
# `ADDR <SYMBOL>:` per function, and `ADDR:\tMNEMONIC\tOPERANDS` per
# instruction. Split on TAB rather than leading space — the address column is
# right-aligned to the file's widest address, so kernel lines have no leading
# whitespace and userland ones do.
#
# Two false-positive classes must be stripped first, and both bit the
# previous line-oriented `grep`: symbol names contain register names
# (`<..._fpu_xmm_roundtrip_a>:`, and branch targets annotated inline as
# `callq 0x… <_RNv…>`), and llvm-objdump appends `# imm = 0x600` /
# `# xmm0 = xmm0[0,1]` comments. Hence the `%` sigil requirement plus the two
# sub()s below.
#
# The pre-filter keeps awk off 1.6 M lines; it must pass through everything
# the classifier and the liveness floors need. `emms` gets its own
# alternative — no operands, and it does not start with `f`.
PREFILTER='^[0-9a-f]+ <|	retq$|%[xyz]mm[0-9]|%mm[0-7]|%st|	f[a-z]|	f?emms$|	[xf](save|rstor)|	v?(ld|st)mxcsr|	vzero'

scan_disassembly() {
    "$OBJDUMP" -d --no-show-raw-insn --section=.text "$ELF" \
        | grep -E "$PREFILTER" \
        | awk -F'\t' '
            /^[0-9a-f]+ <.*>:$/ {
                symbols++;
                sym = substr($0, index($0, "<") + 1);
                sub(/>:$/, "", sym);
                next;
            }
            $1 ~ /^[ ]*[0-9a-f]+: *$/ && NF >= 2 {
                mnem = $2;
                if (mnem == "retq") { returns++; next; }

                ops = (NF >= 3 ? $3 : "");
                sub(/ *#.*$/, "", ops);
                sub(/ *<[^>]*>$/, "", ops);

                cls = "";
                if (ops ~ /%[xyz]mm[0-9]/ || mnem ~ /^v?(ld|st)mxcsr$/ || mnem ~ /^vzero(upper|all)$/)
                    cls = "vec";
                else if (mnem ~ /^f[a-z0-9]*$/ || ops ~ /%st(\(|,|$)/)
                    cls = "x87";
                else if (ops ~ /%mm[0-7]/ || mnem ~ /^f?emms$/)
                    cls = "mmx";
                else if (mnem ~ /^[xf](save|rstor)/)
                    cls = "xsave";
                if (cls == "") next;

                # A hit before the first symbol header inherits no
                # exemption: the sentinel matches no real entry.
                key = cls "\t" mnem "\t" (symbols == 0 ? "<no-enclosing-symbol>" : sym);
                count[key]++;
                next;
            }
            END {
                printf "stat\tsymbols\t%d\n", symbols + 0;
                printf "stat\treturns\t%d\n", returns + 0;
                for (k in count) printf "hit\t%d\t%s\n", count[k], k;
            }'
}

# ---------------------------------------------------------------------------
# The gate
# ---------------------------------------------------------------------------

run_gate() {
    load_allowlist

    if [ -z "$ELF" ]; then
        echo "check_kernel_softfloat: no ELF given" >&2
        exit 2
    fi
    if [ ! -f "$ELF" ]; then
        echo "check_kernel_softfloat: missing $ELF (run \`just build\` first)" >&2
        exit 2
    fi

    OBJDUMP="$("$SCRIPT_DIR/llvm_tool.sh" llvm-objdump)"

    check_test_registry

    local raw symbols returns hits
    raw="$(scan_disassembly)"
    symbols="$(printf '%s\n' "$raw" | sed -n 's/^stat	symbols	//p')"
    returns="$(printf '%s\n' "$raw" | sed -n 's/^stat	returns	//p')"
    hits="$(printf '%s\n' "$raw" | sed -n 's/^hit	//p' | sort -rn || true)"

    if [ "${symbols:-0}" -lt "$MIN_SYMBOLS" ] || [ "${returns:-0}" -lt "$MIN_RETURNS" ]; then
        echo "check_kernel_softfloat: only ${symbols:-0} symbol(s) and ${returns:-0} return(s)" >&2
        echo "  decoded from $ELF (min-symbols $MIN_SYMBOLS, min-returns $MIN_RETURNS in" >&2
        echo "  $ALLOWLIST_FILE) — refusing to report OK. A zero match count means" >&2
        echo "  nothing when nothing was disassembled." >&2
        exit 2
    fi

    if [ "$EMIT_ALLOWLIST" -eq 1 ]; then
        emit_allowlist "$hits"
        return 0
    fi

    local offenders="" allowed_total=0 i matched
    local count cls mnem sym rest
    local n_vec=0 n_x87=0 n_mmx=0 n_xsave=0
    while IFS= read -r line; do
        [ -z "$line" ] && continue
        count="${line%%	*}"
        rest="${line#*	}"
        cls="${rest%%	*}"
        rest="${rest#*	}"
        mnem="${rest%%	*}"
        sym="${rest#*	}"

        case "$cls" in
            vec)   n_vec=$((   n_vec   + count )) ;;
            x87)   n_x87=$((   n_x87   + count )) ;;
            mmx)   n_mmx=$((   n_mmx   + count )) ;;
            xsave) n_xsave=$(( n_xsave + count )) ;;
        esac

        matched=0
        i=0
        while [ "$i" -lt "${#ENTRY_GLOB[@]}" ]; do
            # Class name or literal mnemonic. The mnemonic is tighter:
            # `1 x87 _start` would also excuse a `movups` landing there.
            if { [ "${ENTRY_CLASS[$i]}" = "$cls" ] || [ "${ENTRY_CLASS[$i]}" = "$mnem" ]; } \
                && [[ "$sym" == ${ENTRY_GLOB[$i]} ]]; then
                ENTRY_HITS[$i]=$(( ENTRY_HITS[i] + count ))
                allowed_total=$(( allowed_total + count ))
                matched=1
                break
            fi
            i=$(( i + 1 ))
        done
        [ "$matched" -eq 1 ] && continue
        offenders="$offenders$count	$cls	$mnem	$sym"$'\n'
    done <<< "$hits"

    local fail=0

    if [ -n "$offenders" ]; then
        echo "check_kernel_softfloat: FAIL — $ELF touches XCR0-managed state outside" >&2
        echo "  the allowlist:" >&2
        printf '%s' "$offenders" | while IFS= read -r line; do
            [ -z "$line" ] && continue
            printf '  %6s x  %-6s %-10s %s\n' \
                "$(printf '%s' "$line" | cut -f1)" \
                "$(printf '%s' "$line" | cut -f2)" \
                "$(printf '%s' "$line" | cut -f3)" \
                "$(printf '%s' "$line" | cut -f4)" >&2
        done
        echo >&2
        echo "  The kernel must be +soft-float; check targets/x86_64-slos.json features" >&2
        echo "  and rustc-abi. Hand-written asm! is not covered by target features at" >&2
        echo "  all — if that is the source, it needs an entry in $ALLOWLIST_FILE" >&2
        echo "  with a reviewed justification and a measured budget." >&2
        echo >&2
        fail=1
    fi

    local over_budget="" stale=""
    i=0
    while [ "$i" -lt "${#ENTRY_GLOB[@]}" ]; do
        if [ "${ENTRY_HITS[$i]}" -eq 0 ]; then
            stale="$stale  $ALLOWLIST_FILE:${ENTRY_LINE[$i]}: ${ENTRY_CLASS[$i]} ${ENTRY_GLOB[$i]}"$'\n'
        elif [ "${ENTRY_HITS[$i]}" -gt "${ENTRY_BUDGET[$i]}" ]; then
            over_budget="$over_budget  $ALLOWLIST_FILE:${ENTRY_LINE[$i]}: ${ENTRY_GLOB[$i]} — ${ENTRY_HITS[$i]} hits, budget ${ENTRY_BUDGET[$i]}"$'\n'
        fi
        i=$(( i + 1 ))
    done

    if [ -n "$over_budget" ]; then
        echo "check_kernel_softfloat: allowlisted symbol(s) over budget:" >&2
        printf '%s' "$over_budget" >&2
        echo "  The exemption was measured; more instructions than were reviewed now" >&2
        echo "  carry it. Re-review before raising the number." >&2
        echo >&2
        fail=1
    fi

    if [ -n "$stale" ]; then
        echo "check_kernel_softfloat: allowlist entr(ies) matching no instruction:" >&2
        printf '%s' "$stale" >&2
        echo "  The instruction is gone or the symbol was renamed. Delete the entry —" >&2
        echo "  a dead exemption is how a mis-stated --variant would otherwise pass." >&2
        echo >&2
        fail=1
    fi

    [ "$fail" -ne 0 ] && exit 1

    printf 'check_kernel_softfloat: OK — variant=%s, %s symbols / %s returns decoded\n' \
        "$VARIANT" "$symbols" "$returns"
    printf '  %d XCR0-touching instruction(s) [%d vec, %d x87, %d mmx, %d xsave], all in\n' \
        "$allowed_total" "$n_vec" "$n_x87" "$n_mmx" "$n_xsave"
    printf '  %d/%d allowlist entries; 0 unallowlisted\n' \
        "${#ENTRY_GLOB[@]}" "${#ENTRY_GLOB[@]}"
}

emit_allowlist() {
    local hits="$1"
    printf '# check_kernel_softfloat allowlist — variant: %s\n#\n' "$VARIANT"
    printf '# Emitted from %s. Review every line before committing: an emitted entry\n' "$ELF"
    printf '# records what the instruction *is*, not that it is acceptable.\n\n'
    printf 'min-returns %d\n' $(( returns / 2 ))
    printf 'min-symbols %d\n' $(( symbols / 2 ))
    printf 'expect-test-registry few\n\n'
    printf '%s\n' "$hits" | while IFS= read -r line; do
        [ -z "$line" ] && continue
        printf '%s\t%s\t%s\n' \
            "$(printf '%s' "$line" | cut -f1)" \
            "$(printf '%s' "$line" | cut -f2)" \
            "$(printf '%s' "$line" | cut -f4)"
    done
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
run_self_test() {
    local fixture_root llc probe_o fail=0
    llc="$("$SCRIPT_DIR/llvm_tool.sh" llc)"
    fixture_root="$(mktemp -d)"
    trap 'rm -rf "$fixture_root"' EXIT INT TERM

    # `volatile`, or the unused vector load/store folds to a bare `retq`.
    # `@plain` and `@fpu_xmm_roundtrip_a` are integer-only: the previous gate
    # counted the latter's *header line* as a vector instruction.
    cat > "$fixture_root/probe.ll" <<'FIXTURE'
target triple = "x86_64-unknown-none"
define void @vec_user(ptr %p) {
  %v = load volatile <4 x float>, ptr %p, align 1
  store volatile <4 x float> %v, ptr %p, align 1
  ret void
}
define void @x87_user() {
  call void asm sideeffect "fninit", ""()
  ret void
}
define void @mmx_user() {
  call void asm sideeffect "emms", ""()
  ret void
}
define void @xsave_user(ptr %p) {
  call void asm sideeffect "xsave64 ($0)", "r"(ptr %p)
  ret void
}
define i64 @plain(i64 %a) {
  %r = add i64 %a, 1
  ret i64 %r
}
define i64 @fpu_xmm_roundtrip_a(i64 %a) {
  %r = mul i64 %a, 3
  ret i64 %r
}
FIXTURE
    probe_o="$fixture_root/probe.o"
    "$llc" -mtriple=x86_64-unknown-none -mattr=+sse2 -filetype=obj \
        -o "$probe_o" "$fixture_root/probe.ll"

    write_allowlist() {
        {
            printf 'min-returns 1\n'
            printf 'min-symbols 1\n'
            printf 'expect-test-registry any\n'
            printf '%s\n' "$@"
        } > "$fixture_root/selftest.txt"
    }

    expect() {
        local label="$1" want_exit="$2" want_text="$3" elf="$4"
        local out status
        set +e
        out="$("$0" --variant selftest --gate-data-dir "$fixture_root" "$elf" 2>&1)"
        status=$?
        set -e
        if [ "$status" -ne "$want_exit" ]; then
            echo "check_kernel_softfloat --self-test: $label — expected exit $want_exit, got $status" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            fail=1
            return
        fi
        if [ -n "$want_text" ] && ! printf '%s\n' "$out" | grep -qF "$want_text"; then
            echo "check_kernel_softfloat --self-test: $label — output did not mention '$want_text'" >&2
            printf '%s\n' "$out" | sed 's/^/      /' >&2
            fail=1
            return
        fi
        echo "  $label: ok"
    }

    echo "check_kernel_softfloat: self-test against synthesised objects"

    write_allowlist
    expect "a compiler-emitted vector instruction is rejected" 1 "vec_user" "$probe_o"
    expect "an x87 instruction is rejected" 1 "x87_user" "$probe_o"
    expect "an MMX instruction is rejected" 1 "mmx_user" "$probe_o"
    expect "an unreviewed xsave is rejected" 1 "xsave_user" "$probe_o"

    # Regression test for the symbol-name false positive.
    local out
    out="$("$0" --variant selftest --gate-data-dir "$fixture_root" "$probe_o" 2>&1 || true)"
    if printf '%s\n' "$out" | grep -qE '^ +[0-9]+ x .*(plain|fpu_xmm_roundtrip_a)'; then
        echo "check_kernel_softfloat --self-test: an integer-only symbol was reported" >&2
        printf '%s\n' "$out" | sed 's/^/      /' >&2
        fail=1
    else
        echo "  a symbol merely *named* xmm is not a hit: ok"
    fi

    # The gate must be able to pass, and the budget must bite.
    write_allowlist \
        "$(printf '2\tvec\tvec_user')" \
        "$(printf '1\tfninit\tx87_user')" \
        "$(printf '1\tmmx\tmmx_user')" \
        "$(printf '1\txsave\txsave_user')"
    expect "measured budgets are accepted" 0 "OK" "$probe_o"

    write_allowlist \
        "$(printf '1\tvec\tvec_user')" \
        "$(printf '1\tfninit\tx87_user')" \
        "$(printf '1\tmmx\tmmx_user')" \
        "$(printf '1\txsave\txsave_user')"
    expect "a budget below the measured count fails" 1 "over budget" "$probe_o"

    # One class's exemption must not cover another class's instruction.
    write_allowlist \
        "$(printf '9\tx87\tvec_user')" \
        "$(printf '1\tfninit\tx87_user')" \
        "$(printf '1\tmmx\tmmx_user')" \
        "$(printf '1\txsave\txsave_user')"
    expect "an x87 exemption does not excuse a vector instruction" 1 "vec_user" "$probe_o"

    write_allowlist \
        "$(printf '2\tvec\tvec_user')" \
        "$(printf '1\tfninit\tx87_user')" \
        "$(printf '1\tmmx\tmmx_user')" \
        "$(printf '1\txsave\txsave_user')" \
        "$(printf '1\tvec\t*no_such_symbol*')"
    expect "stale allowlist entry is rejected" 1 "no_such_symbol" "$probe_o"

    # A zero match count means nothing if nothing decoded.
    {
        printf 'min-returns 99999\n'
        printf 'min-symbols 1\n'
        printf 'expect-test-registry any\n'
    } > "$fixture_root/selftest.txt"
    expect "decode-liveness floor is enforced" 2 "refusing to report OK" "$probe_o"

    if "$SCRIPT_DIR/llvm_tool.sh" llvm-not-a-real-tool >/dev/null 2>&1; then
        echo "check_kernel_softfloat --self-test: llvm_tool.sh resolved a nonexistent tool" >&2
        fail=1
    else
        echo "  missing tool fails closed: ok"
    fi

    rm -rf "$fixture_root"
    trap - EXIT INT TERM

    if [ "$fail" -ne 0 ]; then
        echo "check_kernel_softfloat: SELF-TEST FAILED — the gate cannot be trusted to reject" >&2
        exit 1
    fi
    echo "check_kernel_softfloat: self-test OK"
}

if [ "$SELF_TEST" -eq 1 ]; then
    run_self_test
    exit 0
fi

if [ -z "$VARIANT" ]; then
    echo "check_kernel_softfloat: --variant is required (dev | release | tests)" >&2
    echo "  It selects the measured allowlist, and a mis-stated variant fails" >&2
    echo "  rather than passing on the wrong one." >&2
    exit 2
fi

run_gate
