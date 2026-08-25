#!/usr/bin/env bash
# Lockdep pool-headroom ratchet.
#
# Boots the test ISO, parses every
#   LOCKDEP[<phase>]: <state> classes=N/C (P%) edges=E/EC chains=H/HC ...
# line, and fails if any required phase is missing or not ACTIVE, if a
# violation was reported, if a pool exceeds the gate file's max-fill-pct, or if
# a pool carrying a recorded cap exceeds it.
#
# There are two kinds of gate entry, because the three pools are not one kind of
# measurement:
#
#   <phase> <TAB> <pool> <TAB> <cap>   exceeding it FAILS the run
#   band <phase> <pool> <lo> <hi>      leaving it prints DRIFT and passes
#
# A cap is for a quantity a run cannot move on its own: class counts (a class
# registers on the first acquire of a declaration site, and by the end of a
# phase every site the phase reaches has been reached) and everything in the
# `boot` phase, which runs a fixed sequence with no test scheduling in it. Edge
# and chain counts in the test phases instead count which orderings a run
# *happened to observe*; they move between runs of identical code, so an exact
# threshold on them fires on interleaving and on suite growth rather than on a
# change to the kernel. Those carry bands: a run outside one is reported for
# review, not failed. What fails on a real lock-order defect is `violations=`,
# and that is untouched.
#
# The gate data lives in scripts/gates/lockdep/<variant>.txt so weakening a
# check is a diff on a tracked file rather than an edit to this script. A
# directive naming a phase the boot never printed FAILS — cap and band alike:
# a phase that stopped reporting looks exactly like a phase that passed.
#
#     scripts/check_lockdep_headroom.sh
#     scripts/check_lockdep_headroom.sh --log captured-raw.log
#     scripts/check_lockdep_headroom.sh --emit-allowlist --log A.log --log B.log
#     scripts/check_lockdep_headroom.sh --self-test
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VARIANT=tests
LOGS=()
EMIT=0
SELF_TEST=0
GATE_DATA_DIR="$REPO_ROOT/scripts/gates/lockdep"
while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="$2"; shift 2 ;;
        --log) LOGS+=("$2"); shift 2 ;;
        --emit-allowlist) EMIT=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --gate-data-dir) GATE_DATA_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,35p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Phases whose edge and chain counts are a property of the kernel rather than of
# how a run was scheduled. `boot` reaches its report by a fixed sequence with no
# test dispatch in it and measures the same edge and chain counts on every run;
# the test phases do not. Used only by --emit-allowlist, to decide which pools
# come out as caps and which as bands.
DETERMINISTIC_PHASES=" boot "

phase_is_deterministic() {
    [[ "$DETERMINISTIC_PHASES" == *" $1 "* ]]
}

# The one line the whole gate reads, matched once and in full. Per-field
# `sed 's/.*X=\([0-9]*\).*/\1/'` echoed its input unchanged when the pattern
# missed, so a renamed field yielded a whole log line where an integer was
# expected and the script died in arithmetic instead of saying the line did not
# parse. A state word may carry a parenthetical ("OFF (lockdep=off)"), so the
# run-up to `classes=` is matched as "contains no equals sign".
LOCKDEP_RE='^LOCKDEP\[([a-z-]+)\]: ([A-Z]+)[^=]*classes=([0-9]+)/([0-9]+) \(([0-9]+)%\) edges=([0-9]+)/([0-9]+) chains=([0-9]+)/([0-9]+).*violations=([0-9]+)'

declare -A OBSERVED=()
# Per phase/pool extremes across every --log given. One log makes these equal to
# the single observation, which is exactly what the emitter must not hide.
declare -A OBS_MIN=()
declare -A OBS_MAX=()

note_obs() {
    local key="$1" val="$2"
    if [ -z "${OBS_MIN[$key]+x}" ] || [ "$val" -lt "${OBS_MIN[$key]}" ]; then
        OBS_MIN["$key"]=$val
    fi
    if [ -z "${OBS_MAX[$key]+x}" ] || [ "$val" -gt "${OBS_MAX[$key]}" ]; then
        OBS_MAX["$key"]=$val
    fi
}

