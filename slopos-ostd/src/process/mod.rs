//! The `Process` object: the identity a task's address space, descriptor
//! table and resource account all hang off.
//!
//! # Why the object cannot contain what it names
//!
//! `KArc` and `Task` are this crate's, so `Process` must be this crate's too —
//! and this crate sits below `mm` and `fs`, so it cannot name `ProcessVm` or
//! `FdEntry`. The object therefore supplies *identity* and the subsystems keep
//! their storage: `mm` keeps its address-space table, `fs` keeps its
//! descriptor tables, and both re-key from a recycled `u32` onto
//! [`Handle<Process>`].
//!
//! That is not a workaround. A recycled id is the confused-deputy designator:
//! a task holding only an id can be handed the address space of whichever
//! process holds that id *now*, which on a page fault means servicing the
//! fault in a stranger's page tables. A generation-checked handle fails closed
//! instead, and the failure is a typed [`HandleError`] rather than a silent
//! aliasing read.
//!
//! # What is deliberately absent
//!
//! **A children list.** The wait/orphan tree is task-keyed
//! (`Task::children` + `parent_task_id`) and works; a second, unread parent
//! edge made of intrusive links would need its own role marker, its own
//! placement primitives and its own drop-ordering argument, for no reader.
//! [`Process::parent`] is a packed handle instead — see below.
//!
//! **An owning parent reference.** A `KArc<Process>` parent edge would make
//! the child's release a potential release of the parent, so a 256-deep
//! ancestor chain would unwind on one stack against a 2 KiB frame budget, and
//! the cross-CPU store would need an RCU slot to keep a destructor off the
//! writer's stack. A packed [`Handle<Process>`] has neither problem: the store
//! is one atomic word, there is no displaced reference to release, and a
//! parent that has been reaped resolves to [`HandleError::Stale`] — which is
//! exactly what "orphaned" means. This is the same reasoning the tree already
//! wrote down for `Task::process_vm_handle`, applied one level up.
//!
//! **Teardown.** [`Process::drop`] returns the id to the allocator and
//! nothing else. Address-space and descriptor-table teardown stay behind the
//! exit latch and the `on_cpu` bail, because that ordering is load-bearing in
//! two ways a refcount cannot express: CR3 must move off an address space
//! before it is destroyed, and the destroy path holds a cli-lock across a
//! cross-CPU shootdown wait. A `Drop` invoked from an arbitrary last-reference
//! release has no way to refuse that context. The refcount replaces the
//! *decision* — "is this the last task of this process" — not the teardown.

pub mod account;
mod registry;

use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::handle::{Handle, HandleError};

pub use account::{ACCOUNT_SLOT_BITS, AccountId, MAX_ACCOUNTS, root_account};
pub use registry::{
    MAX_PROCESS_ID, PROCESS_SLOT_BITS, ProcessAllocError, ProcessTable, process_count,
    process_find_by_id, process_for_handle, process_registry_reset, process_resolve,
    process_retire, process_spawn, process_spawn_root, with_process_registry,
};

/// Packed [`Handle<Process>`] meaning "no process".
///
/// Zero, so a zeroed `Task` field reads as "no process" rather than as slot 0's
/// — the same convention `Task::process_vm_handle` carries.
pub const PROCESS_HANDLE_NONE: u64 = 0;

/// Pack a process handle into the single word a task carries.
///
/// The slot is stored **biased by one**. Without the bias, slot 0 at
/// generation 0 packs to zero and is indistinguishable from
/// [`PROCESS_HANDLE_NONE`] — and slot 0 generation 0 is exactly what the first
/// `bind` into a fresh table produces, so the collision is on the very first
/// process rather than in some corner. `mm`'s handle avoids this by starting
/// its generations at 1, but that makes the encoding depend on an allocator
/// policy two crates away; biasing the slot makes it a property of the packing
/// itself, which is where a total encoding belongs.
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
    // A packed word whose slot field is 0 was not produced by
    // `pack_process_handle` — every real slot is stored biased. Refuse it
    // rather than unbiasing to `u32::MAX`.
    let slot = biased.slot().checked_sub(1)?;
    Some(Handle::from_parts(slot, biased.generation()))
}

/// A process: one address space, one descriptor table, one resource account,
/// and the tasks that share them.
///
/// Held by `KArc`. The registry owns one reference for as long as the process
/// is reachable by handle or by id; every other holder is a borrow or a clone
/// of that. See the module docs for what this object deliberately does not
/// own.
pub struct Process {
    /// Display and ABI only. Never an authority key, never a lookup key for
    /// anything that matters. `getpid` returns it; nothing resolves an object
    /// through it without a generation check.
    id: u32,

