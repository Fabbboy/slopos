//! The `Process` object: the identity a task's address space, descriptor
//! table and resource account all hang off.
//!
//! This crate sits below `mm` and `fs`, so the object carries identity only
//! and those subsystems keep their own storage, re-keyed from a recycled `u32`
//! onto [`Handle<Process>`]: a recycled id designates whichever process holds
//! it *now*, while a generation-checked handle fails closed with
//! [`HandleError`].
//!
//! The parent edge is a packed handle, not a `KArc` — an owning edge would
//! unwind a deep ancestor chain on one stack against a 2 KiB frame budget, and
//! a reaped parent resolving to [`HandleError::Stale`] is exactly what
//! "orphaned" means. Address-space and descriptor-table teardown stay behind
//! the exit latch rather than in [`Process::drop`]: CR3 must move off an
//! address space before it is destroyed, and the destroy path holds a cli-lock
//! across a cross-CPU shootdown wait, neither of which a `Drop` from an
//! arbitrary last release can refuse.

pub mod account;
pub mod quota;
mod registry;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::handle::{Handle, HandleError};

pub use account::{ACCOUNT_SLOT_BITS, AccountId, MAX_ACCOUNTS, root_account};
pub use quota::{Charge, Reservation, TryChargeError, try_charge};
pub use registry::{
    MAX_PROCESS_ID, PROCESS_SLOT_BITS, ProcessAllocError, ProcessTable, process_count,
    process_find_by_id, process_for_handle, process_registry_reset, process_resolve,
    process_retire, process_spawn, process_spawn_root, with_process_registry,
};

/// Packed [`Handle<Process>`] meaning "no process". Zero, so a zeroed `Task`
/// field reads as "no process" rather than as slot 0's.
pub const PROCESS_HANDLE_NONE: u64 = 0;

/// Pack a process handle into the single word a task carries.
///
/// The slot is stored **biased by one**: unbiased, slot 0 at generation 0 —
/// exactly what the first `bind` into a fresh table produces — packs to zero
/// and is indistinguishable from [`PROCESS_HANDLE_NONE`].
#[inline]
pub fn pack_process_handle(handle: Handle<Process>) -> u64 {
    Handle::<Process>::from_parts(handle.slot() + 1, handle.generation()).pack(PROCESS_SLOT_BITS)
        as u64
}

/// Inverse of [`pack_process_handle`]. `None` for [`PROCESS_HANDLE_NONE`].
#[inline]
pub fn unpack_process_handle(packed: u64) -> Option<Handle<Process>> {
    if packed == PROCESS_HANDLE_NONE {
        return None;
    }
    let biased = Handle::<Process>::unpack(packed as usize, PROCESS_SLOT_BITS);
    // Slot field 0 did not come from the packer; refuse it rather than
    // unbiasing to `u32::MAX`.
    let slot = biased.slot().checked_sub(1)?;
    Some(Handle::from_parts(slot, biased.generation()))
}

/// A process: one address space, one descriptor table, one resource account,
/// and the tasks that share them.
///
/// Held by `KArc`; the registry owns one reference for as long as the process
/// is reachable by handle or by id.
pub struct Process {
    /// Display and ABI only, never a lookup key: nothing resolves an object
    /// through it without a generation check.
    id: u32,

    /// Self-handle: registry slot plus the generation stamped at bind. Written
    /// once at registration, before the object is published.
    handle: AtomicU64,

    /// Parent, as a generation-checked designator.
    ///
    /// Atomic because the writer is not the owner: a dying parent re-homes
    /// children from a CPU that is not theirs. Acquire/Release, so a reader
    /// that sees the new parent also sees what the reparent published before
    /// it.
    parent: AtomicU64,

    /// This process's resource account row, minted with it.
    account: AccountId,

    /// The accounting tree's upward edge: the spawner's account, set once and
    /// never re-homed.
    ///
    /// Deliberately a different field from [`parent`](Self::parent):
    /// reparent-to-init must move the *wait* edge and must not move a budget,
    /// and an immutable accounting edge makes charge migration
    /// unrepresentable rather than merely discouraged.
    account_parent: AccountId,

