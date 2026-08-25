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
COST_RE='^QUOTACOST: depth=([0-9]+) cycles_per_charge=([0-9]+)'
REF_RE='^QUOTACOST: reference cycles_per_op=([0-9]+)'

# phase/kind -> "maxpeak totaldenials mode". Rows are per-process; the cap is
# on the worst single row of a kind, because that is what a per-principal
# ceiling has to clear.
declare -A PEAK=()
declare -A DENIALS=()
declare -A MODE=()
declare -A SEEN_PHASE=()
# depth -> worst observed cycles per charge+refund round trip.
declare -A COST=()
# Cost of one bare CAS round trip in the same run: the scale the two above are
# read against.
REF_COST=0

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

    # The charge path's own cost. Escort measured end-to-end kernel accounting
    # at 8 % throughput and 15-50 % under load; without a floor here a
    # regression of that size passes every other check in this file.
    local cost_line depth cycles
    while IFS= read -r cost_line; do
        cost_line="${cost_line%$'\r'}"
        if [[ ! "$cost_line" =~ $COST_RE ]]; then
            echo "FAIL: could not parse a QUOTACOST line \u2014 the report format moved." >&2
            echo "      line: $cost_line" >&2
            return 1
        fi
        depth="${BASH_REMATCH[1]}"
        cycles="${BASH_REMATCH[2]}"
        if [ -z "${COST[$depth]+x}" ] || [ "$cycles" -gt "${COST[$depth]}" ]; then
            COST["$depth"]=$cycles
        fi
    done < <(grep -oE 'QUOTACOST: depth=[0-9]+ cycles_per_charge=[0-9]+' "$log" || true)

    local ref_line
    while IFS= read -r ref_line; do
        ref_line="${ref_line%$'\r'}"
        if [[ ! "$ref_line" =~ $REF_RE ]]; then
            echo "FAIL: could not parse a QUOTACOST reference line — the report format moved." >&2
            echo "      line: $ref_line" >&2
            return 1
        fi
        if [ "${BASH_REMATCH[1]}" -gt "$REF_COST" ]; then
            REF_COST="${BASH_REMATCH[1]}"
        fi
    done < <(grep -oE 'QUOTACOST: reference cycles_per_op=[0-9]+' "$log" || true)
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
    # Only for a phase this run actually reported: the check path sweeps
    # `min-kinds-for` for dead entries, so emitting it unconditionally produced
    # a file the emitter's own output could not pass.
    if [ -n "${SEEN_PHASE[boot]+x}" ]; then
        echo "min-kinds-for boot 1"
    fi
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
    echo
    echo "# The charge path's cost, as ratios rather than cycle counts. A cycle"
    echo "# count here measures the accelerator: the same tree reports ~1500"
    echo "# cycles per charge under KVM and ~20000 under TCG, and both are"
    echo "# correct. Every number below is x100, so 420 means 4.20."
    echo "#"
    echo "# The reference floors are emitted at half the observed value, because"
    echo "# a bare CAS is relatively dearer under TCG than natively and this"
    echo "# ratio does move with the accelerator. They prove the measurement"
    echo "# happened; the depth ratio below is the check."
    local ref="$REF_COST"
    [ "$ref" -gt 0 ] || ref=1
    for depth in $(printf '%s\n' "${!COST[@]}" | sort -n); do
        printf 'min-charge-over-reference %s %s\n' "$depth" \
            "$(( COST[$depth] * 100 / ref / 2 ))"
    done
    local shallow="${COST[1]:-0}" deepest=0 deep_depth=0
    for depth in $(printf '%s\n' "${!COST[@]}" | sort -n); do
        if [ "$depth" -gt "$deep_depth" ]; then
            deep_depth="$depth"
            deepest="${COST[$depth]}"
        fi
    done
    if [ "$shallow" -gt 0 ] && [ "$deep_depth" -gt 1 ]; then
        # A quarter above the observation: emitting it exactly makes the
        # emitter's own output red on the very next run, so the documented
        # remedy for a ratchet failure would not survive being used.
        printf 'max-depth-cost-ratio %s %s\n' "$deep_depth" \
            "$(( deepest * 125 / shallow ))"
    fi
    echo
    echo "# A physical bound on the reference, not a measurement."
    echo "min-reference-cycles 20"
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
    declare -A COST_FLOOR=()
    local DEPTH_RATIO_DEPTH=0 DEPTH_RATIO_CAP=0 MIN_REFERENCE=0
    declare -A MIN_KINDS_FOR=()
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
            min-kinds-for)
                MIN_KINDS_FOR["$(awk '{print $2}' <<<"$line")"]=$(awk '{print $3}' <<<"$line") ;;
            max-denials)  MAX_DENIALS=$(awk '{print $2}' <<<"$line") ;;
            min-charge-over-reference)
                COST_FLOOR["$(awk '{print $2}' <<<"$line")"]=$(awk '{print $3}' <<<"$line") ;;
            min-reference-cycles)
                MIN_REFERENCE=$(awk '{print $2}' <<<"$line") ;;
            max-depth-cost-ratio)
                DEPTH_RATIO_DEPTH=$(awk '{print $2}' <<<"$line")
                DEPTH_RATIO_CAP=$(awk '{print $3}' <<<"$line") ;;
            require-phase) REQUIRED+=("$(awk '{print $2}' <<<"$line")") ;;
            *)
                echo "check_quota_headroom: $gate:$lineno: unknown directive '$key'" >&2
                return 2
                ;;
        esac
    done < "$gate"

    local fail=0 phase kinds gkey ratio

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
        # A per-phase floor overrides the global one. Boot legitimately
        # reports fewer kinds than the test phases: the only charges taken
        # before userland exists are the idle tasks' stacks.
        local floor="${MIN_KINDS_FOR[$phase]:-$MIN_KINDS}"
        if [ "$kinds" -lt "$floor" ]; then
            echo "FAIL: QUOTA[$phase] reported only $kinds kind(s) (min $floor) — nothing measured." >&2
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

    # A floor, not a cap. The reference cannot carry a ceiling: a bare CAS is
    # relatively far more expensive under TCG than natively, so the same tree
    # reports ~18x here and would report far more under KVM. What it can do is
    # prove the measurement happened and that the charge path is still doing
    # atomic work — a collapsed number is the "stopped measuring reads as
    # healthy" hole, and it is well below any accelerator's honest value.
    if [ "${#COST_FLOOR[@]}" -gt 0 ] && [ "$REF_COST" -le 0 ]; then
        echo "FAIL: the boot printed no QUOTACOST reference line, so no cost ratio can be formed." >&2
        echo "      A cost nobody measures is how an 8 % throughput regression passes." >&2
        fail=1
    fi
    # The floors above are ratios over this, so a reference that collapsed
    # satisfies all of them. A `lock cmpxchg` round trip is not this cheap on
    # any real or emulated x86, so the bound is physical rather than measured.
    if [ "$MIN_REFERENCE" -gt 0 ] && [ "$REF_COST" -gt 0 ] \
        && [ "$REF_COST" -lt "$MIN_REFERENCE" ]; then
        echo "FAIL: the QUOTACOST reference measured $REF_COST cycles, under the floor $MIN_REFERENCE." >&2
        echo "      A CAS round trip cannot be that cheap; the reference measurement collapsed," >&2
        echo "      and every ratio taken against it is meaningless." >&2
        fail=1
    fi
    for depth in "${!COST_FLOOR[@]}"; do
        if [ -z "${COST[$depth]+x}" ]; then
            echo "FAIL: gate expects a QUOTACOST line for depth $depth and the boot printed none." >&2
            echo "      A cost nobody measures is how an 8 % throughput regression passes." >&2
            fail=1
            continue
        fi
        [ "$REF_COST" -gt 0 ] || continue
        ratio=$(( COST[$depth] * 100 / REF_COST ))
        if [ "$ratio" -lt "${COST_FLOOR[$depth]}" ]; then
            echo "FAIL: a charge at depth $depth cost ${COST[$depth]} cycles against a ${REF_COST}-cycle" >&2
            echo "      reference — ${ratio}x100, under the floor ${COST_FLOOR[$depth]}. The charge path" >&2
            echo "      cannot be cheaper than the atomics it performs; the measurement collapsed." >&2
            fail=1
        fi
    done
    if [ "$DEPTH_RATIO_DEPTH" -gt 0 ]; then
        if [ -z "${COST[1]+x}" ] || [ -z "${COST[$DEPTH_RATIO_DEPTH]+x}" ] \
            || [ "${COST[1]}" -le 0 ]; then
            echo "FAIL: gate expects QUOTACOST at depth 1 and $DEPTH_RATIO_DEPTH and the boot printed only:" >&2
            echo "      ${!COST[*]}" >&2
            fail=1
        else
            ratio=$(( COST[$DEPTH_RATIO_DEPTH] * 100 / COST[1] ))
            if [ "$ratio" -gt "$DEPTH_RATIO_CAP" ]; then
                echo "FAIL: depth $DEPTH_RATIO_DEPTH costs ${ratio}x100 of depth 1, over the recorded cap $DEPTH_RATIO_CAP." >&2
                echo "      The walk stopped amortising: each level is meant to add a bounded constant." >&2
                fail=1
            fi
        fi
    fi

    # A cap matching nothing is a dead entry: it stops describing the kernel and
    # would silently keep passing after the kind it names stopped being charged.
    for gkey in "${!CAPS[@]}"; do
        echo "FAIL: gate entry '$gkey' matched no observed phase/kind — dead entry, delete it." >&2
        fail=1
    done
    # Same ratchet: a `min-kinds-for` naming a phase nothing reports is a floor
    # no run will ever apply, which is a silently lowered floor.
    for gkey in "${!MIN_KINDS_FOR[@]}"; do
        if [ -z "${SEEN_PHASE[$gkey]+x}" ]; then
            echo "FAIL: gate entry 'min-kinds-for $gkey' matched no observed phase — dead entry, delete it." >&2
            fail=1
        fi
    done

    [ "$fail" -eq 0 ] || return 1

    for phase in "${REQUIRED[@]}"; do
        for gkey in $(printf '%s\n' "${!PEAK[@]}" | sort); do
            [ "${gkey%%/*}" = "$phase" ] || continue
            echo "OK: $gkey peak=${PEAK[$gkey]} denials=${DENIALS[$gkey]:-0} (mode=${MODE[$phase]})"
        done
    done
    for depth in $(printf '%s\n' "${!COST_FLOOR[@]}" | sort -n); do
        echo "OK: charge cost at depth $depth = $(( COST[$depth] * 100 / REF_COST ))x100 of" \
            "the ${REF_COST}-cycle reference (floor ${COST_FLOOR[$depth]})"
    done
    if [ "$DEPTH_RATIO_DEPTH" -gt 0 ]; then
        echo "OK: depth $DEPTH_RATIO_DEPTH costs $(( COST[$DEPTH_RATIO_DEPTH] * 100 / COST[1] ))x100" \
            "of depth 1 (cap $DEPTH_RATIO_CAP)"
    fi
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

    # 12. A charge that collapsed against the same run's own reference. The
    #     floor that stops a measurement which stopped measuring from reading as
    #     a healthy one.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=150\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/collapsed.log"
    printf '%smin-charge-over-reference 1 300\n' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "under the floor 300" "$tmp/collapsed.log" "collapsed charge cost rejected"

    # 13. A gate that asks about a depth the boot never measured. A cost
    #     nobody measures is exactly how a regression gets through.
    printf '%smin-charge-over-reference 4 300\n' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "printed none" "$tmp/collapsed.log" "missing charge cost rejected"

    # 14. An honest charge passes, so 12 is not a gate that rejects
    #     unconditionally.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1500\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/fast.log"
    printf '%smin-charge-over-reference 1 300\n' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 0 "charge cost at depth 1" "$tmp/fast.log" "honest charge accepted"

    # 14a. The same shape on a machine ten times slower. This is the whole point
    #      of the ratio: an accelerator change must not move the verdict.
    #      Without this case, 12 and 14 are satisfied by an absolute budget in
    #      disguise.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=15000\r\n'
        printf 'QUOTACOST: reference cycles_per_op=1000\r\n'
    } > "$tmp/slow-host.log"
    _expect 0 "charge cost at depth 1" "$tmp/slow-host.log" "a slower host does not move the verdict"

    # 14b. No reference at all. Every ratio would divide by 1 and pass, which is
    #      the "stopped measuring reads as healthy" hole.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1500\r\n'
    } > "$tmp/noref.log"
    _expect 1 "no QUOTACOST reference line" "$tmp/noref.log" "missing reference rejected"

    # 14c. The walk stopped amortising: each level is meant to add a bounded
    #      constant, so a depth-7 charge costing seven times a depth-1 one is
    #      the regression this catches and an absolute budget never could.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1000\r\n'
        printf 'QUOTACOST: depth=7 cycles_per_charge=7000\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/nonamortised.log"
    printf '%smin-charge-over-reference 1 300\nmin-charge-over-reference 7 300\nmax-depth-cost-ratio 7 500\n' \
        "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "over the recorded cap 500" "$tmp/nonamortised.log" "non-amortising walk rejected"

    # 14d. ...and an amortising one passes, so 14c is not unconditional.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1000\r\n'
        printf 'QUOTACOST: depth=7 cycles_per_charge=4000\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/amortised.log"
    _expect 0 "costs 400x100 of depth 1" "$tmp/amortised.log" "amortising walk accepted"

    # 14e. The same non-amortising shape ten times slower still fails, so 14c is
    #      not an absolute budget either.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=10000\r\n'
        printf 'QUOTACOST: depth=7 cycles_per_charge=70000\r\n'
        printf 'QUOTACOST: reference cycles_per_op=1000\r\n'
    } > "$tmp/nonamortised-slow.log"
    _expect 1 "over the recorded cap 500" "$tmp/nonamortised-slow.log" \
        "a slower host does not excuse a non-amortising walk"

    # 14f. A collapsed reference satisfies every ratio floor above, because
    #      they are all ratios over it. The absolute floor is what closes that,
    #      and it is a physical bound rather than a measurement.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=15\r\n'
        printf 'QUOTACOST: reference cycles_per_op=1\r\n'
    } > "$tmp/noref-cycles.log"
    printf '%smin-charge-over-reference 1 300\nmin-reference-cycles 20\n' \
        "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "under the floor 20" "$tmp/noref-cycles.log" "collapsed reference rejected"

    # 14g. ...and an honest reference at the same shape passes, so 14f is not a
    #      gate that rejects unconditionally.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1500\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/honest-ref.log"
    _expect 0 "charge cost at depth 1" "$tmp/honest-ref.log" "honest reference accepted"

    # 14h. A `min-kinds-for` naming a phase nothing reports is a floor no run
    #      will apply -- the same dead-entry ratchet the caps carry.
    printf '%smin-kinds-for ghost 3\n' "$good_gate" > "$tmp/gates/$VARIANT.txt"
    _expect 1 "dead entry" "$clean" "dead min-kinds-for rejected"

    # 14i. The emitter's own output must pass the check path on the same log.
    #      A gate file the documented remedy cannot regenerate is a gate file
    #      nobody can fix.
    {
        cat "$clean"
        printf 'QUOTACOST: depth=1 cycles_per_charge=1500\r\n'
        printf 'QUOTACOST: depth=7 cycles_per_charge=6000\r\n'
        printf 'QUOTACOST: reference cycles_per_op=100\r\n'
    } > "$tmp/roundtrip.log"
    set +e
    out=$( "$0" --variant "$VARIANT" --gate-data-dir "$tmp/gates" \
        --log "$tmp/roundtrip.log" --emit-allowlist 2>&1 )
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
        echo "SELF-TEST FAIL [emit round-trips]: emit exit $rc, want 0" >&2
        sed 's/^/    /' <<<"$out" >&2
        failures=$((failures + 1))
    else
        printf '%s\n' "$out" > "$tmp/gates/$VARIANT.txt"
        _expect 0 "OK:" "$tmp/roundtrip.log" "emit round-trips"
    fi

    # 15. No gate data at all for the variant.
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
