#!/usr/bin/env bash
# Lockdep pool-headroom ratchet.
#
# Boots the test ISO, parses every
#   LOCKDEP[<phase>]: <state> classes=N/C (P%) edges=E/EC chains=H/HC ...
# line, and fails if any required phase is missing or not ACTIVE, if a
# violation was reported, or if a pool exceeds its recorded cap or the
# gate file's max-fill-pct.
#
# The gate data lives in scripts/gates/lockdep/<variant>.txt so weakening a
# check is a diff on a tracked file rather than an edit to this script. A
# directive naming a phase the boot never printed FAILS: a phase that stopped
# reporting looks exactly like a phase that passed.
#
#     scripts/check_lockdep_headroom.sh
#     scripts/check_lockdep_headroom.sh --log captured-raw.log
#     scripts/check_lockdep_headroom.sh --emit-allowlist
#     scripts/check_lockdep_headroom.sh --self-test
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VARIANT=tests
LOG=""
EMIT=0
SELF_TEST=0
GATE_DATA_DIR="$REPO_ROOT/scripts/gates/lockdep"
while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --emit-allowlist) EMIT=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --gate-data-dir) GATE_DATA_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# The one line the whole gate reads, matched once and in full. Per-field
# `sed 's/.*X=\([0-9]*\).*/\1/'` echoed its input unchanged when the pattern
# missed, so a renamed field yielded a whole log line where an integer was
# expected and the script died in arithmetic instead of saying the line did not
# parse. A state word may carry a parenthetical ("OFF (lockdep=off)"), so the
# run-up to `classes=` is matched as "contains no equals sign".
LOCKDEP_RE='^LOCKDEP\[([a-z-]+)\]: ([A-Z]+)[^=]*classes=([0-9]+)/([0-9]+) \(([0-9]+)%\) edges=([0-9]+)/([0-9]+) chains=([0-9]+)/([0-9]+).*violations=([0-9]+)'

declare -A OBSERVED=()

parse_log() {
    local log="$1" raw
    local lines
    lines=$(grep -cE 'LOCKDEP\[[a-z-]+\]:' "$log" || true)
    if [ "$lines" -eq 0 ]; then
        echo "FAIL: no LOCKDEP[...] line was emitted — nothing was measured." >&2
        echo "      The kernel did not reach kdiag_dump_lock_graph (boot panic," >&2
        echo "      missing ISO, or a renamed report line)." >&2
        return 1
    fi
    while IFS= read -r raw; do
        # Strip the CR the serial console leaves on every line.
        raw="${raw%$'\r'}"
        if [[ ! "$raw" =~ $LOCKDEP_RE ]]; then
            echo "FAIL: could not parse a LOCKDEP line — the report format moved." >&2
            echo "      line: $raw" >&2
            echo "      want: LOCKDEP[<phase>]: <STATE> classes=N/C (P%) edges=E/EC chains=H/HC ... violations=V" >&2
            echo "      Update kdiag_dump_lock_graph and this gate together." >&2
            return 1
        fi
        # state classes edges chains violations pool_c pool_e pool_h
        OBSERVED[${BASH_REMATCH[1]}]="${BASH_REMATCH[2]} ${BASH_REMATCH[3]} ${BASH_REMATCH[6]} ${BASH_REMATCH[8]} ${BASH_REMATCH[10]} ${BASH_REMATCH[4]} ${BASH_REMATCH[7]} ${BASH_REMATCH[9]}"
    done < <(grep -oE 'LOCKDEP\[[a-z-]+\]:.*' "$log")
}

emit_gate_data() {
    local phase c e h
    echo "# check_lockdep_headroom gate data — variant: $VARIANT"
    echo "#"
    echo "#     scripts/check_lockdep_headroom.sh --variant $VARIANT --emit-allowlist"
    echo "#"
    echo "# Emitted from one boot. Edges and chains move with scheduling, so"
    echo "# review every cap against several runs before committing: a cap set to"
    echo "# a single observation of a varying quantity fails on interleaving."
    echo
    echo "min-classes 32"
    echo "max-fill-pct 70"
    echo
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        echo "require-phase $phase"
    done
    echo
    echo "# <phase> <TAB> <pool> <TAB> <cap>"
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        read -r _ c e h _ _ _ _ <<<"${OBSERVED[$phase]}"
        printf '%s\tclasses\t%s\n%s\tedges\t%s\n%s\tchains\t%s\n' \
            "$phase" "$c" "$phase" "$e" "$phase" "$h"
    done
}