parse_log() {
    local log="$1" raw
    local lines
    # `grep -c` on a missing file prints nothing and the count below would then
    # die in arithmetic rather than naming the file, which is the same failure
    # the whole-line LOCKDEP_RE match exists to avoid.
    if [ ! -r "$log" ]; then
        echo "FAIL: cannot read log '$log' — nothing was measured." >&2
        return 1
    fi
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
        note_obs "${BASH_REMATCH[1]}/classes" "${BASH_REMATCH[3]}"
        note_obs "${BASH_REMATCH[1]}/edges" "${BASH_REMATCH[6]}"
        note_obs "${BASH_REMATCH[1]}/chains" "${BASH_REMATCH[8]}"
    done < <(grep -oE 'LOCKDEP\[[a-z-]+\]:.*' "$log")
}

emit_gate_data() {
    local phase pool lo hi width runs
    runs=${#LOGS[@]}
    echo "# check_lockdep_headroom gate data — variant: $VARIANT"
    echo "#"
    echo "#     scripts/check_lockdep_headroom.sh --variant $VARIANT --emit-allowlist \\"
    echo "#         --log RUN1.log --log RUN2.log --log RUN3.log"
    echo "#"
    echo "# Emitted from $runs run(s)."
    echo "#"
    echo "# Exact caps where the measurement is deterministic: a class registers"
    echo "# on the first acquire of a declaration site, and by the end of a phase"
    echo "# every site that phase reaches has been reached; boot runs a fixed"
    echo "# sequence with no test scheduling in it. Bands where the value counts"
    echo "# which orderings a run happened to observe, which moves between runs"
    echo "# of identical code."
    echo
    echo "min-classes 32"
    echo "min-edges 16"
    echo "min-chains 32"
    echo "max-fill-pct 70"
    echo
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        echo "require-phase $phase"
    done
    echo
    echo "# <phase> <TAB> <pool> <TAB> <cap> — exceeding one fails the run."
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        printf '%s\tclasses\t%s\n' "$phase" "${OBS_MAX[$phase/classes]}"
        if [ "${OBS_MIN[$phase/classes]}" -ne "${OBS_MAX[$phase/classes]}" ]; then
            printf '# WARNING: %s classes varied %s-%s across these runs. A class count\n' \
                "$phase" "${OBS_MIN[$phase/classes]}" "${OBS_MAX[$phase/classes]}"
            printf '# that is not deterministic falsifies the premise this cap rests on.\n'
        fi
    done
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        phase_is_deterministic "$phase" || continue
        for pool in edges chains; do
            printf '%s\t%s\t%s\n' "$phase" "$pool" "${OBS_MAX[$phase/$pool]}"
            if [ "${OBS_MIN[$phase/$pool]}" -ne "${OBS_MAX[$phase/$pool]}" ]; then
                printf '# WARNING: %s %s varied %s-%s — this phase was believed deterministic.\n' \
                    "$phase" "$pool" "${OBS_MIN[$phase/$pool]}" "${OBS_MAX[$phase/$pool]}"
            fi
        done
    done
    echo
    echo "# band <phase> <pool> <lo> <hi> — reported as DRIFT, never failed. Each"
    echo "# band is the observed range widened by its own width on each side, so a"
    echo "# band emitted from a single run has lo == hi and will drift on the next"
    echo "# one. Measure over several runs."
    for phase in $(printf '%s\n' "${!OBSERVED[@]}" | sort); do
        phase_is_deterministic "$phase" && continue
        printf '# observed over %s run(s): edges %s-%s, chains %s-%s\n' \
            "$runs" "${OBS_MIN[$phase/edges]}" "${OBS_MAX[$phase/edges]}" \
            "${OBS_MIN[$phase/chains]}" "${OBS_MAX[$phase/chains]}"
        for pool in edges chains; do
            lo=${OBS_MIN[$phase/$pool]}
            hi=${OBS_MAX[$phase/$pool]}
            width=$((hi - lo))
            lo=$((lo - width))
            if [ "$lo" -lt 0 ]; then lo=0; fi
            hi=$((hi + width))
            printf 'band %s %s %s %s\n' "$phase" "$pool" "$lo" "$hi"
        done
    done
}

# A band is a review prompt, so it says which way the count went and by how
# much. A drop matters as much as a rise: twenty chains fewer is a phase that
# stopped reaching orderings it used to reach.
report_drift() {
    local phase="$1" pool="$2" val="$3" lo="$4" hi="$5" delta dir
    if [ "$val" -lt "$lo" ]; then
        delta=$((lo - val))
        dir="below"
    else
        delta=$((val - hi))
        dir="above"
    fi
    # stderr, like every other diagnostic here: a DRIFT on stdout of a green
    # CI job is a line nobody reads, and the summary below would then have the
    # last word on the very pool that moved.
    echo "DRIFT: LOCKDEP[$phase] $pool = $val, $delta $dir the recorded band $lo-$hi." >&2
    echo "       Not a failure — this pool counts which orderings the run happened to" >&2
    echo "       observe. If it persists, re-measure over several runs with" >&2
    echo "       --emit-allowlist and say in the commit message what moved." >&2
}

run_gate() {
    local gate="$GATE_DATA_DIR/$VARIANT.txt"

    if [ ! -f "$gate" ]; then
        echo "check_lockdep_headroom: no gate data at $gate" >&2
        echo "  Every gated variant needs its own measured baseline. Create it with:" >&2
        echo "      scripts/check_lockdep_headroom.sh --variant $VARIANT --emit-allowlist" >&2
        return 2
    fi

    local MIN_CLASSES=0 MIN_EDGES=0 MIN_CHAINS=0 MAX_FILL=100
    local -a REQUIRED=()
    declare -A CAPS=()
    declare -A BAND_LO=() BAND_HI=()
    local lineno=0 line key bphase bpool blo bhi
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
            min-edges) MIN_EDGES=$(awk '{print $2}' <<<"$line") ;;
            min-chains) MIN_CHAINS=$(awk '{print $2}' <<<"$line") ;;
            max-fill-pct) MAX_FILL=$(awk '{print $2}' <<<"$line") ;;
            require-phase) REQUIRED+=("$(awk '{print $2}' <<<"$line")") ;;
            band)
                read -r _ bphase bpool blo bhi _ <<<"$line"
                if [[ ! "$blo" =~ ^[0-9]+$ ]] || [[ ! "$bhi" =~ ^[0-9]+$ ]] \
                    || [ -z "$bphase" ] || [ -z "$bpool" ]; then
                    echo "check_lockdep_headroom: $gate:$lineno: want 'band <phase> <pool> <lo> <hi>'" >&2
                    return 2
                fi
                if [ "$blo" -gt "$bhi" ]; then
                    echo "check_lockdep_headroom: $gate:$lineno: band $bphase $bpool has lo $blo > hi $bhi" >&2
                    return 2
                fi
                BAND_LO["$bphase/$bpool"]=$blo
                BAND_HI["$bphase/$bpool"]=$bhi
                ;;
            *)
                echo "check_lockdep_headroom: $gate:$lineno: unknown directive '$key'" >&2
                return 2
                ;;
        esac
    done < "$gate"

    local fail=0 phase state c e h viol pool_c pool_e pool_h pool val pool_size pct gkey bkey
    declare -A DRIFTED=()

    # A pool graded both ways is a pool whose policy nobody decided: the cap
    # would fail runs the band deliberately tolerates.
    for bkey in "${!BAND_LO[@]}"; do
        if [ -n "${CAPS[$bkey]+x}" ]; then
            echo "check_lockdep_headroom: $gate: '$bkey' carries both a cap and a band — pick one." >&2
            return 2
        fi
    done

    # A banded pool has no upper failure of its own, so the floor is the only
    # thing standing between it and a validator that recorded nothing. Declaring
    # the band without the floor is that hole, and it is a gate-file error
    # rather than a run failure because no run can tell you the line is missing.
    for bkey in "${!BAND_LO[@]}"; do
        case "${bkey##*/}" in
            edges)  [ "$MIN_EDGES" -gt 0 ] && continue
                    echo "check_lockdep_headroom: $gate: 'band $bkey' with no min-edges — a pool" >&2 ;;
            chains) [ "$MIN_CHAINS" -gt 0 ] && continue
                    echo "check_lockdep_headroom: $gate: 'band $bkey' with no min-chains — a pool" >&2 ;;
            *)      continue ;;
        esac
        echo "check_lockdep_headroom: that stopped being counted would pass every ceiling above it." >&2
        return 2
    done

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
        # The same hole, for the two pools that carry a band instead of a cap.
        # A floor is not a tight threshold: it sits far below every observation,
        # so it fails a validator that stopped recording and nothing else. The
        # band's low end is deliberately NOT this floor — leaving a band in
        # either direction is a prompt, and a run that happened to observe
        # fewer orderings is exactly as innocent as one that observed more.
        if [ "$e" -lt "$MIN_EDGES" ]; then
            echo "FAIL: LOCKDEP[$phase] recorded only $e edges (min $MIN_EDGES) — nothing measured." >&2
            fail=1
        fi
        if [ "$h" -lt "$MIN_CHAINS" ]; then
            echo "FAIL: LOCKDEP[$phase] recorded only $h chains (min $MIN_CHAINS) — nothing measured." >&2
            fail=1
        fi
        for pool in classes edges chains; do
            case "$pool" in
                classes) val=$c; pool_size=$pool_c ;;
                edges)   val=$e; pool_size=$pool_e ;;
                chains)  val=$h; pool_size=$pool_h ;;
            esac
            pct=$(( val * 100 / pool_size ))
            # Compared without the truncation the percentage carries, so the
            # ceiling the gate file states is the ceiling that is enforced.
            if [ $(( val * 100 )) -gt $(( MAX_FILL * pool_size )) ]; then
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
            if [ -n "${BAND_LO[$gkey]+x}" ]; then
                blo=${BAND_LO[$gkey]}
                bhi=${BAND_HI[$gkey]}
                if [ "$val" -lt "$blo" ] || [ "$val" -gt "$bhi" ]; then
                    report_drift "$phase" "$pool" "$val" "$blo" "$bhi"
                    DRIFTED["$phase"]=1
                fi
                unset "BAND_LO[$gkey]" "BAND_HI[$gkey]"
            fi
        done
    done

    # An entry matching nothing is dead: it stops describing the kernel and
    # would silently keep passing after the phase it names disappeared. Bands
    # are swept exactly as caps are — a phase that stopped reporting its edges
    # is otherwise indistinguishable from a phase whose edges are in band.
    for gkey in "${!CAPS[@]}"; do
        echo "FAIL: gate entry '$gkey' matched no observed phase/pool — dead entry, delete it." >&2
        fail=1
    done
    for bkey in "${!BAND_LO[@]}"; do
        echo "FAIL: gate entry 'band $bkey' matched no observed phase/pool — dead entry, delete it." >&2
        fail=1
    done

    [ "$fail" -eq 0 ] || return 1

    for phase in "${REQUIRED[@]}"; do
        read -r state c e h _ pool_c pool_e pool_h <<<"${OBSERVED[$phase]}"
        local verdict=OK
        [ -z "${DRIFTED[$phase]+x}" ] || verdict=DRIFT
        echo "$verdict: LOCKDEP[$phase] $state classes=$c/$pool_c edges=$e/$pool_e chains=$h/$pool_h"
    done
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
#
# A gate that has never been observed to reject has not been observed to work.
# `--log` already bypasses QEMU, so every case is a crafted log plus a crafted
# gate file — no boot, well under a second for the set. The band cases are the
# ones that keep the new policy honest in both directions: a drift must not
# fail, and everything a band does not grade must still fail.

