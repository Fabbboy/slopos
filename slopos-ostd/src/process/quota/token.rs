//! The linear tokens: [`Reservation`] and [`Charge`].
//!
//! [`try_charge`](super::try_charge) debits the leaf account and every
//! ancestor, so a refusal can land part-way up; the partial debit is handed to
//! a `Reservation` whose `Drop` unwinds it, making rollback the same code as an
//! ordinary refund. Committing one yields a [`Charge`], which lives in exactly
//! one place for exactly the lifetime of the thing it accounts for — never
//! `Option<Charge<_>>`, and no accessor hands one back by value.
//! `scripts/check_charge_linearity.sh` is what keeps that invariant true.

use core::marker::PhantomData;
use core::sync::atomic::Ordering;

use slopos_abi::quota::ResourceKind;

use super::arena::refund_raw;
use super::axis::Refundable;
use crate::process::AccountId;

/// Headroom taken, not yet resident in an object. Dropping it refunds: a
/// reservation that goes out of scope uncommitted is a refusal somewhere up
/// the call chain.
#[must_use = "a dropped Reservation refunds; commit it into the object it accounts for"]
pub struct Reservation<A: Refundable> {
    account: AccountId,
    amount: u32,
    _axis: PhantomData<A>,
}

impl<A: Refundable> Reservation<A> {
    /// [`try_charge`](super::try_charge) is the only minter, which is what
    /// makes holding one proof that the debit happened.
    #[inline]
    pub(super) fn new(account: AccountId, amount: u32) -> Self {
        Self {
            account,
            amount,
            _axis: PhantomData,
        }
    }

    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    #[inline]
    pub fn amount(&self) -> u32 {
        self.amount
    }

    /// Consume without refunding — the one way a reservation's `Drop` is
    /// bypassed, so [`Charge::commit`] can carry the debit forward.
    #[inline]
    fn into_parts(self) -> (AccountId, u32) {
        let parts = (self.account, self.amount);
        core::mem::forget(self);
        parts
    }

    /// Hand this reservation's debit to a [`ChargeSlot`] that has already
    /// recorded it.
    #[inline]
    fn into_slot(self) {
        let _ = self.into_parts();
    }
}

impl<A: Refundable> Drop for Reservation<A> {
    #[inline]
    fn drop(&mut self) {
        refund_raw(self.account, A::KIND, self.amount);
    }
}

impl<A: Refundable> core::fmt::Debug for Reservation<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Reservation")
            .field("axis", &A::NAME)
            .field("account", &self.account)
            .field("amount", &self.amount)
            .finish()
    }
}

/// A resident charge: headroom held for as long as the object holding this
/// token lives.
///
/// Mintable only by consuming a [`Reservation`], and the fields are private,
/// so there is no route to one that skips a completed debit.
///
/// There is deliberately no `&mut Charge` anywhere: an in-place mutator would
/// let a caller adjust an amount without the compiler noticing the old token
/// was never accounted for.
#[must_use = "a Charge must be stored in the object it accounts for, or the charge is lost"]
pub struct Charge<A: Refundable> {
    account: AccountId,
    amount: u32,
    _axis: PhantomData<A>,
}

impl<A: Refundable> Charge<A> {
    /// The only minter.
    #[inline]
    pub fn commit(reservation: Reservation<A>) -> Self {
        let (account, amount) = reservation.into_parts();
        Self {
            account,
            amount,
            _axis: PhantomData,
        }
    }

    /// Grow this charge by a further reservation on the same account, so one
    /// token names the whole amount rather than accumulating per growth.
    ///
    /// A reservation against a *different* account is refunded and the charge
    /// left unchanged: refusing rather than asserting keeps charge migration
    /// unrepresentable in a release build too.
    #[inline]
    pub fn try_extend(self, reservation: Reservation<A>) -> Self {
        if reservation.account() != self.account {
            return self;
        }
        // Saturating would be a phantom debit: the row is already charged for
        // `amount`, so a token capping below `held + amount` refunds less than
        // was taken. Refuse instead and let the reservation's `Drop` return it.
        let Some(total) = self.amount.checked_add(reservation.amount()) else {
            return self;
        };
        let _ = reservation.into_parts();
        let (account, _) = self.into_parts();
        Self {
            account,
            amount: total,
            _axis: PhantomData,
        }
    }

    /// Give back `n` units, keeping the rest.
    ///
    /// Saturating rather than wrapping: a wrapped token would claim `u32::MAX`
    /// and its eventual refund would empty the account's row.
    #[inline]
    pub fn shrink(self, n: u32) -> Self {
        let (account, held) = self.into_parts();
        let give_back = n.min(held);
        refund_raw(account, A::KIND, give_back);
        Self {
            account,
            amount: held - give_back,
            _axis: PhantomData,
        }
    }

    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The amount held, carried rather than recomputed at the refund site,
    /// where a recomputed size would let the ledger and reality diverge.
    #[inline]
    pub fn amount(&self) -> u32 {
        self.amount
    }

    #[inline]
    pub fn kind(&self) -> ResourceKind {
        A::KIND
    }

    #[inline]
    fn into_parts(self) -> (AccountId, u32) {
        let parts = (self.account, self.amount);
        core::mem::forget(self);
        parts
    }

    /// Hand this charge's obligation to a [`ChargeSlot`] that has already
    /// recorded it. Consuming without refunding is correct only there: the
    /// slot has become the charge's single home, so a refund here would
    /// double-count against a debit that is still outstanding.
    #[inline]
    fn into_slot(self) {
        let _ = self.into_parts();
    }

