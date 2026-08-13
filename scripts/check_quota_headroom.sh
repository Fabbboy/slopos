#!/usr/bin/env bash
# Resource-account headroom ratchet.
#
# Boots the test ISO, parses every
#   QUOTA[<phase>]: mode=M slot=S kind=K used=U peak=P limit=L denials=D
# line, and fails if a required phase is missing, if a peak exceeds its
# recorded cap, or if a denial was recorded where the gate expects none.
#
# The numbers here are **measured, never chosen**. Deriving an enforced runtime
# default from a boot-time observation is how Linux shipped limits that could
# not subsequently be raised, so there are deliberately two numbers per kind in
# two places: the enforced default lives in the kernel, and the gate ceiling
# lives here. This file is the second one.
#
# `peak` and not `used`: a dump-time `used` samples whatever happens to be live
# at that instant, which is not the high-water mark a ceiling has to be derived
# from. A peak of zero therefore fails — it means the kind was never exercised,
# and a cap set from it would be a cap on nothing.
#
#     scripts/check_quota_headroom.sh
#     scripts/check_quota_headroom.sh --log captured-raw.log
#     scripts/check_quota_headroom.sh --emit-allowlist
#     scripts/check_quota_headroom.sh --self-test
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VARIANT=tests
LOG=""
EMIT=0
SELF_TEST=0
GATE_DATA_DIR="$REPO_ROOT/scripts/gates/quota"
while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --emit-allowlist) EMIT=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --gate-data-dir) GATE_DATA_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,23p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# The one line the whole gate reads, matched once and in full. A per-field
# `sed` would echo its input unchanged when the pattern missed, so a renamed
# field would yield a whole log line where an integer was expected.
QUOTA_RE='^QUOTA\[([a-z-]+)\]: mode=([a-z]+) slot=([0-9]+) kind=([a-z]+) used=([0-9]+) peak=([0-9]+) limit=(-?[0-9]+) denials=([0-9]+)'

# phase/kind -> "maxpeak totaldenials mode". Rows are per-process; the cap is
# on the worst single row of a kind, because that is what a per-principal
# ceiling has to clear.
declare -A PEAK=()
declare -A DENIALS=()
declare -A MODE=()
declare -A SEEN_PHASE=()

parse_log() {
    local log="$1" raw key phase kind peak denials
    local lines
    lines=$(grep -cE 'QUOTA\[[a-z-]+\]:' "$log" || true)
    if [ "$lines" -eq 0 ]; then
        echo "FAIL: no QUOTA[...] line was emitted — nothing was measured." >&2
        echo "      The kernel did not reach quota_report (boot panic, missing ISO," >&2
        echo "      a renamed report line, or no charge was ever taken)." >&2
        return 1
    fi
    while IFS= read -r raw; do
        raw="${raw%$'\r'}"
        if [[ ! "$raw" =~ $QUOTA_RE ]]; then
            echo "FAIL: could not parse a QUOTA line — the report format moved." >&2
            echo "      line: $raw" >&2
            echo "      want: QUOTA[<phase>]: mode=M slot=S kind=K used=U peak=P limit=L denials=D" >&2
            echo "      Update quota_report and this gate together." >&2
            return 1
        fi
        phase="${BASH_REMATCH[1]}"
        kind="${BASH_REMATCH[4]}"
        peak="${BASH_REMATCH[6]}"
        denials="${BASH_REMATCH[8]}"
        key="$phase/$kind"
        SEEN_PHASE["$phase"]=1
        MODE["$phase"]="${BASH_REMATCH[2]}"
        if [ -z "${PEAK[$key]+x}" ] || [ "$peak" -gt "${PEAK[$key]}" ]; then
            PEAK["$key"]=$peak
        fi
        DENIALS["$key"]=$(( ${DENIALS[$key]:-0} + denials ))
    done < <(grep -oE 'QUOTA\[[a-z-]+\]:.*' "$log")
}

emit_gate_data() {
    local key phase
    echo "# check_quota_headroom gate data — variant: $VARIANT"
    echo "#"
    echo "#     scripts/check_quota_headroom.sh --variant $VARIANT --emit-allowlist"
    echo "#"
    echo "# One line per <phase> <TAB> <kind> <TAB> <cap>, where the cap is the"
    echo "# highest peak any single account row reached for that kind. Measured,"
    echo "# never chosen: raise a cap only with a fresh --emit-allowlist in the"
    echo "# same commit, and say in the message what started consuming more."
    echo "#"
    echo "# A cap matching nothing is a dead entry and fails, because a kind that"
    echo "# stopped being reported looks exactly like a kind that stayed cheap."
    echo
    echo "# The peak of a kind nobody exercised is zero, and a cap derived from"
    echo "# it would bound nothing. Every listed kind must have been reached."
    echo "min-kinds 2"
    echo
    echo "# A denial is an over-limit charge. Under quota=warn it is granted and"
    echo "# counted, so a non-zero total means the enforced tier would have"
    echo "# refused something — which is a finding, not a pass."
    echo "max-denials 0"
    echo
    for phase in $(printf '%s\n' "${!SEEN_PHASE[@]}" | sort); do
        echo "require-phase $phase"
    done
    echo
    for key in $(printf '%s\n' "${!PEAK[@]}" | sort); do
        printf '%s\t%s\t%s\n' "${key%%/*}" "${key##*/}" "${PEAK[$key]}"
    done
}

