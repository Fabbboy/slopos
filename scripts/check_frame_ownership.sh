#!/usr/bin/env bash
# Fail if a page can be freed twice across a refused map.
#
# A `PhysAddr` is a number. It says nothing about who owns the page, so a
# function that takes one, mints the owning `UFrame` internally, and can then
# fail has performed a transfer it cannot reverse: the refusal drops the frame
# — which frees the page — while the caller, holding only the number, frees it
# again. That was `ostd_map_4kb_user`, reachable from an unprivileged demand
# fault whenever the buddy was empty.
#
# Neither free was observable. `BuddyAllocator::free_phys` returns 0 both for a
# real release and for a page whose descriptor is already PCP/FREE/QUIESCE,
# which is exactly what a second free finds, so the bug was silent in every
# variant. What kept it from corrupting memory was three coincidences — the
# page landing on the freeing CPU's private PCP magazine, every caller holding
# a cli-spinlock, and that state check — not ownership. An allocator policy
# change (`no_pcp`, `dma`) detonates it.
#
# The shape that replaced it:
#
#   ostd_map_4kb_user        takes UFrame, returns it in the error
#   ostd_map_4kb_user_fresh  allocates, maps, frees iff refused
#   ostd_map_4kb_user_shared takes a paddr and *aliases* it
#
# ---------------------------------------------------------------------------
# The two checks
# ---------------------------------------------------------------------------
#
#   1. No fallible step between a claim and the PTE store that consumes it.
#      `claim_user_paddr` mints the sole owning ref; from there to
#      `cursor.map(...)` the frame is live in a local, so a `?` or a bare
#      `return Err` in between drops it and frees the page under the caller.
#      This is stated over *distance from the claim* rather than over the
#      parameter type, because a `PhysAddr` parameter is correct for the
#      alias-class entry points (`_shared`, `ostd_map_ring_4kb_user`) whose
#      caller genuinely has no handle to give.
#
#   2. No `free_page_frame` in the fault and map paths. Those release through
#      the owning handle: on the map paths the leaf PTE owns the page, and on
#      a refusal the returned frame's `Drop` is the free. A raw free there is
#      either the second one or a release of a page the PTE still holds.
#      `demand.rs` and `cow.rs` are scanned only *after* their claim, since
#      both correctly free a just-allocated page when the claim itself fails —
#      no MetaSlot ref exists yet at that point.
#
#   3. No constructor that decides claim-vs-alias at run time. The original
#      `wrap_user_paddr` tried `from_unused` and fell through to `from_in_use`
#      on `StateMismatch`, so one call site meant "claim a fresh page" or
#      "alias a live one" depending on the slot — and the two differ in who
#      frees the page. Without this check the first two are evadable by simply
#      not naming `claim_user_paddr`, which is what the pre-fix tree did.
#
# Deliberately accepted, and asserted in the self-test:
#   - `_shared` / ring entry points taking a `PhysAddr` and aliasing it.
#   - `free_page_frame` before the claim (the allocate-then-claim-failed arm).
#   - `alias_user_paddr` followed by a fallible step: dropping an alias
#     releases only that ref, and the origin still holds the page.
#   - `from_unused` / `from_in_use` used on their own, which is how
#     `claim_user_paddr` and `alias_user_paddr` are each defined.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
# shellcheck source=lib/gate_common.sh
. "$SCRIPT_DIR/lib/gate_common.sh"
gate_parse_args check_frame_ownership "$@"

# Files that mint an owning user-page handle, or sit on a map error path.
CLAIM_PATHS='mm/src/user_mappings.rs mm/src/demand.rs mm/src/cow.rs'
# Files whose map paths must release only through the owning handle.
NOFREE_PATHS='mm/src/process_vm.rs mm/src/user_mappings.rs'

