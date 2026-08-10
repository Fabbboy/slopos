//! The process registry: id allocation, slot binding, and handle resolution.
//!
//! # Two allocators, deliberately different shapes
//!
//! **Ids come from an occupancy bitmap, not a FIFO reuse ring.** The ring the
//! address-space allocator carries exists to soften a hazard it cannot fix:
//! with no generation on the id, an id handed straight back to the next
//! process makes a stale reference resolve to something live, so the ring
//! delays reuse until every other free id has been drawn. That is damage
//! control, and once the designator carries a generation it buys nothing while
//! costing an allocator whose reuse order is a documented contract. Here a
//! stale [`Handle<Process>`] fails the generation check outright, so ids are
//! drawn lowest-free and the ring is gone.
//!
//! The bitmap is global and atomic rather than a field of [`ProcessTable`],
//! because [`Process::drop`] returns the id and that destructor runs wherever
//! a last release can — under a lock, with interrupts off, on a dying task's
//! own unwind. One `fetch_and` is legal in all of those; taking the registry's
//! cli-spinlock is not.
//!
//! **Slots come from a [`HandleTable`] with fixed capacity.** Fixed because
//! the spine must never reallocate: `mm` and `fs` key their own tables on this
//! table's slot index, and a moving spine would invalidate that. The
//! generation lives in the slot and is bumped on removal, so a handle minted
//! for a previous occupant resolves to [`HandleError::Stale`].
//!
//! # Weak entries, and why a zombie stays resolvable
//!
//! The table holds [`KWeak<Process>`], never a strong reference. A strong one
//! would make the registry the owner and the last task's exit *not* the last
//! release, which is precisely the decision the object exists to make
//! answerable. The owning reference lives in the spawn lease and, after that,
//! in each task's handle.
//!
//! A process whose tasks have all exited stays in the table until it is
//! reaped, because that is the window a `waitpid` arrives in. Its id stays
//! allocated for the same reason: an id released at last-task-exit could be
//! handed to a stranger before the parent has read the exit status, and the
//! parent would then be told about the wrong process. The id comes back in
//! [`Process::drop`], which runs when the last handle goes — after the reap.

use core::sync::atomic::Ordering;

use slopos_abi::task::{INVALID_PROCESS_ID, MAX_PROCESSES};

use crate::atomic_bitmap::AtomicBitmap;
use crate::handle::{Handle, HandleError, HandleTable};
use crate::mm::{AllocError, KArc, KWeak};
use crate::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use crate::{KVec, lock_class};

use super::account::{AccountId, alloc_generation, root_account};
use super::{Process, pack_process_handle};

/// Slot width of the packed process handle.
///
/// Matches [`PROCESS_VM_SLOT_BITS`](crate::handle::PROCESS_VM_SLOT_BITS): the
/// two handles are stored the same way in adjacent `Task` fields, and one
/// width for both means a mistaken packing is a compile error at the call
/// site rather than a silently truncated slot.
pub const PROCESS_SLOT_BITS: u32 = 16;

/// Highest process id the allocator hands out. Ids start at 1, so the space is
/// `1..=MAX_PROCESS_ID`.
pub const MAX_PROCESS_ID: u32 = MAX_PROCESSES as u32;

const _: () = assert!(MAX_PROCESSES <= (1usize << PROCESS_SLOT_BITS));
const _: () = assert!(MAX_PROCESS_ID as usize <= MAX_PROCESSES);

/// Words in the id-occupancy bitmap. Bit `n` means id `n + 1` is taken.
const ID_WORDS: usize = MAX_PROCESSES.div_ceil(usize::BITS as usize);

/// Why a process could not be created.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessAllocError {
    /// Every id in `1..=MAX_PROCESS_ID` is taken.
    IdExhausted,
    /// Every registry slot is bound.
    NoFreeSlot,
    /// The heap refused the `KArc<Process>` or the registry spine.
    OutOfMemory,
}

/// Live process ids. Bit `n` is set while id `n + 1` is allocated.
static PROCESS_IDS: AtomicBitmap<ID_WORDS> = AtomicBitmap::new();