    /// Live tasks sharing this process.
    task_count: AtomicU32,

    /// Set once the last task has exited. The id stays allocated and the
    /// registry entry stays resolvable until the process is reaped, so a
    /// `waitpid` that arrives after the exit still finds something to answer
    /// with — and so the id cannot be handed to a stranger in that window.
    exited: AtomicBool,

    /// This process's own existence, charged to its **spawner**.
    ///
    /// Released at the reap rather than at the final `Drop`, because the
    /// object outlives the resource: a `Process` stays referenced after
    /// `process_retire`, so a `Drop`-refund would keep the spawner charged for
    /// children that are already gone.
    proc_charge: quota::ChargeSlot<slopos_abi::quota::ProcCount>,
}

impl Process {
    /// Build an unregistered process. [`process_spawn`] is the only way to
    /// obtain one: it allocates the id, binds the slot and stamps the
    /// self-handle as one step.
    fn new(
        id: u32,
        account: AccountId,
        account_parent: AccountId,
        parent: u64,
        proc_charge: quota::Reservation<slopos_abi::quota::ProcCount>,
    ) -> Self {
        let slot = quota::ChargeSlot::empty();
        slot.put(proc_charge);
        Self {
            id,
            handle: AtomicU64::new(PROCESS_HANDLE_NONE),
            parent: AtomicU64::new(parent),
            account,
            account_parent,
            task_count: AtomicU32::new(0),
            exited: AtomicBool::new(false),
            proc_charge: slot,
        }
    }

    /// Give the spawner back the charge for this process.
    ///
    /// Idempotent, and called from the reap; the slot's own `Drop` is the
    /// backstop for a process that is dropped without ever being retired.
    #[inline]
    pub(super) fn release_proc_charge(&self) {
        self.proc_charge.take();
    }

    /// The numeric process id. Display and ABI only.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// This process's own handle. `None` only inside the registry's own bind
    /// step — never on a process any caller can reach.
    #[inline]
    pub fn handle(&self) -> Option<Handle<Process>> {
        unpack_process_handle(self.handle.load(Ordering::Acquire))
    }

    /// The packed form, for storage in a task's `process_handle` field.
    #[inline]
    pub fn handle_raw(&self) -> u64 {
        self.handle.load(Ordering::Acquire)
    }

    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The account this one debits through. Immutable by construction.
    #[inline]
    pub fn account_parent(&self) -> AccountId {
        self.account_parent
    }

    /// The parent process, if it is still resolvable. `None` covers both
    /// "never had one" and "reaped": to every caller, an orphan.
    #[inline]
    pub fn parent(&self) -> Option<Handle<Process>> {
        unpack_process_handle(self.parent.load(Ordering::Acquire))
    }

    /// Re-home the wait edge. The accounting edge is untouched, by design.
    #[inline]
    pub fn set_parent(&self, parent: Option<Handle<Process>>) {
        let packed = parent.map_or(PROCESS_HANDLE_NONE, pack_process_handle);
        self.parent.store(packed, Ordering::Release);
    }

    #[inline]
    pub fn task_count(&self) -> u32 {
        self.task_count.load(Ordering::Acquire)
    }