    /// Give the whole charge back explicitly, for a kind whose refund point is
    /// not its holder's `Drop` — see [`ChargeSlot`].
    #[inline]
    pub fn release(self) {
        let (account, amount) = self.into_parts();
        refund_raw(account, A::KIND, amount);
    }
}

impl<A: Refundable> Drop for Charge<A> {
    /// Atomics on `.bss` and nothing else — no lock, no allocation, no counted
    /// reference — so this is legal from a hard IRQ, from under a cli-spinlock,
    /// from the IRQ-off switch tail, and from a dying task's own unwind.
    #[inline]
    fn drop(&mut self) {
        refund_raw(self.account, A::KIND, self.amount);
    }
}

impl<A: Refundable> core::fmt::Debug for Charge<A> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Charge")
            .field("axis", &A::NAME)
            .field("account", &self.account)
            .field("amount", &self.amount)
            .finish()
    }
}

// Several objects holding a token are counted against the 2 KiB stack-frame gate.
const _: () = assert!(core::mem::size_of::<Charge<slopos_abi::quota::FdSlot>>() <= 16);
const _: () = assert!(core::mem::size_of::<Reservation<slopos_abi::quota::FdSlot>>() <= 16);

/// A `.bss` home for one charge, claimed and released atomically.
///
/// For a resource whose refund point is **not** its holder's `Drop`: a task's
/// destruction is deferred to the graveyard, so a `Charge` field would keep
/// every exited thread charged until the drain. The charge lives here instead
/// and is released at the exit latch.
///
/// [`put`](Self::put) refunds a displaced occupant and [`take`](Self::take) is
/// a move, so exactly one caller can win the release. Every operation is a
/// compare-exchange and a store — no lock, no allocation, no counted reference
/// — so it is legal from the IRQ-off exit path.
pub struct ChargeSlot<A: Refundable> {
    /// Zero means empty, which no live account id can be.
    account: core::sync::atomic::AtomicU64,
    amount: core::sync::atomic::AtomicU32,
    _axis: PhantomData<A>,
}

impl<A: Refundable> ChargeSlot<A> {
    pub const fn empty() -> Self {
        Self {
            account: core::sync::atomic::AtomicU64::new(0),
            amount: core::sync::atomic::AtomicU32::new(0),
            _axis: PhantomData,
        }
    }

    /// Store `reservation` as this slot's charge, refunding any occupant.
    pub fn put(&self, reservation: Reservation<A>) {
        let charge = Charge::commit(reservation);
        let (account, amount) = (charge.account(), charge.amount());
        self.take();
        self.amount.store(amount, Ordering::Release);
        self.account.store(account.raw(), Ordering::Release);
        charge.into_slot();
    }

    /// Take the charge out and refund it. Idempotent.
    pub fn take(&self) {
        let raw = self.account.swap(0, Ordering::AcqRel);
        if raw == 0 {
            return;
        }
        let amount = self.amount.swap(0, Ordering::AcqRel);
        refund_raw(AccountId::from_raw(raw), A::KIND, amount);
    }

    pub fn is_occupied(&self) -> bool {
        self.account.load(Ordering::Acquire) != 0
    }

    pub fn amount(&self) -> u32 {
        if self.account.load(Ordering::Acquire) == 0 {
            return 0;
        }
        self.amount.load(Ordering::Acquire)
    }

    /// The account being charged, or [`AccountId::NONE`] for an empty slot.
    pub fn account(&self) -> AccountId {
        AccountId::from_raw(self.account.load(Ordering::Acquire))
    }

    /// Add `reservation`'s debit to what this slot already holds. A
    /// reservation against a *different* account is refunded and the slot left
    /// alone; see [`Charge::try_extend`].
    pub fn grow(&self, reservation: Reservation<A>) {
        if reservation.amount() == 0 {
            return;
        }
        let raw = self.account.load(Ordering::Acquire);
        if raw == 0 {
            self.put(reservation);
            return;
        }
        if AccountId::from_raw(raw) != reservation.account() {
            return;
        }
        let mut held = self.amount.load(Ordering::Relaxed);
        loop {
            let Some(total) = held.checked_add(reservation.amount()) else {
                return;
            };
            match self.amount.compare_exchange_weak(
                held,
                total,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => break,
                Err(observed) => held = observed,
            }
        }
        reservation.into_slot();
    }

    /// Give back `n` units, keeping the rest. Clamped at what is held.
    pub fn shrink(&self, n: u32) {
        if n == 0 {
            return;
        }
        let raw = self.account.load(Ordering::Acquire);
        if raw == 0 {
            return;
        }
        let mut held = self.amount.load(Ordering::Relaxed);
        loop {
            let give_back = n.min(held);
            if give_back == 0 {
                return;
            }
            match self.amount.compare_exchange_weak(
                held,
                held - give_back,
                Ordering::AcqRel,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    refund_raw(AccountId::from_raw(raw), A::KIND, give_back);
                    return;
                }
                Err(observed) => held = observed,
            }
        }
    }
}

impl<A: Refundable> Drop for ChargeSlot<A> {
    /// Backstop: a slot whose owner was dropped without reaching its release
    /// point still refunds here rather than leaking.
    #[inline]
    fn drop(&mut self) {
        self.take();
    }
}
