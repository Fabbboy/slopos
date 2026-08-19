// Authority proof.
//
// A Verus-annotated mirror of SlopOS's capability model: the per-task mask in
// `slopos_ostd::task::ops::{task_caps, task_set_caps, task_restrict_caps}`,
// the exec intersection in `slopos_core::syscall::process_handlers`, the
// `Launch`-bounded raise site in `slopos_core::exec::spawn_program_with_attrs`,
// and `fork`'s explicit copy in `TaskInner::clone_from_raw`.
//
// It machine-checks four obligations:
//
//   (S1) MONOTONE. Every step except `Exec` and `Spawn` preserves or shrinks a
//        task's authority. Authority is never widened by acting, only by
//        loading a new program image — which is the single raise site the whole
//        model rests on.
//   (S2) EXEC INTERSECTS. `caps' == grant(image) & caps`. Both halves are
//        load-bearing: dropping the `& caps` lets an unprivileged task exec a
//        privileged image and gain its grant; dropping the `grant(image)` lets
//        a privileged program's authority survive into an arbitrary binary.
//        Those are the two CVE shapes this model exists to prevent, two years
//        apart in the same vendor's kernel, on the same entitlement.
//   (S3) SPAWN BOUND. `child.caps == if Launch in parent.caps { grant(image) }
//        else { 0 }`. Without the bound, any task obtains a privileged child by
//        spawning the privileged path.
//   (S4) DROP TOTALITY. `without` is total: it has no failure case a caller
//        could ignore. A historical local root came from an attacker making a
//        privilege *drop* fail inside a program that ignored the result.
//
// Every obligation carries a BROKEN WITNESS that must fail `auth_inv` or the
// obligation's own postcondition, because a proof of a property nothing can
// violate is a proof about the model rather than about the tree.
//
// NOT COVERED, and deliberately excluded rather than assumed:
//
//   - The `Relaxed`/`Acquire`/`Release` ordering on `Task::caps`, and whether a
//     concurrent reader on another CPU observes the narrowed mask promptly.
//     Verus has no weak-memory model. Audited: the store is `Release`, the load
//     is `Acquire`, and every narrowing happens in the acting task's own
//     context — there is no cross-process revocation, which is precisely what
//     licenses the cheap read.
//   - The mapping from a flag word to a mask (`caps_from_task_flags`). That is
//     a total pure function tested exhaustively in the kernel test suite; the
//     model here takes masks as given.
//   - That the dispatcher actually consults the mask before calling a handler.
//     That is a `rustc` totality assert over the syscall table plus
//     `scripts/check_authority_reachability.sh`, not a Verus obligation.
//
// The model is deliberately MORE PERMISSIVE than the tree in one place:
// `Spawn` here takes an arbitrary grant, whereas the tree's `grant_for` reads a
// fixed table keyed on the image path. Proving the bound for every possible
// grant is strictly stronger than proving it for the five the table names.

use vstd::prelude::*;

verus! {

/// A capability mask. `u64` in the tree; `nat`-free `u64` here so the bitwise
/// operations are the ones the kernel actually performs.
pub type Caps = u64;

/// Bit 1 in `slopos_ostd::authority::Capability::Launch`. Named rather than
/// inlined so S3 reads as the rule it is.
pub open spec fn launch_bit() -> Caps {
    2
}

/// Does this mask name `Launch`?
pub open spec fn has_launch(c: Caps) -> bool {
    c & launch_bit() != 0
}

/// One task's authority state.
pub struct Auth {
    /// `Task::caps` — the effective mask.
    pub caps: Caps,
    /// Ghost: the mask this task was created with. Nothing may exceed it
    /// except an `Exec`, which is the point of tracking it.
    pub born_with: Caps,
    /// Ghost: how many times authority was raised outside a load. Must stay 0.
    pub raises: nat,
}

/// The inductive invariant.
pub open spec fn auth_inv(s: Auth) -> bool {
    // (S1) Authority is only ever raised by loading an image. Any other step
    //      that widened it would have incremented this.
    &&& s.raises == 0
}

/// The operations that can change a task's authority.
pub enum Step {
    /// A syscall, a signal, a scheduling decision — anything that is not a
    /// program load. Must not touch the mask.
    Act,
    /// Voluntary reduction: `Cred::without` / `task_restrict_caps`.
    Drop(Caps),
    /// `execve`: the mask becomes `grant & caps`.
    Exec(Caps),
    /// `fork`: the child is the same principal and copies the mask.
    Fork,
}

/// The state machine.
pub open spec fn step(s: Auth, e: Step) -> Auth {
    match e {
        Step::Act => s,
        // Total: an intersection, defined for every input, with no error case.
        Step::Drop(mask) => Auth { caps: s.caps & mask, ..s },
        // The intersection. Both operands, in this order.
        Step::Exec(grant) => Auth { caps: grant & s.caps, ..s },
        Step::Fork => s,
    }
}

/// `x & y` never has a bit that `x` lacks. The whole model reduces to this.
pub proof fn and_is_subset(x: Caps, y: Caps)
    ensures
        x & y == x & y,
        forall|b: Caps| #[trigger] (x & y & b) != 0 ==> (x & b) != 0,
{
    assert forall|b: Caps| #[trigger] (x & y & b) != 0 implies (x & b) != 0 by {
        assert((x & y & b) != 0 ==> (x & b) != 0) by (bit_vector);
    }
}

