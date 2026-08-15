//! Resource accounting: every kernel allocation is charged to an account, and
//! the charge is a linear token that lives inside the thing it accounts for.
//!
//! One node type. An account is a row in a fixed `.bss` arena, named by a
//! generation-stamped [`AccountId`](crate::process::AccountId), one per
//! [`Process`](crate::process::Process) plus one root owned by the kernel,
//! whose parent edge is set once at creation and never re-homed — so the
//! accounting tree *is* the spawn tree and charge migration is
//! unrepresentable.
//!
//! [`try_charge`] debits the leaf and every ancestor and hands back a linear
//! [`Reservation`]; the charged object's constructor consumes that reservation
//! and stores a [`Charge`]; the same `Drop` that releases the object refunds.
//! Because `slopos-ostd` is the only crate permitted `unsafe` and the token's
//! fields are private, no service crate can forge, clone or bypass a `Charge`.
//!
//! See `plans/resource-accounting.md` for the design argument, and the module
//! docs of [`arena`] for why the arena takes no locks and has no release
//! point.

mod arena;
mod axis;
mod charged;
mod token;

pub use arena::{
    AccountCreateError, KindStats, LedgerFault, MAX_ACCOUNT_DEPTH, NO_LIMIT, PagesReconciler,
    TryChargeError, account_count, account_create, account_release, account_release_by_slot,
    for_each_account, ledger_audit, quota_mode, register_pages_reconciler, reset_for_test, root,
    set_limit, set_quota_mode, stats, try_charge,
};
pub use axis::{Refundable, ResourceAxis};
pub use charged::{
    AliasOf, ChargeAuditEntry, Charged, FileBacking, SharedCharge, charge_audit_entries,
    sealed as charged_sealed,
};
pub use token::{Charge, ChargeSlot, Reservation};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::AccountId;
    use crate::process::account::alloc_generation_for_test;
    use crate::test_support::global_lock::{GlobalTestStateGuard, lock_global_test_state};
    use slopos_abi::quota::{FdSlot, ObjectRow, QuotaMode, ResourceKind};

    /// A fresh account on `slot`, debiting through `parent`.
    ///
    /// Slots are picked by the caller so a test can build an exact shape; the
    /// generation comes from the global counter, so no two accounts in a run
    /// share one even on the same slot.
    fn account(slot: u32, parent: AccountId) -> AccountId {
        let id = AccountId::from_parts(slot, alloc_generation_for_test());
        account_create(id, parent).expect("create");
        id
    }

    /// Serialised against every other global-state test in this binary, and
    /// self-cleaning: the arena is process-global, so a test that left rows
    /// behind would be a sibling's phantom ancestor.
    fn fixture() -> impl Drop {
        struct Guard(GlobalTestStateGuard);
        impl Drop for Guard {
            fn drop(&mut self) {
                reset_for_test();
            }
        }
        let guard = lock_global_test_state();
        reset_for_test();
        set_quota_mode(QuotaMode::Enforce);
        Guard(guard)
    }

    fn used(id: AccountId, kind: ResourceKind) -> u32 {
        stats(id, kind).map_or(0, |s| s.used)
    }

    #[test]
    fn a_charge_debits_the_leaf_and_every_ancestor() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);

        let charge = Charge::commit(try_charge::<FdSlot>(child, 3).expect("charge"));
        assert_eq!(used(child, ResourceKind::FdSlot), 3);
        assert_eq!(
            used(parent, ResourceKind::FdSlot),
            3,
            "the ancestor pays too"
        );
        assert_eq!(
            used(root(), ResourceKind::FdSlot),
            3,
            "and so does the root"
        );

        drop(charge);
        assert_eq!(used(child, ResourceKind::FdSlot), 0);
        assert_eq!(used(parent, ResourceKind::FdSlot), 0);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// L4, the hard half: a batch that succeeds at level *k* and fails at
    /// *k+1* must leave every row exactly as it found it.
    #[test]
    fn a_refusal_partway_up_is_the_identity_on_every_row() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        set_limit(parent, ResourceKind::FdSlot, 4);

        let held = Charge::commit(try_charge::<FdSlot>(child, 4).expect("fill the parent"));
        assert_eq!(used(child, ResourceKind::FdSlot), 4);

        let refused = try_charge::<FdSlot>(child, 1).expect_err("the parent is full");
        assert_eq!(refused.refused_by, parent, "the refusing level is named");
        assert_eq!(refused.errno, slopos_abi::Errno::EMFILE);
        assert_eq!(
            used(child, ResourceKind::FdSlot),
            4,
            "the leaf debit must be unwound, not left behind"
        );
        assert_eq!(used(parent, ResourceKind::FdSlot), 4);

        drop(held);
        assert_eq!(used(child, ResourceKind::FdSlot), 0);
        assert_eq!(used(parent, ResourceKind::FdSlot), 0);
    }

    /// L2 as a step property: no successful charge leaves `used > limit`.
    #[test]
    fn no_granted_charge_exceeds_the_ceiling() {
        let _f = fixture();
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 3);

        let mut held = crate::KVec::new();
        while let Ok(r) = try_charge::<FdSlot>(a, 1) {
            held.push(Charge::commit(r)).expect("hold");
            assert!(
                used(a, ResourceKind::FdSlot) <= 3,
                "used passed the limit on a granted charge"
            );
        }
        assert_eq!(held.len(), 3);
        assert_eq!(stats(a, ResourceKind::FdSlot).expect("row").denials, 1);
    }

    /// Releasing an account gives its ancestors back whatever it still holds.
    ///
    /// The charges outstanding against a released row are exactly the ones
    /// whose refunds are about to become generation-mismatch no-ops, so the
    /// ancestors would otherwise keep those debits with nothing left to
    /// retire them. That leak is unbounded across a boot — measured at 574
    /// tasks held by the root while every process row read zero.
    #[test]
    fn releasing_an_account_hands_its_outstanding_amount_back_up() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);

        // A charge that outlives its process: an in-flight SCM_RIGHTS
        // reference, a keepalive pin, a task in the graveyard.
        let outlives = Charge::commit(try_charge::<FdSlot>(child, 4).expect("charge"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 4);

        account_release(child);
        assert_eq!(
            used(parent, ResourceKind::FdSlot),
            0,
            "the parent must not keep a debit nothing can retire"
        );
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);

        // And the token's own refund, now stale, changes nothing further.
        drop(outlives);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// L5: a refund against a released row is a defined no-op, not a write
    /// into whichever principal holds that slot now.
    #[test]
    fn a_stale_refund_does_not_touch_the_slots_new_occupant() {
        let _f = fixture();
        let first = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(first, 5).expect("charge"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 5);

        // The process exits and its row is released while a charge is still
        // outstanding — an in-flight SCM_RIGHTS FileRef, say.
        account_release(first);
        let second = account(1, root());
        assert_ne!(first, second, "same slot, different generation");
        let fresh = Charge::commit(try_charge::<FdSlot>(second, 2).expect("charge"));
        assert_eq!(used(second, ResourceKind::FdSlot), 2);

        drop(charge);
        assert_eq!(
            used(second, ResourceKind::FdSlot),
            2,
            "the stale refund must not credit the new occupant"
        );
        drop(fresh);
    }

    #[test]
    fn warn_grants_over_limit_charges_and_counts_them() {
        let _f = fixture();
        set_quota_mode(QuotaMode::Warn);
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 1);

        let first = Charge::commit(try_charge::<FdSlot>(a, 1).expect("under the limit"));
        let second = Charge::commit(try_charge::<FdSlot>(a, 1).expect("warn grants"));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 2, "warn grants, so the peak is measurable");
        assert_eq!(s.peak, 2);
        assert_eq!(s.denials, 1, "and records what enforce would have refused");
        drop((first, second));
    }

    #[test]
    fn off_consults_no_ceiling_but_still_counts() {
        let _f = fixture();
        set_quota_mode(QuotaMode::Off);
        let a = account(1, root());
        set_limit(a, ResourceKind::FdSlot, 1);
        let held = Charge::commit(try_charge::<FdSlot>(a, 9).expect("off never refuses"));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 9);
        assert_eq!(s.denials, 0);
        drop(held);
    }

    #[test]
    fn peak_survives_the_charge_being_refunded() {
        let _f = fixture();
        let a = account(1, root());
        drop(Charge::commit(try_charge::<FdSlot>(a, 7).expect("charge")));
        let s = stats(a, ResourceKind::FdSlot).expect("row");
        assert_eq!(s.used, 0, "the charge is gone");
        assert_eq!(s.peak, 7, "but the high-water mark is what a ceiling needs");
    }

    #[test]
    fn kinds_do_not_bleed_into_one_another() {
        let _f = fixture();
        let a = account(1, root());
        let fd = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        assert_eq!(used(a, ResourceKind::FdSlot), 2);
        assert_eq!(used(a, ResourceKind::ObjectRow), 0);
        let row = Charge::commit(try_charge::<ObjectRow>(a, 5).expect("charge"));
        assert_eq!(used(a, ResourceKind::FdSlot), 2);
        assert_eq!(used(a, ResourceKind::ObjectRow), 5);
        drop((fd, row));
    }

    #[test]
    fn extend_grows_one_token_rather_than_accumulating_them() {
        let _f = fixture();
        let a = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        let charge = charge.try_extend(try_charge::<FdSlot>(a, 3).expect("charge"));
        assert_eq!(charge.amount(), 5);
        assert_eq!(used(a, ResourceKind::FdSlot), 5);

        let charge = charge.shrink(4);
        assert_eq!(charge.amount(), 1);
        assert_eq!(used(a, ResourceKind::FdSlot), 1);
        drop(charge);
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// The narrow exception the design allows: an extend against a different
    /// account would be charge migration, so it is refunded and refused.
    #[test]
    fn extend_across_accounts_does_not_migrate_the_charge() {
        let _f = fixture();
        let a = account(1, root());
        let b = account(2, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 2).expect("charge"));
        let foreign = try_charge::<FdSlot>(b, 3).expect("charge");
        let charge = charge.try_extend(foreign);
        assert_eq!(charge.amount(), 2, "the charge did not move");
        assert_eq!(used(b, ResourceKind::FdSlot), 0, "and b was refunded");
        drop(charge);
    }

    /// `release` is the exit-latch path: it consumes the token, so the refund
    /// happens once and there is no later `Drop` to double it.
    #[test]
    fn release_refunds_exactly_once() {
        let _f = fixture();
        let a = account(1, root());
        let charge = Charge::commit(try_charge::<FdSlot>(a, 4).expect("charge"));
        charge.release();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
        assert_eq!(used(root(), ResourceKind::FdSlot), 0);
    }

    /// A reservation that is never committed refunds itself. This is the
    /// hierarchical rollback and the "caller changed its mind" path, and they
    /// are deliberately the same code.
    #[test]
    fn an_uncommitted_reservation_refunds_on_drop() {
        let _f = fixture();
        let a = account(1, root());
        {
            let _r = try_charge::<FdSlot>(a, 6).expect("charge");
            assert_eq!(used(a, ResourceKind::FdSlot), 6);
        }
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    #[test]
    fn the_tree_is_bounded_and_creation_at_the_bound_is_refused() {
        let _f = fixture();
        let mut parent = root();
        // The root occupies one level, so MAX_ACCOUNT_DEPTH - 1 more fit.
        for slot in 1..MAX_ACCOUNT_DEPTH as u32 {
            parent = account(slot, parent);
        }
        let id = AccountId::from_parts(MAX_ACCOUNT_DEPTH as u32, alloc_generation_for_test());
        assert_eq!(
            account_create(id, parent).err(),
            Some(AccountCreateError::TooDeep),
            "an unbounded walk must be refused at creation, not discovered at charge time"
        );
    }

    /// The audit is silent on a ledger that is behaving.
    #[test]
    fn the_audit_accepts_a_consistent_ledger() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        let held = Charge::commit(try_charge::<FdSlot>(child, 3).expect("charge"));
        let other = Charge::commit(try_charge::<ObjectRow>(parent, 2).expect("charge"));

        let mut faults = KVecFaults::default();
        assert_eq!(ledger_audit(|f| faults.push(f)), 0, "{:?}", faults.first);
        drop((held, other));
        assert_eq!(ledger_audit(|f| faults.push(f)), 0);
    }

    /// And it is not vacuous: a row edited behind the token's back is caught.
    ///
    /// The failure being modelled is the one the whole design exists to
    /// eliminate — a refund that reached a descendant but not its ancestor,
    /// which leaves the ancestor holding less than the sum of the rows
    /// debiting through it. An audit that could not see this would be a number
    /// with no reader.
    #[test]
    fn the_audit_catches_an_ancestor_that_lost_a_refund() {
        let _f = fixture();
        let parent = account(1, root());
        let child = account(2, parent);
        let held = Charge::commit(try_charge::<FdSlot>(child, 4).expect("charge"));

        let mut faults = KVecFaults::default();
        assert_eq!(ledger_audit(|f| faults.push(f)), 0, "consistent to start");

        // Refund the parent alone, as a hand-written cancel loop that missed a
        // level would: the child still holds 4, the parent now holds 0.
        arena::refund_raw_one_level_for_test(parent, ResourceKind::FdSlot, 4);

        let mut faults = KVecFaults::default();
        let found = ledger_audit(|f| faults.push(f));
        assert!(found > 0, "the audit must see an under-counted ancestor");
        assert!(
            matches!(
                faults.first,
                Some(LedgerFault::AncestorUnderCount {
                    kind: ResourceKind::FdSlot,
                    ancestor_used: 0,
                    children_used: 4,
                    ..
                })
            ),
            "{:?}",
            faults.first
        );

        // Undo the planted corruption before the real token drops: its refund
        // walks the same chain and would underflow the row this test emptied.
        arena::charge_raw_one_level_for_test(parent, ResourceKind::FdSlot, 4);
        drop(held);
    }

    /// Collects audit findings without allocating: the audit runs in contexts
    /// where a `KVec` push would be the wrong thing to do.
    #[derive(Default)]
    struct KVecFaults {
        first: Option<LedgerFault>,
        count: usize,
    }

    impl KVecFaults {
        fn push(&mut self, fault: LedgerFault) {
            if self.first.is_none() {
                self.first = Some(fault);
            }
            self.count += 1;
        }
    }

    /// A `ChargeSlot` refunds exactly once however the release is reached.
    ///
    /// The shape every kind whose refund point is not its holder's `Drop`
    /// relies on — `Task` at the exit latch, `Process` at the reap. Both
    /// latches can fire twice (from `task_terminate` and from the owning CPU's
    /// post-switch path; from a double reap), so "exactly once" has to be a
    /// property of the slot rather than of the caller.
    #[test]
    fn a_charge_slot_releases_exactly_once() {
        let _f = fixture();
        let a = account(1, root());
        let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();

        slot.put(try_charge::<FdSlot>(a, 3).expect("charge"));
        assert!(slot.is_occupied());
        assert_eq!(used(a, ResourceKind::FdSlot), 3);

        slot.take();
        assert!(!slot.is_occupied());
        assert_eq!(used(a, ResourceKind::FdSlot), 0);

        // The second latch finds an empty slot.
        slot.take();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// Overwriting an occupied slot refunds the displaced charge rather than
    /// dropping it on the floor — so a recycled id whose predecessor never
    /// reached its latch costs nothing rather than one leaked charge.
    #[test]
    fn a_charge_slot_refunds_what_it_displaces() {
        let _f = fixture();
        let a = account(1, root());
        let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();

        slot.put(try_charge::<FdSlot>(a, 3).expect("charge"));
        slot.put(try_charge::<FdSlot>(a, 5).expect("charge"));
        assert_eq!(
            used(a, ResourceKind::FdSlot),
            5,
            "the displaced charge must be given back, not leaked"
        );
        slot.take();
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A slot dropped without an explicit release still refunds.
    #[test]
    fn a_dropped_charge_slot_refunds() {
        let _f = fixture();
        let a = account(1, root());
        {
            let slot: ChargeSlot<FdSlot> = ChargeSlot::empty();
            slot.put(try_charge::<FdSlot>(a, 6).expect("charge"));
            assert_eq!(used(a, ResourceKind::FdSlot), 6);
        }
        assert_eq!(used(a, ResourceKind::FdSlot), 0);
    }

    /// A charge against an account whose row has gone succeeds vacuously.
    /// Refusing would leave a process that had already been released unable to
    /// close its own descriptors.
    #[test]
    fn charging_a_released_account_is_vacuous_rather_than_refused() {
        let _f = fixture();
        let a = account(1, root());
        account_release(a);
        let charge = Charge::commit(try_charge::<FdSlot>(a, 3).expect("vacuous success"));
        assert_eq!(used(root(), ResourceKind::FdSlot), 0, "nothing was debited");
        drop(charge);
    }
}
