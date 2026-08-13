#!/usr/bin/env bash
# Keep the resource-accounting token linear in practice.
#
# `Charge<A>` has a `Drop` that refunds, is not `Clone`, and can only be minted
# by consuming a `Reservation`. That gives unforgeability and no-double-refund
# for free. What it does NOT give is linearity: Rust is affine, so a value's
# destructor can be skipped by entirely safe code.
#
# That escape is live in this tree, not hypothetical. `drivers/src/irq.rs:57-58`
# and `drivers/src/touchpad/mod.rs:283-284` each call `core::mem::forget` twice,
# inside a `#![forbid(unsafe_code)]` crate. Nothing stops the same call being
# written against a charge-bearing value, and the effect would be a row that is
# permanently short and a principal that is permanently denied.
#
# So the invariant is narrower than "a missing refund is unrepresentable",
# which would be false and must not be claimed anywhere:
#
#     A `Charge` lives in exactly one place for exactly the lifetime of the
#     thing it accounts for.
#
# This gate is what keeps that true. Five checks, each naming a specific way
# the token could be separated from the object it accounts for:
#
#   forget       mem::forget / ManuallyDrop / .leak() applied to a
#                charge-bearing binding
#   optional     an `Option<Charge<_>>` field \u2014 `Option::take` is a safe
#                separation, so the empty state must be a distinct type
#   take         `.take()` on a field whose name says it holds a charge
#   escape       a non-mint `fn` returning `Charge<_>` by value
#   clone        a `#[derive(...)]` carrying Clone/Copy on a charge-bearing
#                struct
#
#     scripts/check_charge_linearity.sh
#     scripts/check_charge_linearity.sh --self-test

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_charge_linearity "$@"

# ---------------------------------------------------------------------------
# Allowlist. Each entry is `<check>:<path>` and must match something, or the
# gate fails: a dead exemption is how a scanner quietly stops covering a file.
#
# `token.rs` is the mint itself. Its `into_parts` helpers are the *only*
# sanctioned `mem::forget` of a token in the tree — they carry a debit forward
# from a `Reservation` into a `Charge`, or out of a `Charge` into an explicit
# refund, rather than releasing and re-taking it. `commit`, `try_extend`,
# `shrink` and `try_alias` are the minters, so their return types are expected.
# ---------------------------------------------------------------------------
ALLOWLIST='
forget:slopos-ostd/src/process/quota/token.rs
escape:slopos-ostd/src/process/quota/token.rs
escape:fs/src/fileio/mod.rs
'

OUT_OF_SCOPE_RE='^(userland|terminal-core|slibc|slop-protocol|appkit|image|slopos-rt|vendor/)'

# A binding is charge-bearing if its name or its type mentions a charge or a
# reservation. Deliberately name-based as well as type-based: the field that
# matters most is often bound to a local whose type is inferred.
CHARGE_RE='(Charge|Reservation)[[:space:]]*<|_charge|charge_|slot_charge|object_charge'

