//! The process registry: id allocation, slot binding, and handle resolution.
//!
//! Ids are drawn lowest-free from a global atomic bitmap: a stale
//! [`Handle<Process>`] fails the slot's generation check, so reuse needs no
//! delay. The bitmap is global rather than a field of [`ProcessTable`] because
//! [`Process::drop`] returns the id and runs wherever a last release can —
//! under a lock, with interrupts off, on a dying task's own unwind — where
//! taking the registry's cli-spinlock is not legal.
//!
//! The table holds a strong [`KArc<Process>`], so registration *is* ownership:
//! a task names its process by a packed handle, which is a designator and not
//! a reference, so a weak table would leave the object owned by nobody.

use core::sync::atomic::Ordering;

use slopos_abi::task::{INVALID_PROCESS_ID, MAX_PROCESSES};

use crate::atomic_bitmap::AtomicBitmap;
use crate::handle::{Handle, HandleError, HandleTable};
use crate::mm::{AllocError, KArc};
use crate::sync::{LOCK_LEVEL_REGISTRY, SpinLock};
use crate::{KVec, lock_class};

use slopos_abi::quota::ProcCount;

use super::account::{AccountId, alloc_generation};
use super::quota;
use super::{Process, pack_process_handle};

/// Slot width of the packed process handle. Matches
/// [`PROCESS_VM_SLOT_BITS`](crate::handle::PROCESS_VM_SLOT_BITS), so a
/// mistaken packing between the two adjacent `Task` fields is a compile error
/// rather than a silently truncated slot.
pub const PROCESS_SLOT_BITS: u32 = 16;

/// Highest process id the allocator hands out. Ids start at 1, so the space is
/// `1..=MAX_PROCESS_ID`.
pub const MAX_PROCESS_ID: u32 = MAX_PROCESSES as u32;

const _: () = assert!(MAX_PROCESSES <= (1usize << PROCESS_SLOT_BITS));
const _: () = assert!(MAX_PROCESS_ID as usize <= MAX_PROCESSES);

/// Words in the id-occupancy bitmap.
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
    /// The spawner's account is at its process ceiling — the table has room,
    /// this principal does not.
    QuotaExceeded,
}

/// Live process ids. Bit `n` is set while id `n + 1` is allocated.
static PROCESS_IDS: AtomicBitmap<ID_WORDS> = AtomicBitmap::new();

/// Draw the lowest free id, or `None` when the id space is full.
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

/// The registry's own handle type; every caller outside this module holds a
/// `Handle<Process>` instead. The two carry identical bits (`Handle`'s type
/// parameter is `PhantomData`), so the re-tag below is a rename rather than a
/// conversion — confined here so no caller can re-tag a handle between two
/// *different* tables.
type SlotHandle = Handle<KArc<Process>>;

#[inline]
fn as_slot_handle(handle: Handle<Process>) -> SlotHandle {
    SlotHandle::from_parts(handle.slot(), handle.generation())
}

#[inline]
fn as_process_handle(handle: SlotHandle) -> Handle<Process> {
    Handle::from_parts(handle.slot(), handle.generation())
}

/// A slot table of owning process references.
///
/// Takes no lock itself, so a host test can drive a private instance. The
/// global registry is one of these behind a cli-spinlock; nothing else may
/// hold one.
pub struct ProcessTable {
    slots: HandleTable<KArc<Process>>,
}

impl ProcessTable {
    /// A table with every slot pre-created and growth forbidden: `mm` and `fs`
    /// key their own tables on this one's slot index, so the spine must not
    /// move.
    pub fn new() -> Result<Self, AllocError> {
        Ok(Self {
            slots: HandleTable::with_fixed_capacity(MAX_PROCESSES)?,
        })
    }

    /// Bind `process` to a free slot and stamp its self-handle.
    ///
    /// The stamp happens here, so no observer can reach a registered
    /// `Process` whose `handle()` is `None`.
    fn bind(&mut self, process: &KArc<Process>) -> Result<Handle<Process>, ProcessAllocError> {
        let handle = self
            .slots
            .insert(process.clone())
            .map(as_process_handle)
            .map_err(|_| ProcessAllocError::NoFreeSlot)?;
        // Release: a reader resolving the handle from this table also sees the
        // stamp.
        process
            .handle
            .store(pack_process_handle(handle), Ordering::Release);
        Ok(handle)
    }