    /// This process's own identity: the registry slot plus the generation
    /// stamped when that slot was bound. Self-referential — written once at
    /// registration, before the object is published, and never again.
    handle: AtomicU64,

    /// Parent, as a generation-checked designator.
    ///
    /// Mutable: reparent-to-init is required by the wait protocol. Atomic
    /// because the writer is not the owner — a dying parent re-homes children
    /// it does not own, from a CPU that is not theirs. Relaxed is not enough
    /// and SeqCst is not needed: an Acquire load pairs with the Release store
    /// so a reader that sees the new parent also sees everything the reparent
    /// published before it.
    parent: AtomicU64,

    /// This process's resource account row, minted with it.
    account: AccountId,

    /// The accounting tree's upward edge: the spawner's account, set once and
    /// never re-homed.
    ///
    /// Deliberately a different field from [`parent`](Self::parent). Reparent-
    /// to-init must move the *wait* edge and must not move a budget; an
    /// immutable accounting edge is what makes charge migration
    /// unrepresentable rather than merely discouraged. Zircon reached the same
    /// conclusion by making the upward edge `const` at all three levels of its
    /// hierarchy.
    account_parent: AccountId,

    /// Live tasks sharing this process.
    ///
    /// The reader that matters: "is this the last task of this process" was an
    /// O(MAX_TASKS) registry walk under the task-manager lock, on every exit.
    task_count: AtomicU32,

    /// Set once the last task has exited. The id stays allocated and the
    /// registry entry stays resolvable until the process is reaped, so a
    /// `waitpid` that arrives after the exit still finds something to answer
    /// with — and so the id cannot be handed to a stranger in that window.
    exited: AtomicBool,
}

impl Process {
    /// Build an unregistered process. Private: the only way to obtain one is
    /// through [`process_spawn`], which allocates the id, binds the slot and
    /// stamps the self-handle as one step.
    fn new(id: u32, account: AccountId, account_parent: AccountId, parent: u64) -> Self {
        Self {
            id,
            handle: AtomicU64::new(PROCESS_HANDLE_NONE),
            parent: AtomicU64::new(parent),
            account,
            account_parent,
            task_count: AtomicU32::new(0),
            exited: AtomicBool::new(false),
        }
    }

    /// The numeric process id. Display and ABI only.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    /// This process's own handle.
    ///
    /// Stamped at registration, so it is `None` only inside the registry's own
    /// bind step — never on a process any caller can reach.
    #[inline]
    pub fn handle(&self) -> Option<Handle<Process>> {
        unpack_process_handle(self.handle.load(Ordering::Acquire))
    }

    /// The packed form, for storage in a task's `process_handle` field.
    #[inline]
    pub fn handle_raw(&self) -> u64 {
        self.handle.load(Ordering::Acquire)
    }

    /// This process's resource account.
    #[inline]
    pub fn account(&self) -> AccountId {
        self.account
    }

    /// The account this one debits through. Immutable by construction.
    #[inline]
    pub fn account_parent(&self) -> AccountId {
        self.account_parent
    }

    /// The parent process, if it is still resolvable.
    ///
    /// `None` covers both "never had one" and "the parent has been reaped",
    /// which are the same thing to every caller: the process is an orphan.
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

    /// Live task count.
    #[inline]
    pub fn task_count(&self) -> u32 {
        self.task_count.load(Ordering::Acquire)
    }

    /// Join this process. Returns the count after the join.
    ///
    /// Allocation-free and lock-free: one atomic, so it is legal from the
    /// fork path under a preempt guard.
    #[inline]
    pub fn task_join(&self) -> u32 {
        self.task_count.fetch_add(1, Ordering::AcqRel) + 1
    }

    /// Leave this process. Returns `true` if this was the last task.
    ///
    /// Saturating rather than wrapping: a double-leave is a bookkeeping bug,
    /// and the failure it must not produce is a count that wraps to `u32::MAX`
    /// and pins the address space forever.
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
    /// Panic-free and lock-free by construction: the id allocator is an atomic
    /// bitmap, so this is one `fetch_and`. That is what makes the destructor
    /// legal wherever a last release can happen — under the registry lock, on
    /// the exit path with interrupts off, or on a dying task's own unwind.
    ///
    /// The registry slot is *not* released here: releasing it is what caused
    /// this drop.
    fn drop(&mut self) {
        registry::release_process_id(self.id);
    }
}

/// Resolve a handle to the process it names.
///
/// The failure modes are the point: a rebound slot answers
/// [`HandleError::Stale`] rather than handing back whichever process occupies
/// it now.
#[inline]
pub fn process_lookup(handle: Handle<Process>) -> Result<crate::KArc<Process>, HandleError> {
    process_resolve(handle)
}

