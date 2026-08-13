//! The linear tokens: [`Reservation`] and [`Charge`].
//!
//! # Why two types and not one
//!
//! [`try_charge`](super::try_charge) debits the leaf account and every
//! ancestor. A refusal can therefore happen part-way up, with levels `0..k`
//! already debited, and those debits have to come back. Rather than a
//! hand-written cancel loop at the failure site — which is the shape upstream
//! ships a warning and a repair store for — the partial debit is handed to a
//! `Reservation` whose `Drop` unwinds it. The rollback is then the same code
//! as an ordinary refund, and there is no path on which it can be forgotten.
//!
//! [`Charge`] is what a successful reservation becomes when it moves into the
//! object it accounts for. The split is what makes "the counter was
//! incremented before the object existed" a fact about the type rather than a
//! rule about review order: the object's constructor cannot run without
//! consuming a token that only a completed debit produces.
//!
//! # The invariant, stated narrowly
//!
//! Rust is affine, not linear, so "a missing refund is unrepresentable" would
//! be false — `mem::forget`, `ManuallyDrop` and `KBox::leak` are all safe, and
//! `mem::forget` is already called inside a `#![forbid(unsafe_code)]` crate in
//! this tree. What holds instead:
//!
//! > A `Charge` lives in exactly one place for exactly the lifetime of the
//! > thing it accounts for.
//!
//! Never `Option<Charge<_>>` — `Option::take` is a safe separation. No
//! accessor hands one back by value. Under that invariant every safe way to
//! lose the token also leaks the object, and a charge on a leaked object is
//! *correct*: a leaked memfd really does still hold its registry row.
//! `scripts/check_charge_linearity.sh` is what keeps the invariant true.

use core::marker::PhantomData;
use core::sync::atomic::Ordering;

use slopos_abi::quota::ResourceKind;

use super::arena::refund_raw;
use super::axis::Refundable;
use crate::process::AccountId;

/// Headroom taken, not yet resident in an object.
///
/// Stack-only by convention and linear by construction: no `Clone`, no `Copy`,
/// no `Default`, private fields, and a `Drop` that gives the headroom back. A
/// reservation that goes out of scope without being committed is a refusal
/// somewhere up the call chain, and refunding is exactly the right thing to do
/// with it.
#[must_use = "a dropped Reservation refunds; commit it into the object it accounts for"]
pub struct Reservation<A: Refundable> {
    account: AccountId,
    amount: u32,
    _axis: PhantomData<A>,
}

impl<A: Refundable> Reservation<A> {
    /// Private: [`try_charge`](super::try_charge) is the only minter, which is
    /// what makes holding one proof that the debit happened.
    #[inline]
    pub(super) fn new(account: AccountId, amount: u32) -> Self {
        Self {
            account,
            amount,
            _axis: PhantomData,
        }
    }

    /// The account this reservation was taken against.
    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// How much was taken.
    #[inline]
    pub fn amount(&self) -> u32 {
        self.amount
    }