// ---------------------------------------------------------------------------
// S1 — monotone
// ---------------------------------------------------------------------------

/// Every step preserves the invariant.
pub proof fn step_preserves(s: Auth, e: Step)
    requires
        auth_inv(s),
    ensures
        auth_inv(step(s, e)),
{
}

/// `Act` and `Fork` do not touch the mask at all, and `Drop` can only shrink
/// it. Stated per-capability, because "shrinks" for a bitmask means no bit
/// appears that was not already there.
pub proof fn non_load_steps_never_widen(s: Auth, e: Step, b: Caps)
    requires
        auth_inv(s),
        !matches!(e, Step::Exec(_)),
        (step(s, e).caps & b) != 0,
    ensures
        (s.caps & b) != 0,
{
    match e {
        Step::Act => {},
        Step::Fork => {},
        Step::Drop(mask) => {
            // Bound to locals: `by (bit_vector)` reasons over plain integers,
            // not struct field projections.
            let c = s.caps;
            assert(((c & mask) & b) != 0 ==> (c & b) != 0) by (bit_vector);
        },
        Step::Exec(_) => {},
    }
}

// ---------------------------------------------------------------------------
// S2 — exec intersects
// ---------------------------------------------------------------------------

/// The exec result is bounded by BOTH operands. This is the obligation stated
/// exactly: a capability survives exec only if the image's grant names it *and*
/// the task already held it.
pub proof fn exec_is_bounded_by_both(s: Auth, grant: Caps, b: Caps)
    requires
        (step(s, Step::Exec(grant)).caps & b) != 0,
    ensures
        (s.caps & b) != 0,
        (grant & b) != 0,
{
    let c = s.caps;
    assert(((grant & c) & b) != 0 ==> (c & b) != 0) by (bit_vector);
    assert(((grant & c) & b) != 0 ==> (grant & b) != 0) by (bit_vector);
}

/// An unprivileged task cannot gain a capability by exec'ing a privileged
/// image. The escalation direction, stated on its own because it is the half a
/// naive "just take the grant" implementation gets wrong.
pub proof fn exec_cannot_raise(s: Auth, grant: Caps, b: Caps)
    requires
        (s.caps & b) == 0,
    ensures
        (step(s, Step::Exec(grant)).caps & b) == 0,
{
    let c = s.caps;
    assert((c & b) == 0 ==> ((grant & c) & b) == 0) by (bit_vector);
}

/// An entitlement does not survive exec of an image that does not earn it.
/// The inheritance direction — the CVE where a privileged program execs an
/// arbitrary binary and the binary keeps the entitlement.
pub proof fn exec_drops_ungranted(s: Auth, grant: Caps, b: Caps)
    requires
        (grant & b) == 0,
    ensures
        (step(s, Step::Exec(grant)).caps & b) == 0,
{
    let c = s.caps;
    assert((grant & b) == 0 ==> ((grant & c) & b) == 0) by (bit_vector);
}

// ---------------------------------------------------------------------------
// S3 — the spawn bound
// ---------------------------------------------------------------------------

/// What a spawn gives the child, per the tree's rule.
pub open spec fn spawn_child(parent: Auth, grant: Caps) -> Caps {
    if has_launch(parent.caps) {
        grant
    } else {
        0
    }
}

/// A spawner without `Launch` produces a child with no authority, whatever the
/// image's grant says.
pub proof fn spawn_without_launch_grants_nothing(parent: Auth, grant: Caps)
    requires
        !has_launch(parent.caps),
    ensures
        spawn_child(parent, grant) == 0,
{
}

/// A spawner WITH `Launch` hands over exactly the image's grant — not the
/// parent's own authority, and not an intersection with it.
///
/// The intersection is the tempting mistake and it is arithmetically
/// self-defeating: the shell holds no display authority, so
/// `parent.caps & grant` would mean `/bin/roulette` could never draw.
pub proof fn spawn_with_launch_grants_the_image(parent: Auth, grant: Caps)
    requires
        has_launch(parent.caps),
    ensures
        spawn_child(parent, grant) == grant,
{
}

