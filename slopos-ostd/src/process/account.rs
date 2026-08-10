//! The accounting identity a [`Process`](super::Process) is minted with.
//!
//! An `AccountId` names a row in the resource-accounting arena. This module
//! places the id and its generation stamp; the arena's semantics — the debit
//! walk, the linear `Charge` token, the per-kind ceilings — belong to
//! `plans/resource-accounting.md` and land on top of this.
//!
//! It is a generation-stamped slot index rather than a counted reference for
//! the reason the whole plan turns on: a refund has to be legal from a hard
//! IRQ, from under a cli-spinlock and from a dying task's own unwind, and a
//! counted reference makes the last release a heap free. A `.bss` row named by
//! a stamped id has no release point at all, so the context question does not
//! arise, and a refund against a released row is a defined no-op rather than a
//! write into a stranger's numbers.

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::task::MAX_PROCESSES;

/// Slot width of the packed account id.
///
/// Sized from [`MAX_ACCOUNTS`], not chosen: 16 bits covers the arena with
/// room to grow it by two orders of magnitude before the constant has to move,
/// and leaves 48 bits of generation — the same split
/// [`PROCESS_VM_SLOT_BITS`](crate::handle::PROCESS_VM_SLOT_BITS) uses, for the
/// same reason.
pub const ACCOUNT_SLOT_BITS: u32 = 16;

/// Rows in the account arena: one per process, plus the kernel's root.
pub const MAX_ACCOUNTS: usize = MAX_PROCESSES + 1;

const _: () = assert!(MAX_ACCOUNTS <= (1usize << ACCOUNT_SLOT_BITS));

/// Highest generation before the counter wraps back to 1.
///
/// Never 0: a packed id of 0 is [`AccountId::NONE`], so no live id may
/// collide with it.
const GENERATION_MAX: u64 = (1u64 << (64 - ACCOUNT_SLOT_BITS)) - 1;

/// The root account's slot. Fixed rather than allocated so the kernel's own
/// charges have a payer before the arena's allocator has run.
pub const ROOT_ACCOUNT_SLOT: u32 = 0;

/// A resource account, named by slot and generation.
///
/// `Copy` and one word wide, so it fits in a `Charge` without making the token
/// bigger than the thing it accounts for.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AccountId(u64);

impl AccountId {
    /// The absent account. Zero, so a zeroed struct field reads as "no
    /// account" rather than as the root's.
    pub const NONE: Self = Self(0);

    /// Reconstruct from a slot and a generation. Forging one is harmless:
    /// every arena operation compares the generation against the row's before
    /// touching it, so a bogus id is a no-op rather than a stray write.
    #[inline]
    pub const fn from_parts(slot: u32, generation: u64) -> Self {
        let slot_mask = (1u64 << ACCOUNT_SLOT_BITS) - 1;
        Self(
            ((generation & (u64::MAX >> ACCOUNT_SLOT_BITS)) << ACCOUNT_SLOT_BITS)
                | (slot as u64 & slot_mask),
        )
    }

    #[inline]
    pub const fn slot(self) -> u32 {
        (self.0 & ((1u64 << ACCOUNT_SLOT_BITS) - 1)) as u32
    }

    #[inline]
    pub const fn generation(self) -> u64 {
        self.0 >> ACCOUNT_SLOT_BITS
    }

    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// The raw packed word, for storage in a width-limited field.
    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    /// Inverse of [`raw`](Self::raw).
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

impl core::fmt::Debug for AccountId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        if self.is_none() {
            return f.write_str("AccountId(none)");
        }
        f.debug_struct("AccountId")
            .field("slot", &self.slot())
            .field("generation", &self.generation())
            .finish()
    }
}

/// Monotonic source of account generations.
///
/// Global rather than per-row so an id minted before a test-scope reset can
/// never match the slot's next occupant — the property
/// [`VmSlotAlloc::reset`](../../../mm/src/process_vm.rs) preserves for the
/// same reason. Starts at 1 because 0 is [`AccountId::NONE`].
static NEXT_GENERATION: AtomicU64 = AtomicU64::new(1);

/// Draw a fresh, never-reused generation.
///
/// Wraps to 1 rather than 0 at [`GENERATION_MAX`]. A wrap needs 2^48
/// bindings, which at one process per microsecond is nine years of
/// uninterrupted spawning; it is handled rather than relied upon.
pub(super) fn alloc_generation() -> u64 {
    let mut current = NEXT_GENERATION.load(Ordering::Relaxed);
    loop {
        let next = if current >= GENERATION_MAX {
            1
        } else {
            current + 1
        };
        match NEXT_GENERATION.compare_exchange_weak(
            current,
            next,
            Ordering::Relaxed,
            Ordering::Relaxed,
        ) {
            Ok(_) => return current,
            Err(observed) => current = observed,
        }
    }
}

/// The root account: the kernel's own payer, and the ancestor every process
/// account debits through.
///
/// Its slot is fixed and its generation is drawn once at first call, so a
/// caller that names the root before boot has finished still names the same
/// row every other caller does.
pub fn root_account() -> AccountId {
    static ROOT: AtomicU64 = AtomicU64::new(0);
    let existing = ROOT.load(Ordering::Acquire);
    if existing != 0 {
        return AccountId::from_raw(existing);
    }
    let minted = AccountId::from_parts(ROOT_ACCOUNT_SLOT, alloc_generation());
    match ROOT.compare_exchange(0, minted.raw(), Ordering::AcqRel, Ordering::Acquire) {
        Ok(_) => minted,
        // Lost the race; the winner's id is the one everyone uses.
        Err(winner) => AccountId::from_raw(winner),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parts_round_trip() {
        for &(slot, generation) in &[(0u32, 1u64), (7, 5), (255, 1 << 40)] {
            let id = AccountId::from_parts(slot, generation);
            assert_eq!(id.slot(), slot);
            assert_eq!(id.generation(), generation);
            assert_eq!(AccountId::from_raw(id.raw()), id);
        }
    }

    #[test]
    fn none_is_zero_and_distinguishable() {
        assert!(AccountId::NONE.is_none());
        assert_eq!(AccountId::NONE.raw(), 0);
        // Generations start at 1, so no minted id collides with NONE.
        assert!(!AccountId::from_parts(0, 1).is_none());
    }

    #[test]
    fn generations_are_unique() {
        let a = alloc_generation();
        let b = alloc_generation();
        assert_ne!(a, b);
        assert_ne!(a, 0);
        assert_ne!(b, 0);
    }

    #[test]
    fn root_is_stable() {
        assert_eq!(root_account(), root_account());
        assert_eq!(root_account().slot(), ROOT_ACCOUNT_SLOT);
    }
}