/// A process named by both halves of its identity: the generation-checked
/// handle that every table keys on, and the numeric id the ABI shows userland.
///
/// This is the type kernel entry points should take instead of a bare `u32`.
/// A `u32` is a *number* — it says nothing about whether the process it named
/// still exists, and ids recycle, so a stale one silently designates whichever
/// process holds that number now. Every such parameter is a confused-deputy
/// surface waiting for a caller to hold one a moment too long.
///
/// The two fields cannot disagree, because the only way to build one outside
/// this module is [`resolve`](Self::resolve), which reads both from the same
/// live process under one lookup. `id` is therefore safe to hand to a log line
/// or a syscall return without a second thought, and `handle` is what every
/// lookup actually uses.
///
/// # Size
///
/// Two `u64`s, and deliberately so. The obvious layout — a [`Handle`] plus a
/// `u32` — is 24 bytes, and this type replaces a `u32` in the argument list of
/// most of the syscall surface. Three frames crossed the 2 KiB stack gate at
/// 24 bytes; the packed form keeps every one of them under it. Storing the
/// handle in its packed encoding rather than as a struct is what buys that,
/// and costs one shift-and-mask per access on paths that are already doing a
/// table lookup.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct ProcessId {
    /// The handle in its [`pack_process_handle`] encoding. Never zero: a
    /// `ProcessId` names a real process, and the "none" case is `Option`.
    packed: u64,
    id: u32,
}

impl ProcessId {
    /// The identity of a live process.
    ///
    /// The only constructor that consults the registry, and the reason the two
    /// halves are always consistent.
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
    /// The bridge for the ABI boundary, where userland hands over a number and
    /// nothing else. `None` for an id naming no live process — which is the
    /// whole point: the failure is at the boundary, once, instead of being
    /// carried inward as a `u32` that later resolves to a stranger.
    #[inline]
    pub fn resolve(id: u32) -> Option<Self> {
        let process = process_find_by_id(id)?;
        Self::of(process.as_ref())
    }

    /// The generation-checked handle. What every table lookup keys on.
    ///
    /// Total: `packed` is non-zero by construction and only
    /// [`pack_process_handle`] ever wrote it, so the unpack always succeeds.
    /// The `unwrap_or` is unreachable and resolves to nothing — slot `u32::MAX`
    /// is outside every table.
    #[inline]
    pub fn handle(self) -> Handle<Process> {
        unpack_process_handle(self.packed).unwrap_or(Handle::from_parts(u32::MAX, u64::MAX))
    }

    /// The numeric id. Display and ABI only — never a lookup key.
    #[inline]
    pub const fn id(self) -> u32 {
        self.id
    }

    /// The process itself, if it is still live.
    ///
    /// Returns `None` once the process has been reaped, even though `self`
    /// still names it: holding a `ProcessId` does not keep a process alive, by
    /// design. It is a designator, not a reference — an owning one would make
    /// every syscall argument a lifetime-extending edge on the process it
    /// mentions.
    #[inline]
    pub fn get(self) -> Option<crate::KArc<Process>> {
        process_for_handle(self.handle())
    }

    /// Whether the process is still live.
    #[inline]
    pub fn is_live(self) -> bool {
        self.get().is_some()
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
    /// The numeric id alone, so a `{}` in a log line reads the way the old
    /// `u32` did.
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(id: u32) -> Process {
        Process::new(id, AccountId::NONE, AccountId::NONE, PROCESS_HANDLE_NONE)
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
        // Suppress the id-release path: this process was never registered.
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
        );
        assert!(p.parent().is_none());
        let new_parent = Handle::<Process>::from_parts(7, 11);
        p.set_parent(Some(new_parent));
        assert_eq!(p.parent(), Some(new_parent));
        // The accounting edge has no setter at all — this is the compile-time
        // half of "charge migration is unrepresentable".
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
    /// Unbiased, that packs to zero and reads back as "no process".
    #[test]
    fn the_first_slot_at_generation_zero_is_not_none() {
        let first = Handle::<Process>::from_parts(0, 0);
        assert_ne!(pack_process_handle(first), PROCESS_HANDLE_NONE);
        assert_eq!(
            unpack_process_handle(pack_process_handle(first)),
            Some(first)
        );
    }

    /// A word whose slot field is zero cannot have come from the packer.
    #[test]
    fn an_unbiased_word_is_refused_rather_than_wrapping() {
        // Slot field 0, generation 5: the shape a caller would produce by
        // packing without the bias.
        let forged = (5u64) << PROCESS_SLOT_BITS;
        assert!(unpack_process_handle(forged).is_none());
    }
}