self_test() {
    local tmp out rc failures=0 accepts=0 rejects=0 emitters=0
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/gates"

    _line() {
        printf 'LOCKDEP[%s]: %s classes=%s/508 (%s%%) edges=%s/1024 chains=%s/2048 held_max=3/16 held_drops=0 pop_miss=0/0 chain_hit=8 chain_miss=1 violations=%s reports=0 collisions=0 mode=Panic\r\n' \
            "$1" "$2" "$3" "$4" "$5" "$6" "$7"
    }

    _assert_has() {
        local text="$1" want="$2" label="$3"
        if ! grep -qF -- "$want" <<<"$text"; then
            echo "SELF-TEST FAIL [$label]: output missing '$want'" >&2
            sed 's/^/    /' <<<"$text" >&2
            failures=$((failures + 1))
        fi
    }

    _assert_hasnt() {
        local text="$1" unwanted="$2" label="$3"
        if grep -qF -- "$unwanted" <<<"$text"; then
            echo "SELF-TEST FAIL [$label]: output contains '$unwanted' and must not" >&2
            sed 's/^/    /' <<<"$text" >&2
            failures=$((failures + 1))
        fi
    }

    # want_rc want_msg log label [must_not_contain]
    #
    # Counted here rather than tallied by hand at the end: a case added without
    # the tally moving is a case nobody notices is missing, and the tally is the
    # only thing a reader has to check the self-test against.
    _expect() {
        local want_rc="$1" want_msg="$2" log="$3" label="$4" absent="${5:-}"
        if [ "$want_rc" -eq 0 ]; then
            accepts=$((accepts + 1))
        else
            rejects=$((rejects + 1))
        fi
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
        [ -n "$want_msg" ] && _assert_has "$out" "$want_msg" "$label"
        [ -n "$absent" ] && _assert_hasnt "$out" "$absent" "$label"
        return 0
    }

    _line boot ACTIVE 65 12 48 112 0 > "$tmp/clean.log"

    # The positive control. Without it every rejection below could be a gate
    # that rejects unconditionally. It carries a band as well as a cap, so the
    # band parser is exercised on the accept path and an in-band value is
    # proven silent.
    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nband boot edges 40 60\n' > "$tmp/gates/$VARIANT.txt"
    _expect 0 "OK: LOCKDEP[boot]" "$tmp/clean.log" "clean log accepted" "DRIFT:"

    : > "$tmp/empty.log"
    _expect 1 "nothing was measured" "$tmp/empty.log" "empty log rejected"

    # A log path that does not exist must name the path, not die counting an
    # empty string.
    _expect 1 "cannot read log" "$tmp/no-such.log" "missing log rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nrequire-phase post-userland-tests\nboot\tclasses\t65\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "never printed it" "$tmp/clean.log" "missing phase rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nghost\tedges\t1\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "dead entry" "$tmp/clean.log" "dead cap entry rejected"

    # A band matching nothing is the same hole as a dead cap: the phase that
    # stopped reporting is the one whose edges nobody will miss.
    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nband ghost chains 1 2\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "dead entry" "$tmp/clean.log" "dead band entry rejected"

    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t64\n' > "$tmp/gates/$VARIANT.txt"
    _expect 1 "over the recorded cap" "$tmp/clean.log" "over-cap rejected"

    # Grading one pool both ways is a policy nobody decided.
    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tchains\t120\nband boot chains 100 120\n' > "$tmp/gates/$VARIANT.txt"
    _expect 2 "carries both a cap and a band" "$tmp/clean.log" "cap and band on one pool rejected"

    # The pool ceiling must still bite for exactly the pools whose caps were
    # replaced by bands. 1600/2048 = 78% > 70%. The drift-accepted case below is
    # the control proving the band alone did not cause this rejection.
    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nband boot chains 100 120\n' > "$tmp/gates/$VARIANT.txt"
    _line boot ACTIVE 65 12 48 1600 0 > "$tmp/full.log"
    _expect 1 "over max-fill-pct" "$tmp/full.log" "fill ceiling rejected"

    # The whole point of the band: a scheduling-dependent count outside its
    # recorded range is reported and the run still passes.
    _line boot ACTIVE 65 12 48 400 0 > "$tmp/drift.log"
    _expect 0 "DRIFT: LOCKDEP[boot] chains = 400" "$tmp/drift.log" "band drift reported, not rejected" "FAIL:"

    # ...and the drift line names which way and by how much.
    _assert_has "$out" "280 above the recorded band 100-120" "band drift names the delta"

    # A pool that stopped being counted reads as healthy against every ceiling.
    # The floor is what plugs that, and it is a floor rather than the band's own
    # low end on purpose — the three cases below are the whole policy:
    # under the floor fails, between the floor and the band drifts, in band is
    # silent.
    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nband boot edges 40 60\n' > "$tmp/gates/$VARIANT.txt"
    _line boot ACTIVE 65 12 0 112 0 > "$tmp/zero.log"
    _expect 1 "recorded only 0 edges" "$tmp/zero.log" "zero pool against a floor rejected"

    _line boot ACTIVE 65 12 48 4 0 > "$tmp/lowchains.log"
    _expect 1 "recorded only 4 chains" "$tmp/lowchains.log" "chains under the floor rejected"

    # Between the floor and the band: a run that happened to observe fewer
    # orderings is exactly as innocent as one that observed more, so this is a
    # prompt and not a red run. Without this case the floor above could be a
    # gate that rejects every below-band value.
    _line boot ACTIVE 65 12 20 112 0 > "$tmp/lowedges.log"
    _expect 0 "DRIFT: LOCKDEP[boot] edges = 20" "$tmp/lowedges.log" "below-band-above-floor drifts" "FAIL:"
    _assert_has "$out" "20 below the recorded band 40-60" "low drift names the direction"

    # ...and a drifted phase must not have "OK" as the last word on it.
    _assert_hasnt "$out" "OK: LOCKDEP[boot]" "drifted phase is not summarised OK"
    _assert_has "$out" "DRIFT: LOCKDEP[boot] ACTIVE" "drifted phase is summarised DRIFT"

    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nband boot chains 100 120\n' > "$tmp/gates/$VARIANT.txt"

    # A band with no floor is the hole the floors exist to plug, and no run can
    # tell you the line is missing -- so it is a gate-file error, not a failure.
    printf 'min-classes 32\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nband boot edges 40 60\n' > "$tmp/gates/$VARIANT.txt"
    _expect 2 "with no min-edges" "$tmp/clean.log" "band without a floor rejected"
    printf 'min-classes 32\nmin-edges 16\nmax-fill-pct 70\nrequire-phase boot\nboot\tclasses\t65\nband boot chains 100 120\n' > "$tmp/gates/$VARIANT.txt"
    _expect 2 "with no min-chains" "$tmp/clean.log" "chain band without a floor rejected"

    printf 'min-classes 32\nmin-edges 16\nmin-chains 32\nmax-fill-pct 70\nrequire-phase boot\nband boot chains 100 120\n' > "$tmp/gates/$VARIANT.txt"

    # The fill ceiling is compared without truncation, so the number the gate
    # file states is the number enforced: 70% of 2048 is 1433.6, and 1434 must
    # fail where a truncated comparison would have let it through.
    _line boot ACTIVE 65 12 48 1434 0 > "$tmp/edge-fill.log"
    _expect 1 "over max-fill-pct" "$tmp/edge-fill.log" "the stated fill ceiling is the enforced one"
    _line boot ACTIVE 65 12 48 1433 0 > "$tmp/under-fill.log"
    _expect 0 "LOCKDEP[boot]" "$tmp/under-fill.log" "one under the ceiling passes"

    # A validator that turned itself off still reports; it must not pass.
    _line boot "DISABLED (pool overflow)" 65 12 48 112 0 > "$tmp/disabled.log"
    _expect 1 "not ACTIVE" "$tmp/disabled.log" "inactive validator rejected"

    _line boot ACTIVE 65 12 48 112 3 > "$tmp/viol.log"
    _expect 1 "violation(s)" "$tmp/viol.log" "violation rejected"

    _line boot ACTIVE 4 1 48 112 0 > "$tmp/tiny.log"
    _expect 1 "registered only 4 classes" "$tmp/tiny.log" "class floor rejected"

    # A renamed field must say so rather than die in arithmetic.
    printf 'LOCKDEP[boot]: ACTIVE klasses=65/508 (12%%) edges=48/1024 chains=112/2048 violations=0\r\n' > "$tmp/renamed.log"
    _expect 1 "the report format moved" "$tmp/renamed.log" "unparseable line rejected"

    # The emitter must produce a file in the shape the gate now reads: caps for
    # the deterministic pools, bands for the rest, merged across every --log.
    # Without this the operator is back to single-observation bands.
    { _line boot ACTIVE 65 12 43 110 0; _line post-kernel-tests ACTIVE 65 12 148 340 0; } > "$tmp/emit-a.log"
    { _line boot ACTIVE 65 12 43 110 0; _line post-kernel-tests ACTIVE 65 12 160 352 0; } > "$tmp/emit-b.log"
    set +e
    out=$( "$0" --variant "$VARIANT" --gate-data-dir "$tmp/gates" \
        --log "$tmp/emit-a.log" --log "$tmp/emit-b.log" --emit-allowlist 2>&1 )
    rc=$?
    set -e
    if [ "$rc" -ne 0 ]; then
        echo "SELF-TEST FAIL [emit merges several logs]: exit $rc, want 0" >&2
        sed 's/^/    /' <<<"$out" >&2
        failures=$((failures + 1))
    else
        _assert_has "$out" "band post-kernel-tests chains 328 364" "emit merges several logs"
        _assert_has "$out" "band post-kernel-tests edges 136 172" "emit merges several logs"
        _assert_has "$out" "$(printf 'boot\tchains\t110')" "emit keeps boot exact"
        _assert_has "$out" "$(printf 'boot\tedges\t43')" "emit keeps boot exact"
        _assert_hasnt "$out" "band boot" "emit keeps boot exact"
        emitters=$((emitters + 1))
    fi

    # A check grades one run, so several logs on the check path is an operator
    # error rather than a silent last-one-wins.
    set +e
    out=$( "$0" --variant "$VARIANT" --gate-data-dir "$tmp/gates" \
        --log "$tmp/emit-a.log" --log "$tmp/emit-b.log" 2>&1 )
    rc=$?
    set -e
    if [ "$rc" -ne 2 ]; then
        echo "SELF-TEST FAIL [several logs on the check path rejected]: exit $rc, want 2" >&2
        sed 's/^/    /' <<<"$out" >&2
        failures=$((failures + 1))
    else
        _assert_has "$out" "only with --emit-allowlist" "several logs on the check path rejected"
        rejects=$((rejects + 1))
    fi

    if [ "$failures" -ne 0 ]; then
        echo "check_lockdep_headroom: SELF-TEST FAILED ($failures case(s)) — the gate cannot be trusted to reject." >&2
        return 1
    fi
    echo "check_lockdep_headroom: self-test OK — $((accepts + rejects + emitters)) cases," \
        "$accepts accept + $rejects reject + $emitters emitter."
}