# ---------------------------------------------------------------------------
# Check 1 — a fallible step between the claim and the PTE store.
#
# From a `claim_user_paddr(` to the `map(`/`replace(` that consumes the frame,
# no `?` and no `return Err`. The window closes at the consuming call, or
# after 40 lines (no claim-to-map span in the tree is near that).
# ---------------------------------------------------------------------------
scan_claim_window() {
    local root="$1" path
    cd "$root"
    for path in $CLAIM_PATHS; do
        [ -e "$path" ] || continue
        awk -v file="$path" '
            /claim_user_paddr[[:space:]]*\(/ { inwin = 1; start = NR; inerr = 0; next }
            inwin {
                # The claim`s own failure arm runs before any frame exists, so
                # its free and its early return are the caller correctly
                # releasing a page no MetaSlot ref covers. Skip to its close.
                if ($0 ~ /Err\(.*\)[[:space:]]*=>/) { inerr = 1; next }
                if (inerr) {
                    if ($0 ~ /^[[:space:]]*\}[,;]?[[:space:]]*$/) { inerr = 0 }
                    next
                }
                # The consuming call closes the window.
                if ($0 ~ /(cursor|cur)\.(map|replace)(::|[[:space:]]*\()/ ||
                    $0 ~ /ostd_map_4kb_user[[:space:]]*\(/ ||
                    $0 ~ /ostd_replace_4kb_user[[:space:]]*\(/) { inwin = 0; next }
                if ($0 ~ /\?;[[:space:]]*$/ || $0 ~ /\?[[:space:]]*$/ ||
                    $0 ~ /^[[:space:]]*return Err/) {
                    printf "claimwin\t%s:%d: %s\n", file, NR, substr($0, 1, 90)
                    inwin = 0
                    next
                }
                if (NR - start > 40) { inwin = 0 }
            }
        ' "$path"
    done
}

# ---------------------------------------------------------------------------
# Check 2 — a raw free on a map path.
#
# In `process_vm.rs` any `free_page_frame` is a finding: every page it maps is
# owned by a leaf or by a returned frame. In `user_mappings.rs` the same, with
# the `_fresh` helper's post-claim-failure free excluded by check 1's rule
# (it precedes the claim's success, not follows it).
# ---------------------------------------------------------------------------
scan_raw_frees() {
    local root="$1" path
    cd "$root"
    for path in $NOFREE_PATHS; do
        [ -e "$path" ] || continue
        awk -v file="$path" '
            /^[[:space:]]*\/\// { next }
            /free_page_frame[[:space:]]*\(/ {
                # A free guarding a failed claim is the caller releasing a page
                # no MetaSlot ref covers yet.
                if (prev ~ /claim_user_paddr/ || prev ~ /Err\(e\)[[:space:]]*=>/ ||
                    prev2 ~ /claim_user_paddr/) { prev2 = prev; prev = $0; next }
                printf "rawfree\t%s:%d: %s\n", file, NR, substr($0, 1, 90)
            }
            { prev2 = prev; prev = $0 }
        ' "$path"
    done
}

# ---------------------------------------------------------------------------
# Check 3 — a constructor that falls through from claim to alias.
#
# The shape being kept out: a `from_unused` whose `StateMismatch` arm answers
# with `from_in_use` for the same paddr. Scanned across OSTD's frame surface
# and the mm mapping helpers, since either could regrow it.
# ---------------------------------------------------------------------------
FALLTHROUGH_PATHS='slopos-ostd/src/mm/uframe.rs slopos-ostd/src/mm/frame.rs mm/src/user_mappings.rs'

scan_claim_alias_fallthrough() {
    local root="$1" path
    cd "$root"
    for path in $FALLTHROUGH_PATHS; do
        [ -e "$path" ] || continue
        awk -v file="$path" '
            /^[[:space:]]*\/\// { next }
            /from_unused[[:space:]]*\(/ { seen = NR }
            seen && NR - seen <= 6 && /StateMismatch/ && /from_in_use/ {
                printf "fallthrough\t%s:%d: %s\n", file, NR, substr($0, 1, 90)
                seen = 0
            }
        ' "$path"
    done
}

run_scan() {
    local root="$1"
    {
        scan_claim_window "$root"
        scan_raw_frees "$root"
        scan_claim_alias_fallthrough "$root"
    } | sort -u
}

# ---------------------------------------------------------------------------
# Self-test
# ---------------------------------------------------------------------------
if [ "$GATE_SELF_TEST" -eq 1 ]; then
    gate_selftest_begin check_frame_ownership

    # A planted fallible step between the claim and the map — the original
    # defect, in both of its reachable spellings.
    cat > "$(gate_fixture mm/src/user_mappings.rs)" <<'RS'
pub fn bad_map_question(vm: &mut KArc<VmSpace>, va: VirtAddr, pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(pa.as_u64()))?;
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}

pub fn good_shared(vm: &mut KArc<VmSpace>, va: VirtAddr, pa: PhysAddr) -> Result<(), MapError> {
    let frame = UFrame::<AnonymousMeta>::alias_user_paddr(Paddr::new(pa.as_u64()))?;
    let mut cursor = vs.cursor_mut(range)?;
    cursor.map::<Size4Kb, AnonymousMeta>(frame, prop)
}
RS
    cat > "$(gate_fixture mm/src/demand.rs)" <<'RS'
fn bad_map_return(vm: &mut KArc<VmSpace>) -> Result<(), MmError> {
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(pa) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(phys);
            return Err(MmError::MappingFailed);
        }
    };
    if !region.is_ok() {
        return Err(MmError::PermissionDenied);
    }
    ostd_map_4kb_user(vm, va, frame, flags)
}
RS
    # A planted raw free on a map path.
    cat > "$(gate_fixture mm/src/process_vm.rs)" <<'RS'
fn map_user_range(vm: &mut KArc<VmSpace>) -> Result<u32, c_int> {
    if let Err(err) = ostd_map_4kb_user_fresh(vm, VirtAddr::new(current), map_flags) {
        free_page_frame(phys);
        return Err(-1);
    }
    Ok(1)
}
RS
    cat > "$(gate_fixture mm/src/cow.rs)" <<'RS'
fn ok_claim_failure_free(vm: &mut KArc<VmSpace>) -> Result<(), MmError> {
    let frame = match UFrame::<AnonymousMeta>::claim_user_paddr(Paddr::new(new_phys.as_u64())) {
        Ok(f) => f,
        Err(e) => {
            free_page_frame(new_phys);
            return Err(MmError::MappingFailed);
        }
    };
    ostd_replace_4kb_user(vm, va, frame, flags)
}
RS

    # A planted claim/alias fall-through: the constructor this replaced.
    cat > "$(gate_fixture slopos-ostd/src/mm/uframe.rs)" <<'RS'
    pub fn wrap_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        match Frame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default()) {
            Ok(frame) => Ok(Self(frame)),
            Err(FrameError::StateMismatch) => Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?)),
            Err(e) => Err(e),
        }
    }

    pub fn claim_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_unused(paddr, AnonymousMeta::default())?))
    }

    pub fn alias_user_paddr(paddr: Paddr) -> Result<Self, FrameError> {
        Ok(Self(Frame::<AnonymousMeta>::from_in_use(paddr)?))
    }