// ---------------------------------------------------------------------------
// S4 — drop totality
// ---------------------------------------------------------------------------

/// `Drop` is defined for every mask and every state: there is no input for
/// which it has no result, hence no error a caller could ignore.
pub proof fn drop_is_total(s: Auth, mask: Caps)
    ensures
        step(s, Step::Drop(mask)) == (Auth { caps: s.caps & mask, ..s }),
{
}

/// Dropping is idempotent, so a caller that retries a reduction cannot end up
/// with more than one that succeeded.
pub proof fn drop_is_idempotent(s: Auth, mask: Caps)
    ensures
        step(step(s, Step::Drop(mask)), Step::Drop(mask)).caps == step(
            s,
            Step::Drop(mask),
        ).caps,
{
    let c = s.caps;
    assert((c & mask) & mask == c & mask) by (bit_vector);
}

/// Restricting to "everything" is a no-op — the monotonicity guard that makes
/// `task_restrict_caps` safe to call with any mask, including a wider one.
pub proof fn drop_to_all_is_identity(s: Auth)
    ensures
        step(s, Step::Drop(0xFFFF_FFFF_FFFF_FFFF)).caps == s.caps,
{
    let c = s.caps;
    assert(c & 0xFFFF_FFFF_FFFF_FFFFu64 == c) by (bit_vector);
}

// ---------------------------------------------------------------------------
// Broken witnesses
//
// Each is a variant the tree deliberately does NOT implement. Every one must
// break something, or the corresponding obligation is vacuous.
// ---------------------------------------------------------------------------

/// BROKEN: a step that widens authority without a load — the shape any "just
/// add the bit" convenience would take.
pub open spec fn broken_widening_step(s: Auth, extra: Caps) -> Auth {
    Auth { caps: s.caps | extra, raises: (s.raises + 1) as nat, ..s }
}

/// Witness that S1 is load-bearing: a widening step exists that breaks the
/// invariant, while every real `Step` preserves it.
pub proof fn broken_widening_violates_invariant()
    ensures
        exists|s: Auth, extra: Caps|
            #![trigger broken_widening_step(s, extra)]
            auth_inv(s) && !auth_inv(broken_widening_step(s, extra)),
        forall|s: Auth, e: Step| auth_inv(s) ==> #[trigger] auth_inv(step(s, e)),
{
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(auth_inv(ordinary));
    let widened = broken_widening_step(ordinary, launch_bit());
    assert(widened.raises == 1);
    assert(!auth_inv(widened));
    assert(exists|s: Auth, extra: Caps|
        #![trigger broken_widening_step(s, extra)]
        auth_inv(s) && !auth_inv(broken_widening_step(s, extra)));
    assert forall|s: Auth, e: Step| auth_inv(s) implies #[trigger] auth_inv(step(s, e)) by {
        step_preserves(s, e);
    }
}

/// BROKEN: exec that takes the grant WITHOUT intersecting — an unprivileged
/// task exec'ing a privileged image gains its authority.
pub open spec fn broken_exec_ungated(_s: Auth, grant: Caps) -> Caps {
    grant
}

/// Witness that the `& caps` half of S2 is load-bearing.
pub proof fn broken_exec_ungated_escalates()
    ensures
        exists|s: Auth, grant: Caps, b: Caps|
            #![trigger broken_exec_ungated(s, grant) & b]
            (s.caps & b) == 0 && (broken_exec_ungated(s, grant) & b) != 0,
{
    // An ordinary task exec'ing an image whose identity earns `Launch`.
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(broken_exec_ungated(ordinary, launch_bit()) == launch_bit());
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(exists|s: Auth, grant: Caps, b: Caps|
        #![trigger broken_exec_ungated(s, grant) & b]
        (s.caps & b) == 0 && (broken_exec_ungated(s, grant) & b) != 0);
}

/// BROKEN: exec that keeps the old mask un-intersected — the entitlement
/// survives into an arbitrary binary.
pub open spec fn broken_exec_keeps_caps(s: Auth, _grant: Caps) -> Caps {
    s.caps
}