run_gate() {
    local gate="$GATE_DATA_DIR/$VARIANT.txt"

    if [ ! -f "$gate" ]; then
        echo "check_lockdep_headroom: no gate data at $gate" >&2
        echo "  Every gated variant needs its own measured baseline. Create it with:" >&2
        echo "      scripts/check_lockdep_headroom.sh --variant $VARIANT --emit-allowlist" >&2
        return 2
    fi

    local MIN_CLASSES=0 MAX_FILL=100
    local -a REQUIRED=()
    declare -A CAPS=()
    local lineno=0 line key
    while IFS= read -r line || [ -n "$line" ]; do
        lineno=$((lineno + 1))
        line="${line%%#*}"
        [ -z "${line// }" ] && continue
        if [[ "$line" == *$'\t'* ]]; then
            CAPS["$(awk -F'\t' '{print $1"/"$2}' <<<"$line")"]=$(awk -F'\t' '{print $3}' <<<"$line")
            continue
        fi
        key="${line%% *}"
        case "$key" in
            min-classes) MIN_CLASSES=$(awk '{print $2}' <<<"$line") ;;
            max-fill-pct) MAX_FILL=$(awk '{print $2}' <<<"$line") ;;
            require-phase) REQUIRED+=("$(awk '{print $2}' <<<"$line")") ;;
            *)
                echo "check_lockdep_headroom: $gate:$lineno: unknown directive '$key'" >&2
                return 2
                ;;
        esac
    done < "$gate"

    local fail=0 phase state c e h viol pool_c pool_e pool_h pool val pool_size pct gkey
    for phase in "${REQUIRED[@]}"; do
        if [ -z "${OBSERVED[$phase]+x}" ]; then
            echo "FAIL: gate requires phase '$phase' but the boot never printed it." >&2
            fail=1
            continue
        fi
        read -r state c e h viol pool_c pool_e pool_h <<<"${OBSERVED[$phase]}"
        if [ "$state" != "ACTIVE" ]; then
            echo "FAIL: LOCKDEP[$phase] is $state, not ACTIVE — locks are unvalidated." >&2
            fail=1
        fi
        if [ "$viol" -ne 0 ]; then
            echo "FAIL: LOCKDEP[$phase] reported $viol violation(s) with the panic suppressed." >&2
            fail=1
        fi
        if [ "$c" -lt "$MIN_CLASSES" ]; then
            echo "FAIL: LOCKDEP[$phase] registered only $c classes (min $MIN_CLASSES) — nothing measured." >&2
            fail=1
        fi
        for pool in classes edges chains; do
            case "$pool" in
                classes) val=$c; pool_size=$pool_c ;;
                edges)   val=$e; pool_size=$pool_e ;;
                chains)  val=$h; pool_size=$pool_h ;;
            esac
            pct=$(( val * 100 / pool_size ))
            if [ "$pct" -gt "$MAX_FILL" ]; then
                echo "FAIL: LOCKDEP[$phase] $pool pool ${pct}% full ($val/$pool_size), over max-fill-pct $MAX_FILL." >&2
                fail=1
            fi
            gkey="$phase/$pool"
            if [ -n "${CAPS[$gkey]+x}" ]; then
                if [ "$val" -gt "${CAPS[$gkey]}" ]; then
                    echo "FAIL: LOCKDEP[$phase] $pool grew to $val, over the recorded cap ${CAPS[$gkey]}." >&2
                    echo "      Re-measure with --emit-allowlist and say what added the locks." >&2
                    fail=1
                fi
                unset "CAPS[$gkey]"
            fi
        done
    done

    # A cap matching nothing is a dead entry: it stops describing the kernel and
    # would silently keep passing after the phase it names disappeared.
    for gkey in "${!CAPS[@]}"; do
        echo "FAIL: gate entry '$gkey' matched no observed phase/pool — dead entry, delete it." >&2
        fail=1
    done

    [ "$fail" -eq 0 ] || return 1

    for phase in "${REQUIRED[@]}"; do
        read -r state c e h _ pool_c pool_e pool_h <<<"${OBSERVED[$phase]}"
        echo "OK: LOCKDEP[$phase] $state classes=$c/$pool_c edges=$e/$pool_e chains=$h/$pool_h"
    done
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
#
# A gate that has never been observed to reject has not been observed to work.
# `--log` already bypasses QEMU, so every case is a crafted log plus a crafted
# gate file — no boot, well under a second for the set.