/// Draw the lowest free id.
///
/// Lowest-free rather than FIFO: see the module docs. `None` when the id space
/// is full.
fn alloc_process_id() -> Option<u32> {
    PROCESS_IDS.alloc(MAX_PROCESSES).map(|bit| bit as u32 + 1)
}

/// Give an id back. Called only from [`Process::drop`].
pub(super) fn release_process_id(id: u32) {
    if id == INVALID_PROCESS_ID || id == 0 || id > MAX_PROCESS_ID {
        return;
    }
    PROCESS_IDS.free((id - 1) as usize);
}

// ---------------------------------------------------------------------------
// The table
// ---------------------------------------------------------------------------

/// The registry's own handle type.
///
/// [`HandleTable<KWeak<Process>>`] mints `Handle<KWeak<Process>>`, but every
/// caller outside this module holds a `Handle<Process>` — the weak reference
/// is storage, not identity, and no consumer should have to name it. The two
/// carry identical bits (`Handle`'s type parameter is `PhantomData`), so the
/// re-tag below is a rename rather than a conversion. Confined to this module
/// so no caller can re-tag a handle between two *different* tables.
type SlotHandle = Handle<KWeak<Process>>;

#[inline]
fn as_slot_handle(handle: Handle<Process>) -> SlotHandle {
    SlotHandle::from_parts(handle.slot(), handle.generation())
}

#[inline]
fn as_process_handle(handle: SlotHandle) -> Handle<Process> {
    Handle::from_parts(handle.slot(), handle.generation())
}

/// A slot table of weak process references.
///
/// A distinct type rather than a bare `HandleTable` behind the global lock,
/// so the binding and resolution logic has a home that takes no lock and can
/// therefore be exercised by a host test on a private instance. The global
/// registry is one of these behind a cli-spinlock; nothing else may hold one.
pub struct ProcessTable {
    slots: HandleTable<KWeak<Process>>,
}

impl ProcessTable {
    /// A table with every slot pre-created and growth forbidden.
    ///
    /// Fixed capacity is the contract `mm` and `fs` depend on: they key their
    /// own tables on this one's slot index, so the spine must not move.
    pub fn new() -> Result<Self, AllocError> {
        Ok(Self {
            slots: HandleTable::with_fixed_capacity(MAX_PROCESSES)?,
        })
    }

    /// Bind `process` to a free slot and stamp its self-handle.
    ///
    /// The stamp happens here, under whatever exclusion the caller holds, so
    /// no observer can reach a registered `Process` whose `handle()` is
    /// `None`.
    fn bind(&mut self, process: &KArc<Process>) -> Result<Handle<Process>, ProcessAllocError> {
        let weak = KArc::downgrade(process);
        let handle = self
            .slots
            .insert(weak)
            .map(as_process_handle)
            .map_err(|_| ProcessAllocError::NoFreeSlot)?;
        // Release, so a reader that resolves the handle out of this table also
        // sees the stamp.
        process
            .handle
            .store(pack_process_handle(handle), Ordering::Release);
        Ok(handle)
    }

    /// The weak entry `handle` names, or why it is unreachable.
    fn entry(&self, handle: Handle<Process>) -> Result<&KWeak<Process>, HandleError> {
        self.slots.get(as_slot_handle(handle))
    }

    /// Retire a registration, bumping the slot's generation.
    fn retire(&mut self, handle: Handle<Process>) -> bool {
        self.slots.remove(as_slot_handle(handle)).is_ok()
    }

    /// Slots currently bound.
    fn len(&self) -> usize {
        self.slots.len()
    }

    /// The live process carrying `id`, if any.
    fn find_by_id(&self, id: u32) -> Option<KArc<Process>> {
        self.slots
            .iter()
            .find_map(|(_, weak)| weak.upgrade().filter(|process| process.id() == id))
    }

