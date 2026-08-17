// Resource-ledger proof.
//
// Machine-checks the accounting core of `slopos_ostd::process::quota` — the
// `.bss` account arena (`arena.rs`), the linear `Reservation`/`Charge` tokens
// (`token.rs`), and the hierarchical debit walk `try_charge` performs. The
// property that matters is not "the counter is roughly right": it is that a
// *refused* charge is the identity on every row, including the batch that
// succeeded at level k and failed at k+1, which is the case a hand-written
// cancel loop gets wrong.
//
// Six obligations:
//
//   (L1) EQUALITY, not inequality. `used[i] == live_sum[i]` at every level:
//        the row holds exactly the sum of the charges outstanding against it.
//        An inequality (`used >= live`) cannot catch a *phantom refund* — a
//        refund with no charge behind it — which is the failure the linear
//        token exists to eliminate, so the obligation is stated as equality
//        and nothing weaker.
//
//   (L2) AS A STEP PROPERTY. No successful `try_charge` leaves `used > limit`.
//        Deliberately not the global form `forall states. used <= limit`,
//        which is false the instant `LowerLimit` exists — an operator lowering
//        a ceiling below what is already held does not retroactively refuse
//        the outstanding charges, and cannot. The honest claim is about the
//        step, and it is the claim the ceiling actually makes.
//
//   (L3a) NO DOUBLE REFUND. A charge is refunded at most once. Delivered in
//        the tree by the by-value consume (`Charge` is not `Clone`, and
//        `release` takes `self`); modelled here as a per-charge `live` flag
//        that only a transition can clear. This is the half that matters,
//        because a double refund is the *under*-count: it makes a principal
//        look emptier than it is and hands out headroom that does not exist.
//
//   (L4) A DENIED CHARGE IS IDENTITY ON EVERY ROW, including the partial
//        batch. This is the hard one and the reason `Reservation` exists as a
//        separate type from `Charge`.
//
//   (L6) `settle` IS IDEMPOTENT AND ONLY SHRINKS, and reclaim gives back only
//        what was charged. The address-space page charge is the one that
//        tracks a quantity changing over its holder's life, so it is the one
//        where "the token is unique" does not already imply "the number is
//        right". Idempotence is what makes a split exact: a region carved in
//        two settles against the tree's new span, and settling again must
//        change nothing or a second call on an unchanged map would refund
//        pages the map still holds. Shrink-only is what makes a `munmap`
//        unrefusable against a ceiling it is reducing the use of.
//
//   (L5) A STALE REFUND IS IDENTITY. A refund whose generation does not match
//        the row's touches nothing. This is what makes a leaked charge
//        self-healing rather than a permanent lie, and what lets a charge
//        outlive its process (an in-flight SCM_RIGHTS FileRef, a keepalive pin
//        the NIC has not reclaimed) without corrupting the slot's next
//        occupant.
//
// DELETED as an obligation, deliberately: "refunded exactly once". It assumes
// `Drop` always runs, which is false for a fault frame the unwinder skips and
// false for `mem::forget` — which is *already called four times* inside a
// `#![forbid(unsafe_code)]` crate in this tree (drivers/src/irq.rs:57-58,
// drivers/src/touchpad/mod.rs:283-284). What holds is L3a (at most once) plus
// the runtime audit for the other direction; claiming linearity here would be
// claiming something Rust does not give.
//
// NOT IN MODEL, and audited instead — named here and in verification/STATUS.md:
//
//   * The `fetch_add`/`fetch_sub` memory ordering on each row, and the
//     ordering between a refund and the slot release it may race. Verus has
//     no weak-memory model. Each method body is one atomic-bounded `Step`
//     here; the real `charge_row` is a compare-exchange loop and
//     `release_row` another, so an inductive invariant that survives every
//     `Step` is the sequential skeleton of the concurrency claim, not the
//     whole of it. Covered by KernMiri under both Stacked and Tree Borrows,
//     plus the in-kernel `quotacheck` audit.
//
//   * The `Charge` token's *placement* — that it lives in exactly one field
//     for exactly the object's lifetime. That is a syntactic property of the
//     tree, enforced by `scripts/check_charge_linearity.sh`, not a property
//     of this state machine.
//
// Modelling strategy. Flat scalars rather than a `Map`, in the house style: a
// three-level chain (root, parent, leaf) instantiated concretely, which is
// enough to exhibit every partial-batch shape because the debit walk is
// uniform in depth and bounded by MAX_ACCOUNT_DEPTH.

