#!/usr/bin/env bash
# SMP placement-eligibility gate.
#
# Boots the test ISO (or parses a captured raw log) and reads every
#   SCHEDCPU[<phase>]: cpus=N online=0xM eligible=0xM
#   SCHEDCPU[<phase>]: cpu=I online=B eligible=B switches=N ticks=N idle=N pulled=N pushed=N
# line, then holds the run to one invariant: **a CPU that is online is eligible
# for task placement.**
#
# That invariant is not decorative. Every placement helper filters candidates
# through `is_schedulable_cpu`, so a CPU whose runqueue is online but not
# enabled vanishes from every fork, exec and wakeup decision while still
# dispatching whatever work stealing hands it. The machine boots, the suite
# passes, and one core does all the work. It shipped that way once: the
# scheduler boot step ran in the `services` phase and reset every runqueue —
# including the three APs that had already entered their scheduler loops during
# `drivers` — leaving `eligible` a single CPU for the life of the boot.
#
# Two numbers per phase are tracked, both measured:
#   eligible-lag  how many online CPUs may legitimately not be eligible yet.
#                 The BSP enters its scheduler loop only after boot init
#                 finishes, so it is genuinely ineligible in the `boot` phase
#                 and nowhere else.
#   allow-ineligible  *which* CPUs that lag may name, as a mask. A count alone
#                 accepts an AP dropping out as readily as the BSP, which is
#                 the failure this gate exists to catch.
#   require-dispatch  whether every online CPU in the phase must show it
#                 dispatched something. A flag, never a volume: how *many*
#                 switches a CPU makes is a property of the workload and the
#                 machine — one CI runner's least-busy CPU made 874 against
#                 12470 here — so any floor above zero is a coin toss. Zero
#                 versus non-zero is the only part that describes the
#                 scheduler. `0` disables it for the phases whose counters the
#                 hermetic fixture zeroes per test scope.
#
#     scripts/check_sched_spread.sh
#     scripts/check_sched_spread.sh --log captured-raw.log
#     scripts/check_sched_spread.sh --emit-allowlist
#     scripts/check_sched_spread.sh --self-test
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$REPO_ROOT"

VARIANT=tests
LOG=""
EMIT=0
SELF_TEST=0
GATE_DATA_DIR="$REPO_ROOT/scripts/gates/sched"
while [ $# -gt 0 ]; do
    case "$1" in
        --variant) VARIANT="$2"; shift 2 ;;
        --log) LOG="$2"; shift 2 ;;
        --emit-allowlist) EMIT=1; shift ;;
        --self-test) SELF_TEST=1; shift ;;
        --gate-data-dir) GATE_DATA_DIR="$2"; shift 2 ;;
        -h|--help) sed -n '2,31p' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
done

# Matched once and in full: a per-field `sed` echoes its input unchanged on a
# miss, so a renamed field would yield a whole log line where an integer was
# expected.
SUMMARY_RE='^SCHEDCPU\[([a-z-]+)\]: cpus=([0-9]+) online=0x([0-9a-f]+) eligible=0x([0-9a-f]+)'
CPU_RE='^SCHEDCPU\[([a-z-]+)\]: cpu=([0-9]+) online=([01]) eligible=([01]) switches=([0-9]+) ticks=([0-9]+) idle=([0-9]+) pulled=([0-9]+) pushed=([0-9]+)'

declare -A ONLINE_MASK=() ELIGIBLE_MASK=() CPU_COUNT=()
declare -A SWITCHES=() PULLED=() PUSHED=() CPU_ONLINE=() CPU_SEEN=()
declare -A SEEN_PHASE=()
PHASES_IN_ORDER=()