self_test() {
    local tmp out rc failures=0
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/gates"

    _line() {
        printf 'LOCKDEP[%s]: %s classes=%s/508 (%s%%) edges=%s/4096 chains=%s/2048 held_max=3/16 held_drops=0 pop_miss=0/0 chain_hit=8 chain_miss=1 violations=%s reports=0 collisions=0 mode=Panic\r\n' \
            "$1" "$2" "$3" "$4" "$5" "$6" "$7"
    }

    _expect() {
        local want_rc="$1" want_msg="$2" log="$3" label="$4"
        set +e
        out=$( "$0" --variant "$VARIANT" --gate-data-dir "$tmp/gates" --log "$log" 2>&1 )
        rc=$?
        set -e
        if [ "$rc" -ne "$want_rc" ]; then
            echo "SELF-TEST FAIL [$label]: exit $rc, want $want_rc" >&2
            sed 's/^/    /' <<<"$out" >&2
            failures=$((failures + 1))
            return
        fi
        if [ -n "$want_msg" ] && ! grep -qF "$want_msg" <<<"$out"; then
            echo "SELF-TEST FAIL [$label]: output missing '$want_msg'" >&2
            sed 's/^/    /' <<<"$out" >&2
            failures=$((failures + 1))
        fi
    }

    _line boot ACTIVE 65 12 48 112 0 > "$tmp/clean.log"

    # The positive control. Without it every rejection below could be a gate
    # that rejects unconditionally.
    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\n' > "$tmp/gates/$VARIANT.txt"
    _expect 0 "OK: LOCKDEP[boot]" "$tmp/clean.log" "clean log accepted"

    : > "$tmp/empty.log"
    _expect 1 "nothing was measured" "$tmp/empty.log" "empty log rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nrequire-phase post-userland-tests\nboot\tclasses\t65\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "never printed it" "$tmp/clean.log" "missing phase rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nghost\tedges\t1\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "dead entry" "$tmp/clean.log" "dead entry rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t64\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "over the recorded cap" "$tmp/clean.log" "over-cap rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\n' > "$tmp/gates/$VARIANT.txt"
    _line boot ACTIVE 400 78 48 112 0 > "$tmp/full.log"
    _expect 1 "over max-fill-pct" "$tmp/full.log" "fill ceiling rejected"

    # A validator that turned itself off still reports; it must not pass.
    _line boot "DISABLED (pool overflow)" 65 12 48 112 0 > "$tmp/disabled.log"
    _expect 1 "not ACTIVE" "$tmp/disabled.log" "inactive validator rejected"

    _line boot ACTIVE 65 12 48 112 3 > "$tmp/viol.log"
    _expect 1 "violation(s)" "$tmp/viol.log" "violation rejected"

    _line boot ACTIVE 4 1 48 112 0 > "$tmp/tiny.log"
    _expect 1 "nothing measured" "$tmp/tiny.log" "class floor rejected"

    # A renamed field must say so rather than die in arithmetic.
    printf 'LOCKDEP[boot]: ACTIVE klasses=65/508 (12%%) edges=48/4096 chains=112/2048 violations=0\r\n' > "$tmp/renamed.log"
    _expect 1 "the report format moved" "$tmp/renamed.log" "unparseable line rejected"

    if [ "$failures" -ne 0 ]; then
        echo "check_lockdep_headroom: SELF-TEST FAILED ($failures case(s)) — the gate cannot be trusted to reject." >&2
        return 1
    fi
    echo "check_lockdep_headroom: self-test OK — 10 cases, 1 accept + 9 rejects."
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
    exit $?
fi

if [ -z "$LOG" ]; then
    if [ ! -x builddir/run_tests ]; then
        echo "FAIL: builddir/run_tests is not built — run 'just check-lockdep-headroom'." >&2
        exit 1
    fi
    LOG="$(mktemp)"
    trap 'rm -f "$LOG"' EXIT INT TERM
    # Failures are diagnosed from the parsed counters, not the exit status:
    # a suite that fails a test still produces valid LOCKDEP lines.
    builddir/run_tests --raw --no-color > "$LOG" 2>&1 || true
fi

parse_log "$LOG"

if [ "$EMIT" -eq 1 ]; then
    emit_gate_data
    exit 0
fi

run_gate