    /// Drop every registration, returning the displaced weak handles so the
    /// caller can release them off-lock.
    fn drain(&mut self) -> KVec<KWeak<Process>> {
        let mut handles: KVec<SlotHandle> = KVec::new();
        for handle in self.slots.handles() {
            if handles.push(handle).is_err() {
                break;
            }
        }
        let mut taken: KVec<KWeak<Process>> = KVec::new();
        for handle in handles.iter() {
            if let Ok(weak) = self.slots.remove(*handle) {
                let _ = taken.push(weak);
            }
        }
        taken
    }
}

/// Build a process object without registering it.
///
/// Split out from [`process_spawn`] so the id draw and the `KArc` allocation
/// happen before any lock is taken: the registry lock is a cli-spinlock, and
/// the buddy allocator's reuse path performs synchronous cross-CPU TLB drains.
fn new_process(
    parent: Option<Handle<Process>>,
    account_parent: AccountId,
) -> Result<KArc<Process>, ProcessAllocError> {
    let id = alloc_process_id().ok_or(ProcessAllocError::IdExhausted)?;
    let account = AccountId::from_parts(id, alloc_generation());
    let parent_packed = parent.map_or(super::PROCESS_HANDLE_NONE, pack_process_handle);
    KArc::try_new(Process::new(id, account, account_parent, parent_packed)).map_err(|_| {
        release_process_id(id);
        ProcessAllocError::OutOfMemory
    })
}

// ---------------------------------------------------------------------------
// The global registry
// ---------------------------------------------------------------------------

/// The kernel's one process table.
///
/// `LOCK_LEVEL_REGISTRY`. The spine is allocated once, on first use, outside
/// this lock, and never grows.
static PROCESS_REGISTRY: SpinLock<Option<ProcessTable>> =
    SpinLock::new(None, lock_class!("PROCESS_REGISTRY", LOCK_LEVEL_REGISTRY));

/// Run `f` with the global table, allocating the spine if it is not there.
///
/// The allocation happens *outside* the lock, and a spine that loses the
/// install race is dropped outside it too.
fn with_registry_mut<R>(f: impl FnOnce(&mut ProcessTable) -> R) -> Option<R> {
    {
        let mut guard = PROCESS_REGISTRY.lock();
        if let Some(table) = guard.as_mut() {
            return Some(f(table));
        }
    }
    let fresh = ProcessTable::new().ok()?;
    let leftover = {
        let mut guard = PROCESS_REGISTRY.lock();
        if guard.is_some() {
            Some(fresh)
        } else {
            *guard = Some(fresh);
            None
        }
    };
    drop(leftover);
    let mut guard = PROCESS_REGISTRY.lock();
    guard.as_mut().map(f)
}

/// Run `f` with the global table if it exists. Never allocates, so this is the
/// form every read path uses.
pub fn with_process_registry<R>(f: impl FnOnce(&ProcessTable) -> R) -> Option<R> {
    let guard = PROCESS_REGISTRY.lock();
    guard.as_ref().map(f)
}

/// Create a process and publish it, returning the owning reference.
///
/// `parent` is the wait edge and may be `None` (a process with no parent, such
/// as the first user process). `account_parent` is the accounting edge and is
/// fixed here for good: there is no setter.
pub fn process_spawn(
    parent: Option<Handle<Process>>,
    account_parent: AccountId,
) -> Result<KArc<Process>, ProcessAllocError> {
    let process = new_process(parent, account_parent)?;
    match with_registry_mut(|table| table.bind(&process)) {
        // The process's own `Drop` returns the id on either failure path.
        Some(Ok(_)) => Ok(process),
        Some(Err(error)) => Err(error),
        None => Err(ProcessAllocError::OutOfMemory),
    }
}

/// Create the root process: no wait parent, and the kernel's root account as
/// the accounting parent.
///
/// Distinct from [`process_spawn`] rather than a `None` argument to it,
/// because "billed to the kernel's root account" is a different fact from
/// "billed to whoever spawned me", and a call site that reaches for the root
/// should have to name it.
pub fn process_spawn_root() -> Result<KArc<Process>, ProcessAllocError> {
    process_spawn(None, root_account())
}