# Over hex nibbles, not shifts: bash arithmetic is signed, so `>>` on a mask
# with bit 63 set sign-extends and the loop never reaches zero.
popcount() {
    local hex n=0 nib i
    printf -v hex '%016x' "$1"
    for (( i = 0; i < 16; i++ )); do
        nib=$(( 16#${hex:i:1} ))
        n=$(( n + (nib & 1) + ((nib >> 1) & 1) + ((nib >> 2) & 1) + ((nib >> 3) & 1) ))
    done
    echo "$n"
}

parse_log() {
    local log="$1" line
    local found=0
    while IFS= read -r line; do
        if [[ $line =~ $SUMMARY_RE ]]; then
            local phase="${BASH_REMATCH[1]}"
            found=1
            if [ -z "${SEEN_PHASE[$phase]:-}" ]; then
                SEEN_PHASE[$phase]=1
                PHASES_IN_ORDER+=("$phase")
            fi
            CPU_COUNT[$phase]="${BASH_REMATCH[2]}"
            ONLINE_MASK[$phase]=$(( 16#${BASH_REMATCH[3]} ))
            ELIGIBLE_MASK[$phase]=$(( 16#${BASH_REMATCH[4]} ))
        elif [[ $line =~ $CPU_RE ]]; then
            local phase="${BASH_REMATCH[1]}" cpu="${BASH_REMATCH[2]}"
            CPU_SEEN[$phase/$cpu]=1
            CPU_ONLINE[$phase/$cpu]="${BASH_REMATCH[3]}"
            SWITCHES[$phase/$cpu]="${BASH_REMATCH[5]}"
            PULLED[$phase/$cpu]="${BASH_REMATCH[8]}"
            PUSHED[$phase/$cpu]="${BASH_REMATCH[9]}"
        fi
    done < "$log"
    if [ "$found" -eq 0 ]; then
        echo "check_sched_spread: no SCHEDCPU lines in $log" >&2
        echo "  The kernel emits them from sched_cpu_report(); a log with none" >&2
        echo "  is a boot that never reached a report site, not a healthy run." >&2
        exit 1
    fi
}

boot_and_capture() {
    local out="$1"
    just _build-run-tests >/dev/null
    set -o pipefail
    "$REPO_ROOT/builddir/run_tests" --raw --no-color 2>&1 | tee "$out" >/dev/null || true
}

emit_allowlist() {
    echo "# Measured by: scripts/check_sched_spread.sh --variant $VARIANT --emit-allowlist"
    echo "#"
    echo "# eligible-lag     <phase> <n>  online CPUs allowed to be ineligible"
    echo "# allow-ineligible <phase> <m>  which CPUs that lag may name"
    echo "# require-dispatch <phase> <b>  every online CPU must dispatch something"
    echo "# min-online       <n>          floor on the online CPU count"
    echo "#"
    echo "# A lag above 0 outside the boot phase is a bug, not a number to raise:"
    echo "# it means a CPU is dispatching work that no placement decision can see."
    echo "# require-dispatch is zero-versus-non-zero, never a volume: how many"
    echo "# switches a CPU makes varies with the machine, so a floor above zero"
    echo "# fails on a runner that is merely faster or slower."
    echo
    local min_online=99
    for phase in "${PHASES_IN_ORDER[@]}"; do
        local online="${ONLINE_MASK[$phase]}" eligible="${ELIGIBLE_MASK[$phase]}"
        local lag_mask=$(( online & ~eligible ))
        local lag; lag=$(popcount "$lag_mask")
        local n_online; n_online=$(popcount "$online")
        [ "$n_online" -lt "$min_online" ] && min_online="$n_online"
        local dispatch=1 cpu seen=0
        for (( cpu = 0; cpu < ${CPU_COUNT[$phase]}; cpu++ )); do
            [ "${CPU_ONLINE[$phase/$cpu]:-0}" = "1" ] || continue
            seen=1
            [ "${SWITCHES[$phase/$cpu]}" -eq 0 ] && dispatch=0
        done
        [ "$seen" -eq 1 ] || dispatch=0
        echo "eligible-lag	$phase	$lag"
        printf 'allow-ineligible\t%s\t0x%x\n' "$phase" "$lag_mask"
        echo "require-dispatch	$phase	$dispatch"
    done
    echo
    # Chosen, not measured: two CPUs is the least that can demonstrate the
    # invariant at all.
    : "$min_online"
    echo "min-online	2"
}

grade() {
    local gate="$1" rc=0
    local -A WANT_LAG=() WANT_DISPATCH=() WANT_MASK=()
    local -A GATE_LINE_USED=()
    local min_online=1
    local key val phase
    while read -r key phase val; do
        case "${key:-}" in
            ''|'#'*) continue ;;
            eligible-lag) WANT_LAG[$phase]="$val" ;;
            allow-ineligible) WANT_MASK[$phase]=$(( val )) ;;
            require-dispatch) WANT_DISPATCH[$phase]="$val" ;;
            min-online) min_online="$phase" ;;
            *) echo "check_sched_spread: unknown gate key '$key' in $gate" >&2; exit 2 ;;
        esac
    done < <(sed 's/#.*//' "$gate")

    for phase in "${!WANT_LAG[@]}"; do
        if [ -z "${SEEN_PHASE[$phase]:-}" ]; then
            echo "check_sched_spread: gate names phase '$phase', which the run never reported" >&2
            echo "  A dead entry is an exemption for something that no longer happens." >&2
            rc=1
        fi
    done

    for phase in "${PHASES_IN_ORDER[@]}"; do
        local online="${ONLINE_MASK[$phase]}" eligible="${ELIGIBLE_MASK[$phase]}"
        local n_online; n_online=$(popcount "$online")
        local lag_mask=$(( online & ~eligible ))
        local lag; lag=$(popcount "$lag_mask")
        local want="${WANT_LAG[$phase]:-0}"

        if [ "$n_online" -lt "$min_online" ]; then
            echo "check_sched_spread: phase $phase has $n_online online CPU(s), floor is $min_online" >&2
            echo "  A run that brought up fewer CPUs cannot demonstrate the invariant." >&2
            rc=1
        fi

        local allow="${WANT_MASK[$phase]:-0}"
        local unexpected=$(( lag_mask & ~allow ))
        if [ "$lag" -gt "$want" ] || [ "$unexpected" -ne 0 ]; then
            printf 'check_sched_spread: phase %s — %d online CPU(s) are not eligible for placement (allowed %d)\n' \
                "$phase" "$lag" "$want" >&2
            printf '  online=0x%x eligible=0x%x ineligible=0x%x permitted=0x%x unexpected=0x%x\n' \
                "$online" "$eligible" "$lag_mask" "$allow" "$unexpected" >&2
            echo "  Every task placement filters on this set, so those CPUs receive work" >&2
            echo "  only by work stealing. Find what left their runqueue disabled; do not" >&2
            echo "  raise the number." >&2
            rc=1
        fi

        # Before the floor: a missing row is not an idle CPU, and defaulting it
        # to offline would let a truncated report grade as a clean one.
        local want_dispatch="${WANT_DISPATCH[$phase]:-0}"
        local cpu missing=0
        for (( cpu = 0; cpu < ${CPU_COUNT[$phase]}; cpu++ )); do
            (( (online | eligible) & (1 << cpu) )) || continue
            if [ -z "${CPU_SEEN[$phase/$cpu]:-}" ]; then
                missing=$(( missing | (1 << cpu) ))
                continue
            fi
            [ "${CPU_ONLINE[$phase/$cpu]}" = "1" ] || continue
            [ "$want_dispatch" = "1" ] || continue
            if [ "${SWITCHES[$phase/$cpu]}" -eq 0 ]; then
                echo "check_sched_spread: phase $phase cpu $cpu dispatched nothing" >&2
                echo "  An online CPU that never dispatches is a CPU nothing was placed on." >&2
                rc=1
            fi
        done
        if [ "$missing" -ne 0 ]; then
            printf 'check_sched_spread: phase %s reported no per-CPU row for 0x%x\n' "$phase" "$missing" >&2
            printf '  online=0x%x eligible=0x%x — an incomplete report cannot be graded.\n' \
                "$online" "$eligible" >&2
            rc=1
        fi
    done

    if [ "$rc" -eq 0 ]; then
        local summary=""
        for phase in "${PHASES_IN_ORDER[@]}"; do
            local n_online; n_online=$(popcount "${ONLINE_MASK[$phase]}")
            local n_elig; n_elig=$(popcount "${ELIGIBLE_MASK[$phase]}")
            summary+=" $phase=$n_elig/$n_online"
        done
        echo "check_sched_spread: OK — variant=$VARIANT, eligible/online per phase:$summary"
    fi
    return "$rc"
}