# The escape and the type are rarely on one line: `core::mem::forget(c)` names
# a binding whose `Charge` type was declared in the signature above it. So this
# collects charge-bearing binding names in a first pass and matches the
# argument of the escape against them in a second.
scan_forget() {
    local root="$1" file
    cd "$root"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        awk -v fname="$file" -v charge="$CHARGE_RE" '
            function record(text,   n, parts, i, name) {
                # `<name>: Charge<..>` / `<name>: Reservation<..>` in a
                # parameter, a field or a let binding.
                while (match(text, /[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:[[:space:]]*(Charge|Reservation)[[:space:]]*</)) {
                    name = substr(text, RSTART, RLENGTH)
                    sub(/[[:space:]]*:.*/, "", name)
                    bound[name] = 1
                    text = substr(text, RSTART + RLENGTH)
                }
            }
            FNR == NR { record($0); next }
            {
                line = $0; sub(/\/\/.*/, "", line)
                if (line !~ /(mem::forget|ManuallyDrop|\.leak\(\))/) next
                if (line ~ charge) {
                    printf "forget\t%s:%d: %s\n", fname, FNR, substr($0, 1, 110)
                    next
                }
                for (name in bound) {
                    if (line ~ ("(^|[^A-Za-z0-9_])" name "([^A-Za-z0-9_]|$)")) {
                        printf "forget\t%s:%d: %s\n", fname, FNR, substr($0, 1, 110)
                        next
                    }
                }
            }
        ' "$file" "$file"
    done <<< "$2"
}

# An `Option<Charge<_>>` **field**, where `Option::take` is a safe separation
# of the token from its object.
#
# A `-> Option<Reservation<_>>` *return* is deliberately accepted: that is the
# ordinary fallible-mint shape, and the `None` arm means no debit was taken at
# all rather than a charge that went missing. Matching both would make the gate
# fire on every correct mint, which is how a gate gets switched off.
scan_optional() {
    local root="$1" file
    cd "$root"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            { line = $0; sub(/\/\/.*/, "", line) }
            line ~ /->/ { next }
            line ~ /Option[[:space:]]*<[[:space:]]*(Charge|Reservation)[[:space:]]*</ {
                printf "optional\t%s:%d: %s\n", fname, NR, substr($0, 1, 110)
            }
        ' "$file"
    done <<< "$2"
}

# `.take()` on a charge-bearing field.
#
# The shape being kept out is `Option<Charge<_>>::take`, which separates the
# token from the object it accounts for and leaves the object uncharged.
#
# `ChargeSlot::take` is the opposite and is accepted: the slot IS the charge's
# single home for a kind whose refund point is not its holder's `Drop` (a task
# at the exit latch, a process at the reap), and taking from it refunds. It is
# distinguished by the declared field type rather than by the call, so a field
# would have to actually be a `ChargeSlot` to be accepted.
scan_take() {
    local root="$1" file
    cd "$root"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            function note_slots(text,   name) {
                while (match(text, /[A-Za-z_][A-Za-z0-9_]*[[:space:]]*:[[:space:]]*(quota::)?ChargeSlot[[:space:]]*</)) {
                    name = substr(text, RSTART, RLENGTH)
                    sub(/[[:space:]]*:.*/, "", name)
                    slot[name] = 1
                    text = substr(text, RSTART + RLENGTH)
                }
            }
            FNR == NR { note_slots($0); next }
            {
                line = $0; sub(/\/\/.*/, "", line)
                if (line !~ /(_charge|charge_|Charge|Reservation)[A-Za-z0-9_]*[[:space:]]*\.[[:space:]]*take[[:space:]]*\(/) next
                for (name in slot) {
                    if (line ~ ("(^|[^A-Za-z0-9_])" name "[[:space:]]*\\.[[:space:]]*take")) next
                }
                if (line ~ /ChargeSlot|[[:space:]]slot[[:space:]]*\.[[:space:]]*take/) next
                printf "take\t%s:%d: %s\n", fname, FNR, substr($0, 1, 110)
            }
        ' "$file" "$file"
    done <<< "$2"
}

# A `fn` whose return type is a bare `Charge<_>`. The minters are allowlisted
# by file; everything else handing one back by value is a way to separate the
# token from its object.
scan_escape() {
    local root="$1" file
    cd "$root"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            { line = $0; sub(/\/\/.*/, "", line) }
            line ~ /fn[[:space:]]/ && line ~ /->[[:space:]]*Charge[[:space:]]*</ {
                printf "escape\t%s:%d: %s\n", fname, NR, substr($0, 1, 110)
            }
        ' "$file"
    done <<< "$2"
}

# A `#[derive(...)]` carrying Clone or Copy within three lines of a
# charge-bearing field. A cloned charge refunds twice.
scan_clone() {
    local root="$1" file
    cd "$root"
    while IFS= read -r file; do
        [ -f "$file" ] || continue
        awk -v fname="$file" '
            /^[[:space:]]*#\[derive\(/ {
                derive_line = NR
                has_copy = ($0 ~ /(Clone|Copy)/)
                next
            }
            {
                line = $0; sub(/\/\/.*/, "", line)
                if (has_copy && NR - derive_line <= 6 &&
                    line ~ /:[[:space:]]*(Charge|Reservation)[[:space:]]*</) {
                    printf "clone\t%s:%d: %s\n", fname, NR, substr($0, 1, 110)
                    has_copy = 0
                }
                if (line ~ /^[[:space:]]*}/) { has_copy = 0 }
            }
        ' "$file"
    done <<< "$2"
}

scan_all() {
    local root="$1" list
    list="$(gate_collect_rs_files "$root" | grep -Ev "$OUT_OF_SCOPE_RE" || true)"
    gate_require_nonempty check_charge_linearity "$root" "$list"
    scan_forget "$root" "$list"
    scan_optional "$root" "$list"
    scan_take "$root" "$list"
    scan_escape "$root" "$list"
    scan_clone "$root" "$list"
}

# Drop findings the allowlist covers, and report which entries matched so a
# dead one can fail.
declare -a ALLOW_HIT=()
apply_allowlist() {
    local findings="$1" entry idx=0 kept="" line matched
    local -a entries=()
    while IFS= read -r entry; do
        [ -z "${entry// }" ] && continue
        entries+=("$entry")
        ALLOW_HIT+=(0)
    done <<< "$ALLOWLIST"

    while IFS= read -r line; do
        [ -z "$line" ] && continue
        matched=0
        idx=0
        for entry in "${entries[@]}"; do
            if [ "$(printf '%s' "$line" | cut -f1)" = "${entry%%:*}" ] &&
               case "$(printf '%s' "$line" | cut -f2)" in "${entry#*:}"*) true ;; *) false ;; esac
            then
                ALLOW_HIT[$idx]=1
                matched=1
                break
            fi
            idx=$((idx + 1))
        done
        [ "$matched" -eq 0 ] && kept+="$line"$'\n'
    done <<< "$findings"

    ALLOW_ENTRIES=("${entries[@]}")
    printf '%s' "$kept"
}