if [ "$SELF_TEST" -eq 1 ]; then
    self_test
    exit $?
fi

if [ "${#LOGS[@]}" -eq 0 ]; then
    if [ ! -x builddir/run_tests ]; then
        echo "FAIL: builddir/run_tests is not built — run 'just check-lockdep-headroom'." >&2
        exit 1
    fi
    BOOT_LOG="$(mktemp)"
    trap 'rm -f "$BOOT_LOG"' EXIT INT TERM
    # Failures are diagnosed from the parsed counters, not the exit status:
    # a suite that fails a test still produces valid LOCKDEP lines.
    builddir/run_tests --raw --no-color > "$BOOT_LOG" 2>&1 || true
    LOGS=("$BOOT_LOG")
elif [ "${#LOGS[@]}" -gt 1 ] && [ "$EMIT" -eq 0 ]; then
    echo "check_lockdep_headroom: --log may be repeated only with --emit-allowlist." >&2
    echo "  A check grades one run. Merging several into one verdict would hide" >&2
    echo "  which run said what, and summarising several runs is what" >&2
    echo "  --emit-allowlist exists to do." >&2
    exit 2
fi

for log_path in "${LOGS[@]}"; do
    parse_log "$log_path"
done

if [ "$EMIT" -eq 1 ]; then
    emit_gate_data
    exit 0
fi

run_gate