/// Resolve a handle to an owning reference.
///
/// [`HandleError::Stale`] means the slot was rebound — the handle names a
/// process that no longer exists, and *not* whichever process holds that slot
/// now. [`HandleError::NoEntry`] means the slot holds nothing live: either it
/// was vacated and not yet reused, or its process is mid-teardown. Both are
/// the point of the exercise.
pub fn process_resolve(handle: Handle<Process>) -> Result<KArc<Process>, HandleError> {
    // No registry at all means nothing has ever been registered, so every
    // handle names a slot outside the table.
    let weak = with_process_registry(|table| table.entry(handle).cloned())
        .ok_or(HandleError::OutOfBounds)??;
    weak.upgrade().ok_or(HandleError::NoEntry)
}

/// Resolve a handle, discarding the reason it failed.
#[inline]
pub fn process_for_handle(handle: Handle<Process>) -> Option<KArc<Process>> {
    process_resolve(handle).ok()
}

/// Find a live process by numeric id.
///
/// O(live processes) and deliberately not the fast path: ids are for display
/// and for the syscall ABI, and a caller holding a handle must use it. The
/// scan is what `kill(pid, …)` and `waitpid(pid, …)` need, where the caller
/// genuinely has only a number.
pub fn process_find_by_id(id: u32) -> Option<KArc<Process>> {
    if id == INVALID_PROCESS_ID || id == 0 {
        return None;
    }
    with_process_registry(|table| table.find_by_id(id))?
}

/// Number of registry slots currently bound.
pub fn process_count() -> usize {
    with_process_registry(|table| table.len()).unwrap_or(0)
}

/// Retire a process's registry entry: the reap.
///
/// Bumps the slot's generation, so every outstanding handle to it becomes
/// [`HandleError::Stale`] from this point. The id is *not* released here —
/// that happens in [`Process::drop`], when the last owning reference goes, so
/// no window exists in which the id is free while a handle to the process is
/// still live.
///
/// Returns `false` if the handle did not resolve, which is the idempotent
/// case: a double reap is a no-op rather than a second generation bump that
/// would invalidate the slot's *next* occupant's handles.
pub fn process_retire(handle: Handle<Process>) -> bool {
    with_registry_mut(|table| table.retire(handle)).unwrap_or(false)
}