run_gate() {
    local gate="$GATE_DATA_DIR/$VARIANT.txt"

    if [ ! -f "$gate" ]; then
        echo "check_quota_headroom: no gate data at $gate" >&2
        echo "  Every gated variant needs its own measured baseline. Create it with:" >&2
        echo "      scripts/check_quota_headroom.sh --variant $VARIANT --emit-allowlist" >&2
        return 2
    fi

    local MIN_KINDS=0 MAX_DENIALS=0
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
            min-kinds)    MIN_KINDS=$(awk '{print $2}' <<<"$line") ;;
            max-denials)  MAX_DENIALS=$(awk '{print $2}' <<<"$line") ;;
            require-phase) REQUIRED+=("$(awk '{print $2}' <<<"$line")") ;;
            *)
                echo "check_quota_headroom: $gate:$lineno: unknown directive '$key'" >&2
                return 2
                ;;
        esac
    done < "$gate"

    local fail=0 phase kinds gkey

    for phase in "${REQUIRED[@]}"; do
        if [ -z "${SEEN_PHASE[$phase]+x}" ]; then
            echo "FAIL: gate requires phase '$phase' but the boot never printed it." >&2
            echo "      A phase that stopped reporting looks exactly like a phase that passed." >&2
            fail=1
            continue
        fi
        kinds=0
        for gkey in "${!PEAK[@]}"; do
            [ "${gkey%%/*}" = "$phase" ] && kinds=$((kinds + 1))
        done
        if [ "$kinds" -lt "$MIN_KINDS" ]; then
            echo "FAIL: QUOTA[$phase] reported only $kinds kind(s) (min $MIN_KINDS) — nothing measured." >&2
            fail=1
        fi
    done

    for gkey in "${!PEAK[@]}"; do
        phase="${gkey%%/*}"
        # Only phases the gate asked about are held to a cap; a boot that
        # reports an extra phase is not a failure.
        case " ${REQUIRED[*]} " in *" $phase "*) ;; *) continue ;; esac

        if [ "${PEAK[$gkey]}" -eq 0 ]; then
            echo "FAIL: $gkey peaked at 0 — the kind was never exercised, so a cap on it bounds nothing." >&2
            fail=1
        fi
        if [ "${DENIALS[$gkey]:-0}" -gt "$MAX_DENIALS" ]; then
            echo "FAIL: $gkey recorded ${DENIALS[$gkey]} denial(s), over max-denials $MAX_DENIALS." >&2
            echo "      Under quota=warn these were granted; under quota=enforce they would refuse." >&2
            fail=1
        fi
        if [ -n "${CAPS[$gkey]+x}" ]; then
            if [ "${PEAK[$gkey]}" -gt "${CAPS[$gkey]}" ]; then
                echo "FAIL: $gkey peaked at ${PEAK[$gkey]}, over the recorded cap ${CAPS[$gkey]}." >&2
                echo "      Re-measure with --emit-allowlist and say what started consuming more." >&2
                fail=1
            fi
            unset "CAPS[$gkey]"
        fi
    done

    # A cap matching nothing is a dead entry: it stops describing the kernel and
    # would silently keep passing after the kind it names stopped being charged.
    for gkey in "${!CAPS[@]}"; do
        echo "FAIL: gate entry '$gkey' matched no observed phase/kind — dead entry, delete it." >&2
        fail=1
    done

    [ "$fail" -eq 0 ] || return 1

    for phase in "${REQUIRED[@]}"; do
        for gkey in $(printf '%s\n' "${!PEAK[@]}" | sort); do
            [ "${gkey%%/*}" = "$phase" ] || continue
            echo "OK: $gkey peak=${PEAK[$gkey]} denials=${DENIALS[$gkey]:-0} (mode=${MODE[$phase]})"
        done
    done
}