    /// Join this process. Returns the count after the join.
    ///
    /// One atomic, so it is legal from the fork path under a preempt guard.
    #[inline]
    pub fn task_join(&self) -> u32 {
        self.task_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Leave this process. Returns `true` if this was the last task.
    ///
    /// Saturating rather than wrapping: a double-leave must not produce a
    /// count that wraps to `u32::MAX` and pins the address space forever.
    #[inline]
    pub fn task_leave(&self) -> bool {
        let mut current = self.task_count.load(Ordering::Acquire);
        loop {
            if current == 0 {
                debug_assert!(false, "process task count underflow");
                return false;
            }
            match self.task_count.compare_exchange_weak(
                current,
                current - 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return current == 1,
                Err(observed) => current = observed,
            }
        }
    }

    /// Whether the last task has left.
    #[inline]
    pub fn has_exited(&self) -> bool {
        self.exited.load(Ordering::Acquire)
    }

    /// Mark the process exited. Returns `false` if it already was, so the
    /// caller can keep an exit-once action exactly once.
    #[inline]
    pub fn mark_exited(&self) -> bool {
        !self.exited.swap(true, Ordering::AcqRel)
    }
}

impl core::fmt::Debug for Process {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Process")
            .field("id", &self.id)
            .field("handle", &self.handle())
            .field("tasks", &self.task_count())
            .field("exited", &self.has_exited())
            .finish()
    }
}

impl Drop for Process {
    /// Return the id to the allocator. Nothing else.
    ///
    /// Panic-free and lock-free by construction, which is what makes the
    /// destructor legal wherever a last release can happen — under the
    /// registry lock, on the exit path with interrupts off, or on a dying
    /// task's own unwind. The registry slot is *not* released here: releasing
    /// it is what caused this drop.
    ///
    /// The account row goes dark here rather than at the reap, so a charge
    /// that outlives its process refunds against a released row and is a
    /// defined no-op instead of a debit against whoever holds the slot next.
    fn drop(&mut self) {
        // Releases *this* process's row; `proc_charge` bills the spawner's and
        // is refunded by its own field destructor after this body returns.
        quota::account_release(self.account);
        registry::release_process_id(self.id);
    }
}

/// Resolve a handle to the process it names. A rebound slot answers
/// [`HandleError::Stale`] rather than whichever process occupies it now.
#[inline]
pub fn process_lookup(handle: Handle<Process>) -> Result<crate::KArc<Process>, HandleError> {
    process_resolve(handle)
}

/// A process named by both halves of its identity: the generation-checked
/// handle that every table keys on, and the numeric id the ABI shows userland.
///
/// What kernel entry points take instead of a bare `u32`: ids recycle, so a
/// stale number silently designates whichever process holds it now.
///
/// The two fields cannot disagree, because the only way to build one outside
/// this module is [`resolve`](Self::resolve), which reads both from the same
/// live process under one lookup.
///
/// The handle is stored packed rather than as a [`Handle`] struct — the
/// 24-byte layout put three frames over the 2 KiB stack gate, since this type
/// replaces a `u32` across most of the syscall surface.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    /// The handle in its [`pack_process_handle`] encoding. Never zero: a
    /// `ProcessId` names a real process, and the "none" case is `Option`.
    packed: u64,
    id: u32,
}

impl ProcessId {
    /// The identity of a live process. `None` for one that was never
    /// registered.
    #[inline]
    pub fn of(process: &Process) -> Option<Self> {
        let packed = process.handle_raw();
        if packed == PROCESS_HANDLE_NONE {
            return None;
        }
        Some(Self {
            packed,
            id: process.id(),
        })
    }

    /// Resolve a numeric id to a live process identity.
    ///
    /// The bridge at the ABI boundary: an id naming no live process fails
    /// here, once, instead of being carried inward as a `u32` that later
    /// resolves to a stranger.
    #[inline]
    pub fn resolve(id: u32) -> Option<Self> {
        let process = process_find_by_id(id)?;
        Self::of(process.as_ref())
    }

    /// The generation-checked handle. What every table lookup keys on.
    ///
    /// `packed` is non-zero by construction and only [`pack_process_handle`]
    /// ever wrote it, so the `unwrap_or` is unreachable — and slot `u32::MAX`
    /// is outside every table, so it resolves to nothing if it is ever hit.
    #[inline]
    pub fn handle(self) -> Handle<Process> {
        unpack_process_handle(self.packed).unwrap_or(Handle::from_parts(u32::MAX, u64::MAX))
    }

    /// The numeric id. Display and ABI only — never a lookup key.
    #[inline]
    pub const fn id(self) -> u32 {
        self.id
    }