    /// Consume without refunding, yielding the parts. The one way a
    /// reservation's `Drop` is bypassed, and it exists so [`Charge::commit`]
    /// can carry the debit forward rather than release and re-take it.
    #[inline]
    fn into_parts(self) -> (AccountId, u32) {
        let parts = (self.account, self.amount);
        core::mem::forget(self);
        parts
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
/// Mintable only by consuming a [`Reservation`], so a service crate cannot
/// fabricate one — `slopos-ostd` is the only crate permitted `unsafe` and the
/// fields are private, so there is no safe or unsafe route to a `Charge` that
/// does not pass through a completed debit.
///
/// Every mutator takes `self` by value and hands a new token back. There is
/// deliberately no `&mut Charge` anywhere: an in-place mutator would let a
/// caller adjust an amount without the compiler noticing the old token was
/// never accounted for.
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

    /// Grow this charge by a further reservation on the same account.
    ///
    /// The growth path for a tier-2 charge — a region that gains pages keeps
    /// one token naming the whole amount rather than accumulating a token per
    /// growth. A reservation against a *different* account is refunded and the
    /// charge left unchanged, because a charge that moved accounts would be
    /// charge migration, which this design makes unrepresentable everywhere
    /// else. Refusing rather than asserting keeps that true in a release
    /// build, where an assertion would be compiled out and the charge would
    /// silently move.
    #[inline]
    pub fn try_extend(self, reservation: Reservation<A>) -> Self {
        if reservation.account() != self.account {
            return self;
        }
        // Saturating here would be a phantom debit: the row has already been
        // charged for `amount`, so a token that caps below `held + amount`
        // refunds less than was taken and the difference is never given back.
        // Refuse the extension instead — the reservation's `Drop` gives its
        // debit straight back, which is the identity the whole design turns
        // on. Reaching this needs a single object holding 4 billion units,
        // which no ceiling permits; it is handled rather than relied upon.
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
    /// Saturating rather than wrapping: shrinking past zero is a bookkeeping
    /// bug, and the failure it must not produce is a token claiming `u32::MAX`
    /// whose eventual refund empties the account's row.
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

    /// The account being charged.
    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The amount held. Carried rather than recomputed at the refund site:
    /// recomputing a size at refund time is how a ledger and reality diverge.
    #[inline]
    pub fn amount(&self) -> u32 {
        self.amount
    }

    /// The kind this charge debits. For the audit, which walks charges of
    /// erased type.
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
    /// recorded it.
    ///
    /// Private to this module and used only by [`ChargeSlot::put`], which
    /// stores the account and amount before calling it. Consuming the token
    /// without refunding is correct exactly there and nowhere else: the slot
    /// has become the charge's single home, so refunding here would
    /// double-count against a debit that is still outstanding.
    #[inline]
    fn into_slot(self) {
        let _ = self.into_parts();
    }

    /// Give the whole charge back explicitly, for a kind whose refund point is
    /// not its holder's `Drop`.
    ///
    /// [`ResourceKind::Task`](slopos_abi::quota::ResourceKind::Task) is the
    /// case that needs this: a task's destruction is deferred to the
    /// graveyard, so the refund happens at the exit latch instead. Consumes
    /// the token, so the later `Drop` cannot double-refund — there is no later
    /// `Drop`.
    #[inline]
    pub fn release(self) {
        let (account, amount) = self.into_parts();
        refund_raw(account, A::KIND, amount);
    }
}

impl<A: Refundable> Drop for Charge<A> {
    /// Atomics on `.bss` and nothing else — no lock, no allocation, no counted
    /// reference. That is what makes this legal from a hard IRQ, from under a
    /// cli-spinlock, from the IRQ-off switch tail, and from a dying task's own
    /// unwind, which is the whole reason the account is a generation-stamped
    /// row rather than a `KArc`.
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

// The token replaces nothing larger than a `u64` in the objects it lands in,
// and several of those objects are counted against a 2 KiB stack-frame gate.
const _: () = assert!(core::mem::size_of::<Charge<slopos_abi::quota::FdSlot>>() <= 16);
const _: () = assert!(core::mem::size_of::<Reservation<slopos_abi::quota::FdSlot>>() <= 16);

/// A `.bss` home for one charge, claimed and released atomically.
///
/// For a resource whose refund point is **not** its holder's `Drop`. The
/// motivating case is a task: its destruction is deferred to the graveyard, so
/// a `Charge` field would keep a thousand exited threads charged until the
/// drain — spurious refusals under exactly the load the quota bounds. The
/// charge lives here instead and is released at the exit latch.
///
/// The slot owns a real token rather than a decomposed account-and-amount
/// pair. A charge stored as plain data is a charge nothing refunds if the row
/// is overwritten, which is the failure the linear token exists to prevent;
/// [`put`](Self::put) refunds a displaced occupant rather than dropping it on
/// the floor, and [`take`](Self::take) is a move, so exactly one caller can
/// win the release.
///
/// Every operation is a compare-exchange and a store: no lock, no allocation,
/// no counted reference, so it is legal from the IRQ-off exit path.
pub struct ChargeSlot<A: Refundable> {
    /// The charged account, packed. Zero means empty, which no live account
    /// id can be.
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
        // The displaced occupant is refunded through the same path a take
        // would use, so an overwrite cannot leak.
        self.take();
        self.amount.store(amount, Ordering::Release);
        self.account.store(account.raw(), Ordering::Release);
        // The slot is now the charge's single home. Handing the token's
        // amount to the slot and then letting the token refund would
        // double-count, so the token is consumed without refunding here — the
        // one sanctioned place that happens, and the reason `ChargeSlot` lives
        // beside the token rather than in a service crate.
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

    /// Whether this slot currently holds a charge.
    pub fn is_occupied(&self) -> bool {
        self.account.load(Ordering::Acquire) != 0
    }
}

impl<A: Refundable> Drop for ChargeSlot<A> {
    /// The backstop. A slot released explicitly — at an exit latch, at a reap
    /// — is already empty by the time this runs, so the common path is one
    /// relaxed load. A slot whose owner was dropped without reaching its
    /// release point still refunds here rather than leaking.
    #[inline]
    fn drop(&mut self) {
        self.take();
    }
}