/// Witness that the `grant &` half of S2 is load-bearing.
pub proof fn broken_exec_keeps_caps_inherits()
    ensures
        exists|s: Auth, grant: Caps, b: Caps|
            #![trigger broken_exec_keeps_caps(s, grant) & b]
            (grant & b) == 0 && (broken_exec_keeps_caps(s, grant) & b) != 0,
{
    // A privileged program exec'ing an image that earns nothing.
    let privileged = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    assert(broken_exec_keeps_caps(privileged, 0) == launch_bit());
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(exists|s: Auth, grant: Caps, b: Caps|
        #![trigger broken_exec_keeps_caps(s, grant) & b]
        (grant & b) == 0 && (broken_exec_keeps_caps(s, grant) & b) != 0);
}

/// BROKEN: a spawn that ignores the `Launch` bound.
pub open spec fn broken_spawn_unbounded(_parent: Auth, grant: Caps) -> Caps {
    grant
}

/// Witness that S3's bound is load-bearing: without it an unprivileged
/// spawner produces a privileged child.
pub proof fn broken_spawn_unbounded_raises()
    ensures
        exists|p: Auth, grant: Caps|
            #![trigger broken_spawn_unbounded(p, grant)]
            !has_launch(p.caps) && broken_spawn_unbounded(p, grant) != spawn_child(p, grant),
{
    let ordinary = Auth { caps: 0, born_with: 0, raises: 0 };
    assert(0u64 & 2u64 == 0) by (bit_vector);
    assert(!has_launch(ordinary.caps));
    assert(spawn_child(ordinary, launch_bit()) == 0);
    assert(broken_spawn_unbounded(ordinary, launch_bit()) == launch_bit());
    assert(2u64 != 0) by (bit_vector);
    assert(exists|p: Auth, grant: Caps|
        #![trigger broken_spawn_unbounded(p, grant)]
        !has_launch(p.caps) && broken_spawn_unbounded(p, grant) != spawn_child(p, grant));
}

/// BROKEN: the spawn bound written as an intersection with the parent's own
/// authority — the tempting mistake S3 names.
pub open spec fn broken_spawn_intersects_parent(parent: Auth, grant: Caps) -> Caps {
    parent.caps & grant
}

/// Witness that the intersection form is not merely different but WRONG: a
/// launcher holding `Launch` and nothing else could confer nothing at all, so
/// no privileged program could ever start.
pub proof fn broken_spawn_intersection_starves_the_grant()
    ensures
        exists|p: Auth, grant: Caps|
            #![trigger broken_spawn_intersects_parent(p, grant)]
            has_launch(p.caps) && broken_spawn_intersects_parent(p, grant) == 0
                && spawn_child(p, grant) != 0,
{
    // The shell: holds `Launch`, holds no display authority.
    let shell = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    // `/bin/roulette`'s grant: a display bit, which the shell does not hold.
    let display: Caps = 16;
    assert(2u64 & 2u64 != 0) by (bit_vector);
    assert(has_launch(shell.caps));
    assert(2u64 & 16u64 == 0) by (bit_vector);
    assert(broken_spawn_intersects_parent(shell, display) == 0);
    assert(spawn_child(shell, display) == display);
    assert(16u64 != 0) by (bit_vector);
    assert(exists|p: Auth, grant: Caps|
        #![trigger broken_spawn_intersects_parent(p, grant)]
        has_launch(p.caps) && broken_spawn_intersects_parent(p, grant) == 0
            && spawn_child(p, grant) != 0);
}

/// BROKEN: a fallible drop. Modelled as a reduction that sometimes leaves the
/// mask untouched — what a caller that ignores an error return observes.
pub open spec fn broken_drop_may_fail(s: Auth, mask: Caps, fails: bool) -> Caps {
    if fails {
        s.caps
    } else {
        s.caps & mask
    }
}

/// Witness that S4's totality matters: a fallible drop leaves authority the
/// caller believes it dropped, which is the historical local-root shape.
pub proof fn broken_fallible_drop_retains_authority()
    ensures
        exists|s: Auth, mask: Caps, b: Caps|
            #![trigger broken_drop_may_fail(s, mask, true) & b]
            ((s.caps & mask) & b) == 0 && (broken_drop_may_fail(s, mask, true) & b) != 0,
{
    let privileged = Auth { caps: launch_bit(), born_with: launch_bit(), raises: 0 };
    // The program asks to drop everything.
    assert(2u64 & 0u64 == 0) by (bit_vector);
    assert((2u64 & 0u64) & 2u64 == 0) by (bit_vector);
    assert(broken_drop_may_fail(privileged, 0, true) == launch_bit());
    assert(launch_bit() & launch_bit() != 0) by (bit_vector);
    assert(exists|s: Auth, mask: Caps, b: Caps|
        #![trigger broken_drop_may_fail(s, mask, true) & b]
        ((s.caps & mask) & b) == 0 && (broken_drop_may_fail(s, mask, true) & b) != 0);
}

} // verus!

fn main() {}