self_test() {
    local tmp; tmp=$(mktemp -d); trap 'rm -rf "$tmp"' RETURN
    mkdir -p "$tmp/gates"
    local out

    cat > "$tmp/gates/tests.txt" <<'EOF'
eligible-lag	boot	1
allow-ineligible	boot	0x1
require-dispatch	boot	0
eligible-lag	post-kernel-tests	0
allow-ineligible	post-kernel-tests	0x0
require-dispatch	post-kernel-tests	1
min-online	2
EOF

    # Healthy: the BSP lags at boot and nowhere else.
    cat > "$tmp/good.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe
SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=1 online=1 eligible=1 switches=9 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0xf
SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=90 ticks=99 idle=3 pulled=1 pushed=2
SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=1 switches=70 ticks=99 idle=9 pulled=2 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=1 switches=60 ticks=99 idle=8 pulled=0 pushed=1
SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=1 switches=55 ticks=99 idle=7 pulled=0 pushed=0
EOF
    if ! out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/good.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — healthy log rejected:" >&2
        echo "$out" >&2
        exit 1
    fi

    # The bug this gate exists for: three APs online, only the BSP eligible.
    cat > "$tmp/regressed.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0x1
SCHEDCPU[boot]: cpu=0 online=1 eligible=1 switches=3 ticks=10 idle=1 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=1 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=2 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=3 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0x1
SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=90 ticks=99 idle=3 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=0 switches=7 ticks=99 idle=90 pulled=7 pushed=0
EOF
    if out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/regressed.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted a run with 3 ineligible CPUs" >&2
        exit 1
    fi
    case "$out" in
        *"are not eligible for placement"*) ;;
        *) echo "check_sched_spread: self-test FAILED — wrong rejection reason:" >&2
           echo "$out" >&2; exit 1 ;;
    esac

    # The volume that broke this gate in CI: a machine where the busiest CPU
    # does 20x the least busy still passes, because only zero is a finding.
    cat > "$tmp/lopsided.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe
SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=1 online=1 eligible=1 switches=9 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0xf
SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=20056 ticks=99 idle=3 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=1 switches=874 ticks=99 idle=9 pulled=2 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=1 switches=1167 ticks=99 idle=8 pulled=0 pushed=1
SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=1 switches=13761 ticks=99 idle=7 pulled=0 pushed=0
EOF
    if ! out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/lopsided.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — a lopsided but healthy run was rejected:" >&2
        echo "$out" >&2
        exit 1
    fi

    # An eligible CPU that never dispatches.
    cat > "$tmp/idle_cpu.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe
SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=1 online=1 eligible=1 switches=9 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpus=4 online=0xf eligible=0xf
SCHEDCPU[post-kernel-tests]: cpu=0 online=1 eligible=1 switches=90 ticks=99 idle=3 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=1 online=1 eligible=1 switches=0 ticks=99 idle=99 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=2 online=1 eligible=1 switches=5 ticks=99 idle=90 pulled=0 pushed=0
SCHEDCPU[post-kernel-tests]: cpu=3 online=1 eligible=1 switches=5 ticks=99 idle=90 pulled=0 pushed=0
EOF
    if out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/idle_cpu.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted an eligible CPU that never dispatched" >&2
        exit 1
    fi

    # A gate entry naming a phase the run never reported.
    cat > "$tmp/gates/dead.txt" <<'EOF'