if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_charge_linearity
    root="$GATE_FIXTURE_ROOT"
    mkdir -p "$root/src"

    # Must fire: one per check.
    cat > "$(gate_fixture src/positives.rs)" <<'RS'
struct Leaky {
    slot_charge: Charge<FdSlot>,
}

fn escapes(c: Charge<FdSlot>) -> Charge<FdSlot> { c }

struct Optional {
    slot_charge: Option<Charge<FdSlot>>,
}

fn drops_it(c: Charge<FdSlot>) {
    core::mem::forget(c);
}

fn steals(e: &mut Optional) {
    let _ = e.slot_charge.take();
}

#[derive(Clone)]
struct Cloneable {
    slot_charge: Charge<FdSlot>,
}
RS

    # Must not fire.
    cat > "$(gate_fixture src/negatives.rs)" <<'RS'
// A forget of something that is not a charge: the live escape this gate cites,
// which must stay accepted or the gate cries wolf on real driver code.
fn irq_claim(handle: Handle, line: Line) {
    core::mem::forget(handle);
    core::mem::forget(line);
}

// A charge stored by value with no Clone in sight.
struct Proper {
    slot_charge: Charge<FdSlot>,
}

// Borrowing a charge is fine: it cannot be separated from its owner.
impl Proper {
    fn object_charge(&self) -> &Charge<ObjectRow> { &self.other }
}

// An Option of something else entirely.
struct Unrelated {
    backing: Option<KArc<dyn FileBacking>>,
}

// A take on a field that is not a charge.
fn take_entry(slot: &mut Slot) {
    let _ = slot.descriptors.take();
}

// `ChargeSlot::take` IS the sanctioned release for a kind whose refund point
// is not its holder's Drop — the slot is the charge's single home, and taking
// from it refunds.
struct Reaped {
    proc_charge: quota::ChargeSlot<ProcCount>,
}

impl Reaped {
    fn release(&self) {
        self.proc_charge.take();
    }
}

// The ordinary fallible mint: `None` means no debit was taken, not a charge
// that went missing.
fn reserve(account: AccountId) -> Option<Reservation<TaskCount>> {
    try_charge::<TaskCount>(account, 1).ok()
}

// Clone on a struct with no charge field.
#[derive(Clone, Copy)]
struct Flags {
    cloexec: bool,
}
RS

    GATE_FINDINGS="$(scan_all "$root")"
    gate_expect forget 1 "mem::forget of a charge-bearing binding"
    gate_expect optional 1 "an Option<Charge<_>> field"
    gate_expect take 1 "a .take() on a charge field"
    gate_expect escape 1 "a non-mint fn returning Charge<_> by value"
    gate_expect clone 1 "a derived Clone on a charge-bearing struct"
    gate_expect_silent 'negatives\.rs' \
        "the live driver mem::forget pair, a by-value charge field, a borrowing accessor, an unrelated Option/take, Clone on a charge-free struct, a fallible mint returning Option<Reservation>, and ChargeSlot::take"
    gate_selftest_end
fi

findings="$(scan_all "$REPO_ROOT")"
kept="$(apply_allowlist "$findings")"

fail=0
idx=0
for entry in "${ALLOW_ENTRIES[@]}"; do
    if [ "${ALLOW_HIT[$idx]}" -eq 0 ]; then
        echo "check_charge_linearity: allowlist entry '$entry' matched nothing." >&2
        echo "  A dead exemption is how a scan quietly stops covering a file. Remove it." >&2
        fail=1
    fi
    idx=$((idx + 1))
done

if [ -n "${kept// }" ]; then
    echo "check_charge_linearity: a resource charge can be separated from what it accounts for:" >&2
    printf '%s' "$kept" | sed 's/^/  /' >&2
    echo >&2
    echo "  A Charge must live in exactly one place for exactly the lifetime of the" >&2
    echo "  thing it accounts for. Never Option<Charge<_>> — use a distinct uncharged" >&2
    echo "  type for the empty state. Never hand one back by value from anything but" >&2
    echo "  a mint. Never Clone one: a cloned charge refunds twice." >&2
    fail=1
fi

[ "$fail" -ne 0 ] && exit 1
echo "check_charge_linearity: OK — every charge is bound to the object it accounts for"