/// Retire every registration and clear the id space. Test-fixture only.
///
/// The generation counter deliberately survives: it is the only thing
/// separating a handle minted before the reset from the slot's next occupant,
/// so it stays globally monotonic across one. Same contract `VmSlotAlloc::reset`
/// keeps for its own.
pub fn process_registry_reset() {
    let removed = with_registry_mut(|table| table.drain()).unwrap_or_default();
    // Off-lock: a released weak handle can be the allocation's last reference.
    drop(removed);

    for id in 1..=MAX_PROCESS_ID {
        release_process_id(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` against a private table, serialised against every other test in
    /// this binary.
    ///
    /// **Private table**, because the global registry sits behind a
    /// cli-spinlock whose `PreemptGuard` is a `gs`-relative RMW — it faults on
    /// a host with no PCR. What is under test is the binding and resolution
    /// algebra, which the table owns and the lock only serialises.
    ///
    /// **Serialised anyway**, because the *id* allocator cannot be made
    /// private: `Process::drop` returns ids to the global bitmap, so a private
    /// one would desynchronise from the destructor. Two tests drawing from it
    /// concurrently would see each other's ids, and the exhaustion test would
    /// starve any sibling that ran inside its window.
    fn with_table<R>(f: impl FnOnce(&mut ProcessTable) -> R) -> R {
        let _serial = crate::test_support::global_lock::lock_global_test_state();
        let mut table = ProcessTable::new().expect("private process table");
        f(&mut table)
    }

    fn spawn_into(table: &mut ProcessTable) -> (KArc<Process>, Handle<Process>) {
        let process = new_process(None, AccountId::NONE).expect("process");
        let handle = table.bind(&process).expect("bind");
        (process, handle)
    }

    fn resolve(
        table: &ProcessTable,
        handle: Handle<Process>,
    ) -> Result<KArc<Process>, HandleError> {
        table.entry(handle)?.upgrade().ok_or(HandleError::NoEntry)
    }

    #[test]
    fn bind_publishes_a_resolvable_self_handle() {
        with_table(|t| {
            let (process, handle) = spawn_into(t);
            assert_eq!(
                process.handle(),
                Some(handle),
                "a bound process carries the handle it was bound under"
            );
            assert_eq!(resolve(t, handle).expect("resolve").id(), process.id());
        });
    }

    #[test]
    fn a_retired_slot_makes_its_handle_stale() {
        with_table(|t| {
            let (first, stale) = spawn_into(t);
            assert!(t.retire(stale));
            drop(first);

            // `with_fixed_capacity` recycles the lowest slot first, so the next
            // bind lands on the slot the retired handle names.
            let (second, fresh) = spawn_into(t);
            assert_eq!(
                stale.slot(),
                fresh.slot(),
                "the slot must be recycled to prove the point"
            );
            assert_eq!(
                resolve(t, stale).err(),
                Some(HandleError::Stale),
                "a handle to the previous occupant must not resolve to the new one"
            );
            assert_eq!(resolve(t, fresh).expect("fresh resolves").id(), second.id());
        });
    }

    /// The property phase 4 exists to deliver: a recycled *id* does not
    /// resolve to the prior principal.
    #[test]
    fn a_recycled_id_does_not_resolve_to_the_prior_principal() {
        with_table(|t| {
            let (first, stale) = spawn_into(t);
            let first_id = first.id();
            t.retire(stale);
            // The id comes back here, in `Drop`, not at retire.
            drop(first);

            let (second, _) = spawn_into(t);
            assert_eq!(
                second.id(),
                first_id,
                "lowest-free allocation must reissue the id, or this proves nothing"
            );
            assert_eq!(
                resolve(t, stale).err(),
                Some(HandleError::Stale),
                "same id, different principal — the old designator must say so"
            );
        });
    }

    #[test]
    fn an_id_is_held_until_the_process_drops_not_until_it_retires() {
        with_table(|t| {
            let (process, handle) = spawn_into(t);
            let id = process.id();
            t.retire(handle);

            // Retired but still referenced: the id is still spoken for, so a
            // `waitpid` arriving now cannot be answered about a stranger.
            let (other, _) = spawn_into(t);
            assert_ne!(
                other.id(),
                id,
                "a reaped-but-referenced id must not reissue"
            );
            drop(other);

            drop(process);
            let (reuser, _) = spawn_into(t);
            assert_eq!(reuser.id(), id, "the id returns once the last handle goes");
        });
    }

    #[test]
    fn live_ids_are_unique() {
        with_table(|t| {
            let mut held: KVec<KArc<Process>> = KVec::new();
            for _ in 0..8 {
                let (process, _) = spawn_into(t);
                held.push(process).expect("hold");
            }
            let mut ids: KVec<u32> = KVec::new();
            for process in held.iter() {
                ids.push(process.id()).expect("collect");
            }
            ids.sort_unstable();
            for pair in ids.windows(2) {
                assert_ne!(pair[0], pair[1], "live ids must not collide");
            }
            assert_eq!(t.len(), 8);
        });
    }

    #[test]
    fn find_by_id_ignores_zero_and_the_sentinel() {
        with_table(|t| {
            assert!(t.find_by_id(0).is_none());
            assert!(t.find_by_id(INVALID_PROCESS_ID).is_none());
            let (process, _) = spawn_into(t);
            assert_eq!(t.find_by_id(process.id()).expect("find").id(), process.id());
        });
    }

    #[test]
    fn the_wait_edge_rehomes_and_the_accounting_edge_does_not() {
        with_table(|t| {
            let (parent, parent_handle) = spawn_into(t);
            let child = new_process(Some(parent_handle), parent.account()).expect("child");
            t.bind(&child).expect("bind child");

            assert_eq!(child.parent(), Some(parent_handle));
            assert_eq!(child.account_parent(), parent.account());

            // Reparent-to-init moves the wait edge; the budget stays put. There
            // is no setter for the other side at all — this asserts the runtime
            // half of a property the type system already carries.
            child.set_parent(None);
            assert!(child.parent().is_none());
            assert_eq!(
                child.account_parent(),
                parent.account(),
                "reparenting must not move a charge"
            );
        });
    }

    #[test]
    fn a_reparented_child_sees_its_old_parent_as_stale_not_as_a_stranger() {
        with_table(|t| {
            let (parent, parent_handle) = spawn_into(t);
            let child = new_process(Some(parent_handle), parent.account()).expect("child");
            t.bind(&child).expect("bind child");

            // The parent exits and is reaped while the child still names it.
            t.retire(parent_handle);
            drop(parent);

            // Its slot is taken by an unrelated process.
            let (_stranger, stranger_handle) = spawn_into(t);
            assert_eq!(stranger_handle.slot(), parent_handle.slot());

            let orphan_edge = child.parent().expect("the edge is still stored");
            assert_eq!(
                resolve(t, orphan_edge).err(),
                Some(HandleError::Stale),
                "an orphan's stale parent edge must not resolve to the slot's new occupant"
            );
        });
    }

    #[test]
    fn retire_is_idempotent() {
        with_table(|t| {
            let (_process, handle) = spawn_into(t);
            assert!(t.retire(handle));
            assert!(
                !t.retire(handle),
                "a second reap must not bump the generation again"
            );
        });
    }

    #[test]
    fn a_process_with_no_strong_refs_resolves_to_no_entry() {
        with_table(|t| {
            let (process, handle) = spawn_into(t);
            drop(process);
            // Still registered, but nothing owns it: the weak upgrade fails, and
            // the answer distinguishes that from a rebound slot.
            assert_eq!(resolve(t, handle).err(), Some(HandleError::NoEntry));
        });
    }

    /// A fixed-capacity spine refuses rather than reallocating.
    ///
    /// Under the global-test-state lock and against a *small* table, because
    /// the id bitmap is process-global: filling a `MAX_PROCESSES`-wide table
    /// would spend the whole id space, and a sibling test drawing an id in
    /// that window would see `IdExhausted` for reasons nothing to do with it.
    /// The property under test is the spine's refusal, which any capacity
    /// exhibits.
    #[test]
    fn a_full_table_refuses_rather_than_growing() {
        let _serial = crate::test_support::global_lock::lock_global_test_state();
        let mut t = ProcessTable {
            slots: HandleTable::with_fixed_capacity(2).expect("small table"),
        };
        let mut held: KVec<KArc<Process>> = KVec::new();
        for _ in 0..2 {
            let process = new_process(None, AccountId::NONE).expect("process");
            t.bind(&process).expect("bind");
            held.push(process).expect("hold");
        }
        let overflow = new_process(None, AccountId::NONE).expect("process");
        assert_eq!(
            t.bind(&overflow).err(),
            Some(ProcessAllocError::NoFreeSlot),
            "a fixed-capacity spine must refuse rather than reallocate"
        );
    }

    /// The id space is finite and the allocator says so rather than handing
    /// out a duplicate.
    #[test]
    fn an_exhausted_id_space_refuses() {
        let _serial = crate::test_support::global_lock::lock_global_test_state();
        let mut held: KVec<KArc<Process>> = KVec::new();
        let mut drawn = 0usize;
        while let Ok(process) = new_process(None, AccountId::NONE) {
            drawn += 1;
            if held.push(process).is_err() {
                break;
            }
        }
        assert_eq!(
            drawn, MAX_PROCESSES,
            "the allocator must hand out exactly the id space, then refuse"
        );
        assert_eq!(
            new_process(None, AccountId::NONE).err(),
            Some(ProcessAllocError::IdExhausted)
        );
        // Give the space back, or every later test in this binary starves.
        drop(held);
        assert!(new_process(None, AccountId::NONE).is_ok());
    }

    #[test]
    fn drain_empties_the_table() {
        with_table(|t| {
            let (_a, _) = spawn_into(t);
            let (_b, _) = spawn_into(t);
            assert_eq!(t.len(), 2);
            let taken = t.drain();
            assert_eq!(taken.len(), 2);
            assert_eq!(t.len(), 0);
        });
    }
}