# ---------------------------------------------------------------------------
# Self-test
#
# A gate that has never been observed to reject has not been observed to work.
# `--log` bypasses QEMU, so every case is a crafted log plus a crafted gate
# file — no boot, well under a second for the set.
# ---------------------------------------------------------------------------
self_test() {
    local tmp out rc failures=0
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/gates"

    _line() {
        printf 'QUOTA[%s]: mode=%s slot=%s kind=%s used=%s peak=%s limit=%s denials=%s\r\n' \
            "$1" "$2" "$3" "$4" "$5" "$6" "$7" "$8"
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
            return
        fi
        echo "  $label"
    }

    local clean="$tmp/clean.log"
    {
        _line post-userland-tests warn 0 fdslot 3 257 -1 0
        _line post-userland-tests warn 7 fdslot 0 18 -1 0
        _line post-userland-tests warn 0 objectrow 1 257 -1 0
    } > "$clean"

    local good_gate='min-kinds 2
max-denials 0
require-phase post-userland-tests
post-userland-tests	fdslot	257
post-userland-tests	objectrow	257
'

    # 1. The positive control. Without it every rejection below could be a
    #    gate that rejects unconditionally.
    printf '%s' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 0 "OK: post-userland-tests/fdslot" "$clean" "clean log accepted"

    # 2. peak == cap exactly must pass: a cap is a ceiling, not a strict bound.
    printf 'min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\npost-userland-tests\tobjectrow\t257\n' > "$tmp/gates/$VARIANT.txt"
    _expect 0 "OK:" "$clean" "peak == cap accepted"

    # 3. Nothing measured at all.
    : > "$tmp/empty.log"
    _expect 1 "nothing was measured" "$tmp/empty.log" "empty log rejected"

    # 4. A phase that stopped reporting.
    printf 'min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\nrequire-phase boot\npost-userland-tests\tfdslot\t257\npost-userland-tests\tobjectrow\t257\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "never printed it" "$clean" "missing phase rejected"

    # 5. A cap naming a kind nobody reports.
    printf '%sposte\tghost\t1\n' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "dead entry" "$clean" "dead entry rejected"

    # 6. A peak over its cap.
    printf 'min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t256\npost-userland-tests\tobjectrow\t257\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "over the recorded cap" "$clean" "over-cap rejected"

    # 7. A peak of zero: the kind was never exercised.
    {
        _line post-userland-tests warn 0 fdslot 0 0 -1 0
        _line post-userland-tests warn 0 objectrow 1 257 -1 0
    } > "$tmp/zero.log"
    printf '%s' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "peaked at 0" "$tmp/zero.log" "zero peak rejected"

    # 8. A denial recorded where the gate expects none.
    {
        _line post-userland-tests warn 0 fdslot 3 257 -1 0
        _line post-userland-tests warn 7 fdslot 0 18 64 4
        _line post-userland-tests warn 0 objectrow 1 257 -1 0
    } > "$tmp/denied.log"
    printf '%s' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "denial(s), over max-denials" "$tmp/denied.log" "denial rejected"

    # 9. Fewer kinds than the floor: a report that shrank to nothing.
    _line post-userland-tests warn 0 fdslot 3 257 -1 0 > "$tmp/onekind.log"
    printf 'min-kinds 2\nmax-denials 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "min 2" "$tmp/onekind.log" "min-kinds floor rejected"

    # 10. An unparseable line — the format moved and the gate must say so
    #     rather than reading a log line as an integer.
    printf 'QUOTA[post-userland-tests]: slot=0 kind=fdslot high_water=257\r\n' > "$tmp/moved.log"
    printf '%s' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "the report format moved" "$tmp/moved.log" "renamed field rejected"

    # 11. An unknown directive, which must be an error rather than ignored: a
    #     typo'd cap would otherwise silently bound nothing.
    printf 'min-kinds 2\nmax-denialz 0\nrequire-phase post-userland-tests\npost-userland-tests\tfdslot\t257\n' > "$tmp/gates/$VARIANT.txt"
    _expect 2 "unknown directive" "$clean" "unknown directive rejected"

    # 12. No gate data at all for the variant.
    rm -f "$tmp/gates/$VARIANT.txt"
    _expect 2 "no gate data" "$clean" "missing gate data rejected"

    if [ "$failures" -ne 0 ]; then
        echo "check_quota_headroom: SELF-TEST FAILED ($failures case(s))" >&2
        exit 1
    fi
    echo "check_quota_headroom: self-test OK"
    exit 0
}

[ "$SELF_TEST" -eq 1 ] && self_test

if [ -z "$LOG" ]; then
    LOG="$(mktemp)"
    trap 'rm -f "$LOG"' EXIT INT TERM
    builddir/run_tests --raw --no-color > "$LOG" 2>&1 || true
fi

parse_log "$LOG"

if [ "$EMIT" -eq 1 ]; then
    emit_gate_data
    exit 0
fi

run_gate