RS

    GATE_FINDINGS="$(run_scan "$GATE_FIXTURE_ROOT")"
    # `bad_map_question`'s cursor_mut `?`, and `bad_map_return`'s explicit
    # error return. The `cow.rs` claim-failure arm returns Err too, but it is
    # inside the claim's own `Err(e) =>` arm, before a frame exists.
    gate_expect claimwin 2 "a fallible step between claim and PTE store"
    gate_expect rawfree 1 "a raw free on a map path"
    gate_expect fallthrough 1 "a claim/alias fall-through constructor"

    # Negatives: the alias path, a free that guards a failed claim, and the
    # two split constructors each using one half on its own.
    gate_expect_silent 'good_shared|ok_claim_failure_free' \
        "alias-class maps and the claim-failure free"
    gate_expect_silent 'claim_user_paddr\(paddr|alias_user_paddr\(paddr' \
        "the split constructors"

    gate_selftest_end
fi

# ---------------------------------------------------------------------------
# Real scan
# ---------------------------------------------------------------------------
GATE_FINDINGS="$(run_scan "$REPO_ROOT")"

if [ -n "$GATE_FINDINGS" ]; then
    echo "check_frame_ownership: a refused map must not free the caller's page:" >&2
    printf '%s\n' "$GATE_FINDINGS" | sed 's/^[a-z]*\t/  /' >&2
    cat >&2 <<'MSG'

  Between `claim_user_paddr` and the PTE store that consumes the frame, the
  frame is live in a local: a `?` there drops it, which frees the page, while
  the caller still holds the paddr it passed in and frees it again. Move the
  fallible step ahead of the claim, or take the owning `UFrame` as a parameter
  so the refusal can hand it back.

  On a map path, release through the owning handle — the leaf PTE on success,
  the returned frame's `Drop` on refusal. A raw `free_page_frame` there is
  either the second free or a release of a page the page table still holds.
MSG
    exit 1
fi

echo "check_frame_ownership: OK — no fallible step between a page claim and its"
echo "check_frame_ownership: PTE store, and no raw free on a map path"