    /// The entry `handle` names, or why it is unreachable.
    fn entry(&self, handle: Handle<Process>) -> Result<&KArc<Process>, HandleError> {
        self.slots.get(as_slot_handle(handle))
    }

    /// Retire a registration, bumping the slot's generation and returning the
    /// reference the table held so the caller can release it off-lock.
    fn retire(&mut self, handle: Handle<Process>) -> Option<KArc<Process>> {
        self.slots.remove(as_slot_handle(handle)).ok()
    }

    fn len(&self) -> usize {
        self.slots.len()
    }

    fn find_by_id(&self, id: u32) -> Option<KArc<Process>> {
        self.slots
            .iter()
            .find(|(_, process)| process.id() == id)
            .map(|(_, process)| process.clone())
    }

    /// Drop every registration, returning the displaced references so the
    /// caller can release them off-lock.
    fn drain(&mut self) -> KVec<KArc<Process>> {
        let mut handles: KVec<SlotHandle> = KVec::new();
        for handle in self.slots.handles() {
            if handles.push(handle).is_err() {
                break;
            }
        }
        let mut taken: KVec<KArc<Process>> = KVec::new();
        for handle in handles.iter() {
            if let Ok(process) = self.slots.remove(*handle) {
                let _ = taken.push(process);
            }
        }
        taken
    }
}

/// Build a process object without registering it.
///
/// Split from [`process_spawn`] so the id draw and the `KArc` allocation
/// happen before the registry's cli-spinlock is taken: the buddy allocator's
/// reuse path performs synchronous cross-CPU TLB drains.
fn new_process(
    parent: Option<Handle<Process>>,
    account_parent: AccountId,
) -> Result<KArc<Process>, ProcessAllocError> {
    let id = alloc_process_id().ok_or(ProcessAllocError::IdExhausted)?;
    // The arena slot is the process id, drawn from `1..=MAX_PROCESS_ID`, so it
    // never collides with the root's slot 0 and needs no allocator of its own.
    let account = AccountId::from_parts(id, alloc_generation());
    let parent_packed = parent.map_or(super::PROCESS_HANDLE_NONE, pack_process_handle);
    // Charged before the object exists: `MAX_PROCESSES` is reached long before
    // `MAX_TASKS`, so without this one principal can spend the whole table.
    let charge = match quota::try_charge::<ProcCount>(account_parent, 1) {
        Ok(reservation) => reservation,
        Err(_) => {
            release_process_id(id);
            return Err(ProcessAllocError::QuotaExceeded);
        }
    };
    let process = KArc::try_new(Process::new(
        id,
        account,
        account_parent,
        parent_packed,
        charge,
    ))
    .map_err(|_| {
        release_process_id(id);
        ProcessAllocError::OutOfMemory
    })?;
    // A depth refusal leaves an account naming no row, which every arena
    // operation treats as a vacuous success; the charges still reach whichever
    // ancestors do have rows.
    let _ = quota::account_create(account, account_parent);
    Ok(process)
}

/// The kernel's one process table. The spine is allocated once, on first use,
/// outside this lock, and never grows.
static PROCESS_REGISTRY: SpinLock<Option<ProcessTable>> =
    SpinLock::new(None, lock_class!("PROCESS_REGISTRY", LOCK_LEVEL_REGISTRY));

/// Run `f` with the global table, allocating the spine if it is not there.
///
/// The allocation, and the drop of a spine that loses the install race, both
/// happen outside the lock.
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
/// `parent` is the wait edge and may be `None`. `account_parent` is the
/// accounting edge and is fixed here for good: there is no setter.
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
pub fn process_spawn_root() -> Result<KArc<Process>, ProcessAllocError> {
    process_spawn(None, quota::root())
}

/// Resolve a handle to an owning reference.
///
/// [`HandleError::Stale`] means the slot was rebound — the handle names a
/// process that no longer exists, and *not* whichever process holds that slot
/// now; [`HandleError::NoEntry`] means the slot was vacated. The clone is what
/// makes a resolved reference safe to hold across a concurrent reap.
pub fn process_resolve(handle: Handle<Process>) -> Result<KArc<Process>, HandleError> {
    // No registry means nothing was ever registered, so every handle names a
    // slot outside the table.
    with_process_registry(|table| table.entry(handle).cloned()).ok_or(HandleError::OutOfBounds)?
}

/// Resolve a handle, discarding the reason it failed.
#[inline]
pub fn process_for_handle(handle: Handle<Process>) -> Option<KArc<Process>> {
    process_resolve(handle).ok()
}