eligible-lag	boot	1
allow-ineligible	boot	0x1
eligible-lag	no-such-phase	0
min-online	2
EOF
    if out=$( "$0" --variant dead --gate-data-dir "$tmp/gates" --log "$tmp/good.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted a dead gate entry" >&2
        exit 1
    fi

    # The lag is within budget but names an AP rather than the BSP.
    cat > "$tmp/wrong_cpu.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xd
SCHEDCPU[boot]: cpu=0 online=1 eligible=1 switches=3 ticks=10 idle=1 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=1 online=1 eligible=0 switches=0 ticks=10 idle=10 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=2 online=1 eligible=1 switches=8 ticks=10 idle=9 pulled=0 pushed=0
SCHEDCPU[boot]: cpu=3 online=1 eligible=1 switches=7 ticks=10 idle=9 pulled=0 pushed=0
EOF
    if out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/wrong_cpu.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted an AP standing in for the BSP's lag" >&2
        exit 1
    fi
    case "$out" in
        *unexpected=0x2*) ;;
        *) echo "check_sched_spread: self-test FAILED — wrong rejection reason:" >&2
           echo "$out" >&2; exit 1 ;;
    esac

    # A summary that counts CPUs the report never describes.
    cat > "$tmp/truncated.log" <<'EOF'
SCHEDCPU[boot]: cpus=4 online=0xf eligible=0xe
SCHEDCPU[boot]: cpu=0 online=1 eligible=0 switches=3 ticks=10 idle=1 pulled=0 pushed=0
EOF
    if out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/truncated.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted a report missing per-CPU rows" >&2
        exit 1
    fi
    case "$out" in
        *"no per-CPU row"*) ;;
        *) echo "check_sched_spread: self-test FAILED — wrong rejection reason:" >&2
           echo "$out" >&2; exit 1 ;;
    esac

    # A 64-CPU mask: signed shifts would not terminate here.
    cat > "$tmp/wide.log" <<'EOF'
SCHEDCPU[boot]: cpus=64 online=0xffffffffffffffff eligible=0xffffffffffffffff
EOF
    out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/wide.log" --emit-allowlist 2>&1 )
    case "$out" in
        *"min-online	2"*) ;;
        *) echo "check_sched_spread: self-test FAILED — 64-CPU mask not handled:" >&2
           echo "$out" >&2; exit 1 ;;
    esac

    # A log with no SCHEDCPU lines at all is a failure, not a pass.
    echo "nothing to see here" > "$tmp/empty.log"
    if out=$( "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/empty.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — accepted a log with no SCHEDCPU lines" >&2
        exit 1
    fi

    # --emit-allowlist output must round-trip through the check path.
    "$0" --variant tests --gate-data-dir "$tmp/gates" --log "$tmp/good.log" --emit-allowlist \
        > "$tmp/gates/roundtrip.txt" 2>/dev/null
    if ! out=$( "$0" --variant roundtrip --gate-data-dir "$tmp/gates" --log "$tmp/good.log" 2>&1 ); then
        echo "check_sched_spread: self-test FAILED — emitted allowlist rejects its own input:" >&2
        echo "$out" >&2
        exit 1
    fi

    echo "check_sched_spread: self-test OK"
}

[ "$SELF_TEST" -eq 1 ] && self_test && exit 0

CAPTURE=""
if [ -z "$LOG" ]; then
    CAPTURE="$REPO_ROOT/builddir/sched-spread.log"
    boot_and_capture "$CAPTURE"
    LOG="$CAPTURE"
fi

parse_log "$LOG"

if [ "$EMIT" -eq 1 ]; then
    emit_allowlist
    exit 0
fi

GATE_FILE="$GATE_DATA_DIR/$VARIANT.txt"
if [ ! -f "$GATE_FILE" ]; then
    echo "check_sched_spread: no gate data at $GATE_FILE" >&2
    echo "  Measure it: scripts/check_sched_spread.sh --variant $VARIANT --emit-allowlist" >&2
    exit 2
fi

grade "$GATE_FILE"