    /// The process itself, if it is still live. `None` once it has been
    /// reaped: a `ProcessId` is a designator, not a reference, so it does not
    /// keep the process alive.
    #[inline]
    pub fn get(self) -> Option<crate::KArc<Process>> {
        process_for_handle(self.handle())
    }

    #[inline]
    pub fn is_live(self) -> bool {
        self.get().is_some()
    }

    /// The account this process's resources are charged to.
    ///
    /// [`AccountId::NONE`] once the process has been reaped — every arena
    /// operation treats that as a vacuous success — never the root's, which
    /// would bill the kernel for a departed user process's resources.
    #[inline]
    pub fn account(self) -> AccountId {
        self.get()
            .map_or(AccountId::NONE, |process| process.account())
    }
}

impl core::fmt::Debug for ProcessId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "ProcessId({}, slot {}, gen {})",
            self.id,
            self.handle().slot(),
            self.handle().generation()
        )
    }
}

impl core::fmt::Display for ProcessId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_charge() -> quota::Reservation<slopos_abi::quota::ProcCount> {
        quota::try_charge::<slopos_abi::quota::ProcCount>(AccountId::NONE, 1)
            .expect("a charge against no account is vacuous")
    }

    fn scratch(id: u32) -> Process {
        Process::new(
            id,
            AccountId::NONE,
            AccountId::NONE,
            PROCESS_HANDLE_NONE,
            no_charge(),
        )
    }

    #[test]
    fn task_count_join_and_leave() {
        let p = scratch(1);
        assert_eq!(p.task_count(), 0);
        assert_eq!(p.task_join(), 1);
        assert_eq!(p.task_join(), 2);
        assert!(!p.task_leave());
        assert!(p.task_leave(), "the second leave is the last task");
        assert_eq!(p.task_count(), 0);
        // Never registered, so skip the id-release path.
        core::mem::forget(p);
    }

    #[test]
    fn exit_marks_once() {
        let p = scratch(2);
        assert!(!p.has_exited());
        assert!(p.mark_exited());
        assert!(!p.mark_exited(), "a second mark reports it was already set");
        assert!(p.has_exited());
        core::mem::forget(p);
    }

    #[test]
    fn parent_edge_is_rehomeable_and_account_edge_is_not() {
        let p = Process::new(
            3,
            AccountId::from_parts(4, 9),
            AccountId::from_parts(1, 2),
            PROCESS_HANDLE_NONE,
            no_charge(),
        );
        assert!(p.parent().is_none());
        let new_parent = Handle::<Process>::from_parts(7, 11);
        p.set_parent(Some(new_parent));
        assert_eq!(p.parent(), Some(new_parent));
        assert_eq!(p.account_parent(), AccountId::from_parts(1, 2));
        assert_eq!(p.account(), AccountId::from_parts(4, 9));
        core::mem::forget(p);
    }

    #[test]
    fn handle_packing_round_trips_and_zero_is_none() {
        assert!(unpack_process_handle(PROCESS_HANDLE_NONE).is_none());
        for &(slot, generation) in &[(0u32, 0u64), (0, 1), (255, 3), (17, 1 << 30)] {
            let h = Handle::<Process>::from_parts(slot, generation);
            assert_eq!(unpack_process_handle(pack_process_handle(h)), Some(h));
        }
    }

    /// The first process a fresh table binds lands on slot 0 generation 0.
    #[test]
    fn the_first_slot_at_generation_zero_is_not_none() {
        let first = Handle::<Process>::from_parts(0, 0);
        assert_ne!(pack_process_handle(first), PROCESS_HANDLE_NONE);
        assert_eq!(
            unpack_process_handle(pack_process_handle(first)),
            Some(first)
        );
    }

    #[test]
    fn an_unbiased_word_is_refused_rather_than_wrapping() {
        // Slot field 0, generation 5: what packing without the bias produces.
        let forged = (5u64) << PROCESS_SLOT_BITS;
        assert!(unpack_process_handle(forged).is_none());
    }
}