/// Find a live process by numeric id.
///
/// O(live processes) and deliberately not the fast path: a caller holding a
/// handle must use it. The scan is what `kill(pid, …)` and `waitpid(pid, …)`
/// need, where the caller genuinely has only a number.
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
/// [`HandleError::Stale`], and releases the reference the table held. The id
/// is released in [`Process::drop`] instead, so it is never free while a
/// handle to the process is still live. Returns `false` when the handle did
/// not resolve, making a double reap a no-op rather than a second generation
/// bump against the slot's *next* occupant.
pub fn process_retire(handle: Handle<Process>) -> bool {
    // Released outside the lock, in the shape every other container release
    // uses.
    let released = with_registry_mut(|table| table.retire(handle)).flatten();
    let retired = released.is_some();
    if let Some(process) = released.as_deref() {
        // The charge is released at the reap, not at the final `Drop`, which
        // would keep the spawner paying for as long as anything still held a
        // reference.
        process.release_proc_charge();
    }
    drop(released);
    retired
}

/// Retire every registration and clear the id space. Test-fixture only.
///
/// The generation counter deliberately survives: it is the only thing
/// separating a pre-reset handle from the slot's next occupant.
pub fn process_registry_reset() {
    let removed = with_registry_mut(|table| table.drain()).unwrap_or_default();
    // Off-lock: a released weak handle can be the allocation's last reference.
    drop(removed);

    // Account rows go with the id space: a `Process` that outlived the drain
    // still holds its spawner's `ProcCount` charge, and darkening the row now
    // makes that late refund a generation-mismatch no-op rather than a debit
    // against whoever is reissued the slot.
    for id in 1..=MAX_PROCESS_ID {
        quota::account_release_by_slot(id);
        release_process_id(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run `f` against a private table, serialised against every other test in
    /// this binary.
    ///
    /// Private, because the global registry's cli-spinlock takes a
    /// `gs`-relative `PreemptGuard` that faults on a host with no PCR.
    /// Serialised anyway, because `Process::drop` returns ids to the
    /// process-global bitmap, which no test can make private.
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
        table.entry(handle).cloned()
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
            assert!(t.retire(stale).is_some());
            drop(first);

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

    #[test]
    fn a_recycled_id_does_not_resolve_to_the_prior_principal() {
        with_table(|t| {
            let (first, stale) = spawn_into(t);
            let first_id = first.id();
            drop(t.retire(stale));
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
            drop(t.retire(handle));

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

            drop(t.retire(parent_handle));
            drop(parent);

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
            assert!(t.retire(handle).is_some());
            assert!(
                t.retire(handle).is_none(),
                "a second reap must not bump the generation again"
            );
        });
    }

    #[test]
    fn registration_owns_the_process() {
        with_table(|t| {
            let (process, handle) = spawn_into(t);
            let id = process.id();
            drop(process);

            let still_there = resolve(t, handle).expect("registration keeps it alive");
            assert_eq!(still_there.id(), id);
            drop(still_there);

            let released = t.retire(handle).expect("retire returns the entry");
            drop(released);
            let (reuser, _) = spawn_into(t);
            assert_eq!(reuser.id(), id);
        });
    }

    #[test]
    fn a_vacated_slot_resolves_to_no_entry() {
        with_table(|t| {
            let (process, handle) = spawn_into(t);
            drop(t.retire(handle));
            drop(process);
            assert_eq!(resolve(t, handle).err(), Some(HandleError::NoEntry));
        });
    }

    /// Against a *small* table, because the id bitmap is process-global:
    /// filling a `MAX_PROCESSES`-wide one would spend the whole id space, and
    /// a sibling test drawing an id in that window would see `IdExhausted` for
    /// reasons nothing to do with it.
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
    fn a_process_id_does_not_follow_a_recycled_number() {
        with_table(|t| {
            let (first, handle) = spawn_into(t);
            let stale = super::super::ProcessId::of(&first).expect("identity");
            assert_eq!(stale.id(), first.id());
            assert_eq!(stale.handle(), handle);

            drop(t.retire(handle));
            drop(first);

            let (second, _) = spawn_into(t);
            assert_eq!(
                second.id(),
                stale.id(),
                "the number must recycle or this proves nothing"
            );
            assert_eq!(
                resolve(t, stale.handle()).err(),
                Some(HandleError::Stale),
                "a ProcessId must not follow its number onto a new process"
            );
        });
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