use vstd::prelude::*;

verus! {

// ===========================================================================
// Abstract ledger state.
//
// Three rows on one chain: leaf -> mid -> root, mirroring a process account,
// its spawner's, and the kernel's. `live_*` is the sum of the amounts of the
// charges outstanding against that row — the quantity L1 says `used` equals.
// ===========================================================================

pub struct Ledger {
    // Row occupancy.
    pub used_leaf: nat,
    pub used_mid: nat,
    pub used_root: nat,

    // The ghost sum of live charges debiting each row. A charge against the
    // leaf debits all three, so `live_mid` includes the leaf's and `live_root`
    // includes both.
    pub live_leaf: nat,
    pub live_mid: nat,
    pub live_root: nat,

    // Ceilings. `NO_LIMIT` is modelled as a very large nat rather than a
    // separate variant; the arena's `u32::MAX` sentinel plays the same role.
    pub limit_leaf: nat,
    pub limit_mid: nat,
    pub limit_root: nat,

    // The generation stamped on the leaf's row, and the generation the
    // outstanding charge was minted against. A refund applies only when they
    // match — the mechanism that makes a stale refund a defined no-op.
    pub gen_row: nat,
    pub gen_charge: nat,

    // Whether the leaf row is bound at all.
    pub leaf_live: bool,

    // The single outstanding charge under test: its amount, and whether it has
    // already been given back. L3a is the claim that `refunded` never goes
    // from true back to a second refund.
    pub charge_amount: nat,
    pub charge_held: bool,
}

// ===========================================================================
// The invariant.
// ===========================================================================

/// (L1) Every row holds exactly the sum of the charges outstanding against it.
///
/// Equality in both directions: `>=` alone would admit a phantom refund
/// (a credit with no charge behind it), which is precisely the failure the
/// linear token is there to make unrepresentable.
pub open spec fn l1_equality(s: Ledger) -> bool {
    &&& s.used_leaf == s.live_leaf
    &&& s.used_mid == s.live_mid
    &&& s.used_root == s.live_root
}

/// The hierarchical shape: an ancestor holds at least what its descendants do,
/// because every debit against a descendant walked through it.
///
/// This is what the in-kernel `quotacheck` audit checks at runtime
/// (`LedgerFault::AncestorUnderCount`), stated here as an invariant the step
/// relation preserves.
pub open spec fn hierarchy_monotone(s: Ledger) -> bool {
    &&& s.live_mid >= s.live_leaf
    &&& s.live_root >= s.live_mid
}

pub open spec fn ledger_inv(s: Ledger) -> bool {
    &&& l1_equality(s)
    &&& hierarchy_monotone(s)
}

/// Initial state: nothing charged, the leaf bound, generations agreeing.
pub open spec fn ledger_init(s: Ledger) -> bool {
    &&& s.used_leaf == 0
    &&& s.used_mid == 0
    &&& s.used_root == 0
    &&& s.live_leaf == 0
    &&& s.live_mid == 0
    &&& s.live_root == 0
    &&& !s.charge_held
    &&& s.charge_amount == 0
}

pub proof fn ledger_init_inv(s: Ledger)
    requires
        ledger_init(s),
    ensures
        ledger_inv(s),
{
}

// ===========================================================================
// Steps.
//
// One variant per atomic-bounded operation, each doc-commented with the real
// call site it mirrors.
// ===========================================================================

pub enum Step {
    /// `try_charge` succeeds at every level: all three rows are debited and a
    /// `Reservation` is handed back.
    /// slopos-ostd/src/process/quota/arena.rs — `try_charge`, the loop's
    /// successful exit.
    TryChargeOk { n: nat },

    /// `try_charge` debits the leaf, then the mid level refuses. The leaf's
    /// debit MUST be given back before the error returns.
    /// slopos-ostd/src/process/quota/arena.rs — `try_charge`'s `unwind` call
    /// on the `charge_row` error arm.
    TryChargeDeniedAtLevel1 { n: nat },

    /// `try_charge` debits the leaf and the mid, then the root refuses. Both
    /// must be unwound. This is the partial batch that makes L4 hard.
    /// slopos-ostd/src/process/quota/arena.rs — same `unwind`, two levels deep.
    TryChargeDeniedAtLevel2 { n: nat },

    /// A refund whose generation matches the row's: every level is credited.
    /// slopos-ostd/src/process/quota/arena.rs — `refund_raw`.
    RefundLive,

    /// A refund whose generation does not match — the row was released and
    /// its slot rebound. A defined no-op.
    /// slopos-ostd/src/process/quota/arena.rs — `refund_raw`'s `row_for`
    /// returning `None` on the generation compare.
    RefundStale,

    /// A child account is bound beneath the leaf.
    /// slopos-ostd/src/process/quota/arena.rs — `account_create`.
    SubAccountCreate,

    /// The leaf's row goes dark; outstanding amounts are already reflected in
    /// its ancestors, so nothing moves.
    /// slopos-ostd/src/process/quota/arena.rs — `account_release`.
    SubAccountDrop,

    /// An operator lowers a ceiling, possibly below what is already held.
    /// slopos-ostd/src/process/quota/arena.rs — `set_limit`.
    LowerLimit { to: nat },

    /// The slot's generation is bumped, invalidating every outstanding
    /// designator for it.
    /// slopos-ostd/src/process/quota/arena.rs — `account_release`'s
    /// generation store, and `account_create`'s on the next occupant.
    SlotRelease,

    /// `Charge::try_extend` with headroom: the token grows in place.
    /// slopos-ostd/src/process/quota/token.rs — `Charge::try_extend`.
    ExtendOk { n: nat },

    /// `try_extend`'s reservation was refused, so the charge is unchanged.
    /// slopos-ostd/src/process/quota/token.rs — the caller's `try_charge`
    /// having returned `Err` before `try_extend` is reached.
    ExtendDenied,

    /// `Charge::shrink`: part of the amount is given back.
    /// slopos-ostd/src/process/quota/token.rs — `Charge::shrink`.
    Shrink { n: nat },

    /// A reclaimer released `n` pages that were charged.
    ///
    /// Modelled as an ordinary refund and not as a special case, which is the
    /// claim being made: reclaim gives pages back through the same token path
    /// as a `munmap`, so it cannot produce a refund the ledger has no charge
    /// behind. slopos-ostd/src/mm/reclaim.rs — `run`.
    Reclaim { n: nat },

    /// `VmaMap::settle`: give back whatever the charge holds above what the
    /// map spans.
    ///
    /// The whole address-space page charge in one step. `want` is the tree's
    /// span after the mutation; the charge falls to meet it and never rises,
    /// which is what makes a `munmap` unrefusable.
    /// mm/src/vma_region.rs — `settle`.
    SettleTo { want: nat },
}

/// Whether a debit of `n` fits under `limit`.
pub open spec fn fits(used: nat, n: nat, limit: nat) -> bool {
    used + n <= limit
}

pub open spec fn step(s: Ledger, t: Step) -> Ledger {
    match t {
        Step::TryChargeOk { n } => {
            // Only modelled as taken when every level has room and no charge
            // is already outstanding (the model tracks one token).
            if !s.charge_held && fits(s.used_leaf, n, s.limit_leaf) && fits(
                s.used_mid,
                n,
                s.limit_mid,
            ) && fits(s.used_root, n, s.limit_root) {
                Ledger {
                    used_leaf: (s.used_leaf + n) as nat,
                    used_mid: (s.used_mid + n) as nat,
                    used_root: (s.used_root + n) as nat,
                    live_leaf: (s.live_leaf + n) as nat,
                    live_mid: (s.live_mid + n) as nat,
                    live_root: (s.live_root + n) as nat,
                    charge_amount: n,
                    charge_held: true,
                    gen_charge: s.gen_row,
                    ..s
                }
            } else {
                s
            }
        },
        // The leaf was debited and then given back: identity. Written as the
        // *composition* rather than as `s` so the proof has to show the
        // round-trip cancels, which is the thing under test.
        Step::TryChargeDeniedAtLevel1 { n } => {
            let debited = Ledger { used_leaf: (s.used_leaf + n) as nat, ..s };
            Ledger { used_leaf: (debited.used_leaf - n) as nat, ..debited }
        },
        // Leaf and mid debited, then both given back.
        Step::TryChargeDeniedAtLevel2 { n } => {
            let debited = Ledger {
                used_leaf: (s.used_leaf + n) as nat,
                used_mid: (s.used_mid + n) as nat,
                ..s
            };
            Ledger {
                used_leaf: (debited.used_leaf - n) as nat,
                used_mid: (debited.used_mid - n) as nat,
                ..debited
            }
        },
        Step::RefundLive => {
            // The generation compare is what makes this the live path.
            if s.charge_held && s.leaf_live && s.gen_charge == s.gen_row {
                Ledger {
                    used_leaf: (s.used_leaf - s.charge_amount) as nat,
                    used_mid: (s.used_mid - s.charge_amount) as nat,
                    used_root: (s.used_root - s.charge_amount) as nat,
                    live_leaf: (s.live_leaf - s.charge_amount) as nat,
                    live_mid: (s.live_mid - s.charge_amount) as nat,
                    live_root: (s.live_root - s.charge_amount) as nat,
                    charge_held: false,
                    ..s
                }
            } else {
                s
            }
        },
        // A stale refund touches nothing at all — not the row it names, and
        // not the principal that holds that slot now.
        Step::RefundStale => s,
        Step::SubAccountCreate => s,
        Step::SubAccountDrop => Ledger { leaf_live: false, ..s },
        Step::LowerLimit { to } => Ledger { limit_leaf: to, ..s },
        Step::SlotRelease => Ledger { gen_row: (s.gen_row + 1) as nat, leaf_live: false, ..s },
        Step::ExtendOk { n } => {
            if s.charge_held && fits(s.used_leaf, n, s.limit_leaf) && fits(
                s.used_mid,
                n,
                s.limit_mid,
            ) && fits(s.used_root, n, s.limit_root) {
                Ledger {
                    used_leaf: (s.used_leaf + n) as nat,
                    used_mid: (s.used_mid + n) as nat,
                    used_root: (s.used_root + n) as nat,
                    live_leaf: (s.live_leaf + n) as nat,
                    live_mid: (s.live_mid + n) as nat,
                    live_root: (s.live_root + n) as nat,
                    charge_amount: (s.charge_amount + n) as nat,
                    ..s
                }
            } else {
                s
            }
        },
        Step::ExtendDenied => s,
        // Reclaim is a refund and nothing more. Bounded by what is held, so
        // it can never manufacture headroom that was not charged.
        Step::Reclaim { n } => {
            if s.charge_held && n <= s.charge_amount && s.leaf_live && s.gen_charge == s.gen_row {
                Ledger {
                    used_leaf: (s.used_leaf - n) as nat,
                    used_mid: (s.used_mid - n) as nat,
                    used_root: (s.used_root - n) as nat,
                    live_leaf: (s.live_leaf - n) as nat,
                    live_mid: (s.live_mid - n) as nat,
                    live_root: (s.live_root - n) as nat,
                    charge_amount: (s.charge_amount - n) as nat,
                    ..s
                }
            } else {
                s
            }
        },
        // Shrink-only: `want` above what is held is a no-op, because growth is
        // always pre-reserved by the caller that wanted it.
        Step::SettleTo { want } => {
            if s.charge_held && want < s.charge_amount && s.leaf_live && s.gen_charge == s.gen_row {
                let give_back = (s.charge_amount - want) as nat;
                Ledger {
                    used_leaf: (s.used_leaf - give_back) as nat,
                    used_mid: (s.used_mid - give_back) as nat,
                    used_root: (s.used_root - give_back) as nat,
                    live_leaf: (s.live_leaf - give_back) as nat,
                    live_mid: (s.live_mid - give_back) as nat,
                    live_root: (s.live_root - give_back) as nat,
                    charge_amount: want,
                    ..s
                }
            } else {
                s
            }
        },
        Step::Shrink { n } => {
            if s.charge_held && n <= s.charge_amount {
                Ledger {
                    used_leaf: (s.used_leaf - n) as nat,
                    used_mid: (s.used_mid - n) as nat,
                    used_root: (s.used_root - n) as nat,
                    live_leaf: (s.live_leaf - n) as nat,
                    live_mid: (s.live_mid - n) as nat,
                    live_root: (s.live_root - n) as nat,
                    charge_amount: (s.charge_amount - n) as nat,
                    ..s
                }
            } else {
                s
            }
        },
    }
}

/// Every step preserves the invariant — so L1 and the hierarchy hold in every
/// reachable state, under every interleaving of the modelled operations.
pub proof fn step_preserves(s: Ledger, t: Step)
    requires
        ledger_inv(s),
        // The row cannot hold less than the single outstanding charge; this is
        // the frame condition the tree gets from `used` being the sum.
        s.charge_held ==> s.charge_amount <= s.used_leaf,
    ensures
        ledger_inv(step(s, t)),
{
}

// ===========================================================================
// (L4) A denied charge is the identity on every row.
//
// The obligation the two-phase Reservation/Charge split exists to deliver, and
// the one upstream hand-writes a cancel loop for — shipping a warning and a
// repair store for when it goes wrong.
// ===========================================================================

/// A refusal at the FIRST level above the leaf leaves every row untouched.
pub proof fn l4_denied_at_level1_is_identity(s: Ledger, n: nat)
    ensures
        step(s, Step::TryChargeDeniedAtLevel1 { n }) == s,
{
}

/// A refusal at the SECOND level — after the leaf AND the mid have both been
/// debited — also leaves every row untouched.
///
/// This is the partial batch: the walk succeeded for k levels and failed at
/// k+1, so the unwind has to give back exactly the levels it took and no
/// others. A cancel loop that unwound one level, or all three, would fail
/// here.
pub proof fn l4_denied_at_level2_is_identity(s: Ledger, n: nat)
    ensures
        step(s, Step::TryChargeDeniedAtLevel2 { n }) == s,
{
}

// ===========================================================================
// (L2) No successful charge leaves a row over its ceiling.
//
// Stated as a step property. The global form is false as soon as LowerLimit
// exists, and pretending otherwise would be claiming a guarantee the ceiling
// does not make.
// ===========================================================================

pub proof fn l2_granted_charge_respects_every_ceiling(s: Ledger, n: nat)
    requires
        // The step was actually taken (the guard held).
        step(s, Step::TryChargeOk { n }) != s,
    ensures
        step(s, Step::TryChargeOk { n }).used_leaf <= s.limit_leaf,
        step(s, Step::TryChargeOk { n }).used_mid <= s.limit_mid,
        step(s, Step::TryChargeOk { n }).used_root <= s.limit_root,
{
}

/// And the honest converse: after `LowerLimit`, a row CAN sit above its
/// ceiling. Stated as a proof rather than left implicit, so nobody later reads
/// L2 as the global claim it deliberately is not.
pub proof fn l2_is_not_global_because_a_limit_can_be_lowered()
    ensures
        exists|s: Ledger, to: nat|
            #![trigger step(s, Step::LowerLimit { to })]
            step(s, Step::LowerLimit { to }).used_leaf > step(
                s,
                Step::LowerLimit { to },
            ).limit_leaf,
{
    let s = Ledger {
        used_leaf: 10,
        used_mid: 10,
        used_root: 10,
        live_leaf: 10,
        live_mid: 10,
        live_root: 10,
        limit_leaf: 100,
        limit_mid: 100,
        limit_root: 100,
        gen_row: 1,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 10,
        charge_held: true,
    };
    let lowered = step(s, Step::LowerLimit { to: 4 });
    assert(lowered.used_leaf == 10);
    assert(lowered.limit_leaf == 4);
    assert(step(s, Step::LowerLimit { to: 4 }).used_leaf > step(
        s,
        Step::LowerLimit { to: 4 },
    ).limit_leaf);
}

// ===========================================================================
// (L5) A stale refund is the identity.
// ===========================================================================

/// A refund arriving after its account's slot was released touches nothing —
/// not the row, and not whichever principal holds that slot now.
///
/// This is what makes a leaked charge self-healing, and what lets a charge
/// outlive its process at all.
pub proof fn l5_stale_refund_is_identity(s: Ledger)
    ensures
        step(s, Step::RefundStale) == s,
{
}

/// The generation compare is load-bearing, not decoration: a `RefundLive` on a
/// state whose generations disagree is also the identity.
pub proof fn l5_generation_mismatch_refunds_nothing(s: Ledger)
    requires
        s.gen_charge != s.gen_row,
    ensures
        step(s, Step::RefundLive) == s,
{
}

// ===========================================================================
// (L3a) No double refund.
// ===========================================================================

/// A second refund of an already-refunded charge does nothing.
///
/// In the tree this is delivered by the type system rather than by a check:
/// `Charge` is not `Clone`, `release` takes `self` by value, and `Drop` runs
/// at most once — so a second refund is not a thing that can be written. The
/// model keeps the flag so the property is stated where the other four are.
pub proof fn l3a_second_refund_is_identity(s: Ledger)
    requires
        !s.charge_held,
    ensures
        step(s, Step::RefundLive) == s,
{
}

/// And a refund followed by a second refund lands in the same state as one.
pub proof fn l3a_refund_is_idempotent(s: Ledger)
    requires
        ledger_inv(s),
        s.charge_held ==> s.charge_amount <= s.used_leaf,
    ensures
        step(step(s, Step::RefundLive), Step::RefundLive) == step(s, Step::RefundLive),
{
}

// ===========================================================================
// Whole-trace induction: the invariant holds in every reachable state.
// ===========================================================================

pub open spec fn ledger_run(s: Ledger, trace: Seq<Step>) -> Ledger
    decreases trace.len(),
{
    if trace.len() == 0 {
        s
    } else {
        step(ledger_run(s, trace.drop_last()), trace.last())
    }
}

/// The frame condition, carried along the trace: a held charge never exceeds
/// the row it is charged against.
pub open spec fn charge_bounded(s: Ledger) -> bool {
    s.charge_held ==> s.charge_amount <= s.used_leaf
}

pub proof fn step_preserves_bounded(s: Ledger, t: Step)
    requires
        ledger_inv(s),
        charge_bounded(s),
    ensures
        charge_bounded(step(s, t)),
{
}

/// (L1) In EVERY reachable state, every row holds exactly the sum of the
/// charges outstanding against it — the equality the runtime `quotacheck`
/// audit re-checks against the real arena.
pub proof fn l1_holds_on_every_trace(s0: Ledger, trace: Seq<Step>)
    requires
        ledger_init(s0),
        charge_bounded(s0),
    ensures
        ledger_inv(ledger_run(s0, trace)),
        charge_bounded(ledger_run(s0, trace)),
    decreases trace.len(),
{
    if trace.len() == 0 {
        ledger_init_inv(s0);
    } else {
        l1_holds_on_every_trace(s0, trace.drop_last());
        step_preserves(ledger_run(s0, trace.drop_last()), trace.last());
        step_preserves_bounded(ledger_run(s0, trace.drop_last()), trace.last());
    }
}

// ===========================================================================
// (L6) `settle` is idempotent, and only ever shrinks.
//
// The property that makes a split exact where FreeBSD's per-object counter
// could not be. A region carved in two settles once against the tree's new
// span; settling again must be the identity, or a second call on an unchanged
// map would refund pages the map still holds. And it must never raise the
// charge, because growth is pre-reserved by the caller and a `munmap` that
// could be refused against a ceiling it is *reducing* the use of would be a
// process unable to give memory back.
// ===========================================================================

/// Settling twice to the same target is settling once.
pub proof fn settle_is_idempotent(s: Ledger, want: nat)
    requires
        ledger_inv(s),
        charge_bounded(s),
    ensures
        step(step(s, Step::SettleTo { want }), Step::SettleTo { want })
            == step(s, Step::SettleTo { want }),
{
}

/// Settling never raises the charge, and never raises a row.
pub proof fn settle_only_shrinks(s: Ledger, want: nat)
    requires
        ledger_inv(s),
        charge_bounded(s),
    ensures
        step(s, Step::SettleTo { want }).charge_amount <= s.charge_amount,
        step(s, Step::SettleTo { want }).used_leaf <= s.used_leaf,
        step(s, Step::SettleTo { want }).used_root <= s.used_root,
{
}

/// Reclaim gives back only what was charged, so it cannot manufacture
/// headroom — the property that makes a reclaimer safe to register.
pub proof fn reclaim_never_exceeds_the_charge(s: Ledger, n: nat)
    requires
        ledger_inv(s),
        charge_bounded(s),
    ensures
        step(s, Step::Reclaim { n }).used_leaf <= s.used_leaf,
        step(s, Step::Reclaim { n }).charge_amount <= s.charge_amount,
{
}

// ===========================================================================
// Broken witnesses.
//
// Each pairs an `exists` — a reachable state the broken variant corrupts —
// with a `forall` showing the real step preserves the property. Without these
// the obligations above could all be vacuous.
// ===========================================================================

/// BROKEN 1: debit `0..k`, then return `Err` without unwinding.
///
/// The hand-written cancel loop upstream ships a warning and a repair store
/// for exactly this. Violates L4: a denied charge is supposed to be the
/// identity, and this leaves the leaf permanently short of headroom it never
/// granted.
pub open spec fn broken_denied_without_unwind(s: Ledger, n: nat) -> Ledger {
    Ledger { used_leaf: (s.used_leaf + n) as nat, ..s }
}

pub proof fn broken_denied_without_unwind_violates_l4()
    ensures
        exists|s: Ledger, n: nat|
            #![trigger broken_denied_without_unwind(s, n)]
            broken_denied_without_unwind(s, n) != s,
        forall|s: Ledger, n: nat| #[trigger]
            step(s, Step::TryChargeDeniedAtLevel1 { n }) == s,
{
    let s = Ledger {
        used_leaf: 0,
        used_mid: 0,
        used_root: 0,
        live_leaf: 0,
        live_mid: 0,
        live_root: 0,
        limit_leaf: 8,
        limit_mid: 8,
        limit_root: 8,
        gen_row: 1,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 0,
        charge_held: false,
    };
    assert(broken_denied_without_unwind(s, 1).used_leaf == 1);
    assert(broken_denied_without_unwind(s, 1) != s);
    assert(exists|s: Ledger, n: nat|
        #![trigger broken_denied_without_unwind(s, n)]
        broken_denied_without_unwind(s, n) != s);
}

/// BROKEN 2: a refund that skips the generation compare.
///
/// Violates L1 and L5 together: it credits whichever principal holds the slot
/// now, so that row's `used` drops below the sum of the charges actually
/// outstanding against it — a phantom refund, which is exactly what an
/// inequality-shaped L1 would fail to catch.
pub open spec fn broken_refund_ignores_generation(s: Ledger) -> Ledger {
    Ledger {
        used_leaf: (s.used_leaf - s.charge_amount) as nat,
        used_mid: (s.used_mid - s.charge_amount) as nat,
        used_root: (s.used_root - s.charge_amount) as nat,
        charge_held: false,
        ..s
    }
}

pub proof fn broken_refund_ignoring_generation_violates_l1()
    ensures
        exists|s: Ledger|
            #![trigger broken_refund_ignores_generation(s)]
            ledger_inv(s) && !ledger_inv(broken_refund_ignores_generation(s)),
        // The real step is the identity on exactly those states.
        forall|s: Ledger| s.gen_charge != s.gen_row ==> #[trigger]
            step(s, Step::RefundLive) == s,
{
    // The slot was released and rebound: the row's generation has moved on,
    // and its 5 units belong to the NEW occupant. A stale charge of 5 refunds
    // against it anyway.
    let s = Ledger {
        used_leaf: 5,
        used_mid: 5,
        used_root: 5,
        live_leaf: 5,
        live_mid: 5,
        live_root: 5,
        limit_leaf: 100,
        limit_mid: 100,
        limit_root: 100,
        gen_row: 2,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 5,
        charge_held: true,
    };
    assert(ledger_inv(s));
    let corrupt = broken_refund_ignores_generation(s);
    assert(corrupt.used_leaf == 0);
    assert(corrupt.live_leaf == 5);
    assert(!l1_equality(corrupt));
    assert(!ledger_inv(corrupt));
    assert(exists|s: Ledger|
        #![trigger broken_refund_ignores_generation(s)]
        ledger_inv(s) && !ledger_inv(broken_refund_ignores_generation(s)));
}

/// BROKEN 3: a `Clone`able charge.
///
/// Two tokens naming one debit; each refunds, so the row is credited twice for
/// one charge. Violates L1 in the under-count direction — the failure Windows
/// needed a dedicated bug check for and XNU a panic-on-negative flag compiled
/// out of shipping kernels. Here `Charge` is simply not `Clone`, and
/// `#[derive(Charged)]` refuses to expand alongside `Clone` or `Copy`.
pub open spec fn broken_double_refund(s: Ledger) -> Ledger {
    Ledger {
        used_leaf: (s.used_leaf - s.charge_amount - s.charge_amount) as nat,
        used_mid: (s.used_mid - s.charge_amount - s.charge_amount) as nat,
        used_root: (s.used_root - s.charge_amount - s.charge_amount) as nat,
        live_leaf: (s.live_leaf - s.charge_amount) as nat,
        live_mid: (s.live_mid - s.charge_amount) as nat,
        live_root: (s.live_root - s.charge_amount) as nat,
        charge_held: false,
        ..s
    }
}

pub proof fn broken_cloneable_charge_violates_l1()
    ensures
        exists|s: Ledger|
            #![trigger broken_double_refund(s)]
            ledger_inv(s) && !ledger_inv(broken_double_refund(s)),
{
    // Two charges of 3 outstanding, one of which is a clone of the other: the
    // row holds 6 and the live sum is 6, but both tokens refund 3.
    let s = Ledger {
        used_leaf: 6,
        used_mid: 6,
        used_root: 6,
        live_leaf: 6,
        live_mid: 6,
        live_root: 6,
        limit_leaf: 100,
        limit_mid: 100,
        limit_root: 100,
        gen_row: 1,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 3,
        charge_held: true,
    };
    assert(ledger_inv(s));
    let corrupt = broken_double_refund(s);
    assert(corrupt.used_leaf == 0);
    assert(corrupt.live_leaf == 3);
    assert(!l1_equality(corrupt));
    assert(exists|s: Ledger|
        #![trigger broken_double_refund(s)]
        ledger_inv(s) && !ledger_inv(broken_double_refund(s)));
}

/// BROKEN 4: a check-then-charge split.
///
/// `has_headroom(kind) -> bool` followed by an unconditional debit. Between
/// the two, another CPU takes the last unit; the debit then lands over the
/// ceiling. This is why no headroom predicate is exposed anywhere and the
/// `Reservation` is the only observation of headroom — the same split that
/// caused a multi-commit fix series upstream, including an off-by-one that
/// permitted one task past the limit.
pub open spec fn broken_check_then_charge(s: Ledger, n: nat, stale_used: nat) -> Ledger {
    // The check used `stale_used`; the debit applies to the current value.
    if fits(stale_used, n, s.limit_leaf) {
        Ledger { used_leaf: (s.used_leaf + n) as nat, ..s }
    } else {
        s
    }
}

pub proof fn broken_check_then_charge_violates_l2()
    ensures
        exists|s: Ledger, n: nat, stale: nat|
            #![trigger broken_check_then_charge(s, n, stale)]
            broken_check_then_charge(s, n, stale).used_leaf > s.limit_leaf,
{
    // The check saw 0 used against a ceiling of 4 and passed. By the time the
    // debit landed, a peer had taken all 4.
    let s = Ledger {
        used_leaf: 4,
        used_mid: 4,
        used_root: 4,
        live_leaf: 4,
        live_mid: 4,
        live_root: 4,
        limit_leaf: 4,
        limit_mid: 100,
        limit_root: 100,
        gen_row: 1,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 4,
        charge_held: true,
    };
    let corrupt = broken_check_then_charge(s, 1, 0);
    assert(corrupt.used_leaf == 5);
    assert(corrupt.used_leaf > s.limit_leaf);
    assert(exists|s: Ledger, n: nat, stale: nat|
        #![trigger broken_check_then_charge(s, n, stale)]
        broken_check_then_charge(s, n, stale).used_leaf > s.limit_leaf);
}

/// BROKEN 5: hierarchical debit combined with committed child limits.
///
/// If a parent's row were credited with each child's *ceiling* at creation
/// time — the admission-control shape — rather than with what the children
/// actually hold, the parent would double-count: once for the reservation and
/// again for each real charge walking up. Violates L1.
///
/// SlopOS deliberately over-commits instead: a parent may hand out child
/// ceilings summing past its own, and the bound comes from the debit reaching
/// every ancestor. That is KeyKOS's sub-bank rule, and it is why a ceiling can
/// be unreachable without that being a bug.
pub open spec fn broken_commit_child_limit(s: Ledger, child_limit: nat) -> Ledger {
    Ledger { used_mid: (s.used_mid + child_limit) as nat, ..s }
}

pub proof fn broken_committed_child_limit_violates_l1()
    ensures
        exists|s: Ledger, child_limit: nat|
            #![trigger broken_commit_child_limit(s, child_limit)]
            ledger_inv(s) && !ledger_inv(broken_commit_child_limit(s, child_limit)),
{
    let s = Ledger {
        used_leaf: 0,
        used_mid: 0,
        used_root: 0,
        live_leaf: 0,
        live_mid: 0,
        live_root: 0,
        limit_leaf: 8,
        limit_mid: 64,
        limit_root: 256,
        gen_row: 1,
        gen_charge: 1,
        leaf_live: true,
        charge_amount: 0,
        charge_held: false,
    };
    assert(ledger_inv(s));
    let corrupt = broken_commit_child_limit(s, 8);
    assert(corrupt.used_mid == 8);
    assert(corrupt.live_mid == 0);
    assert(!l1_equality(corrupt));
    assert(exists|s: Ledger, child_limit: nat|
        #![trigger broken_commit_child_limit(s, child_limit)]
        ledger_inv(s) && !ledger_inv(broken_commit_child_limit(s, child_limit)));
}

} // verus!

fn main() {}
