use core::sync::atomic::Ordering;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::KVec;
use slopos_ostd::handle::Handle;
use slopos_ostd::process::Process;

use super::*;

/// Mint one console `OpenFile` for fd `stdin`/`stdout`/`stderr`. Each
/// holds its own alias of the shared console backing, dropped with the
/// `OpenFile`.
fn open_console_fd(
    tty_ops: &'static dyn FileOps,
    console_tty: TtyIndex,
    flags: OpenMode,
    backing: KArc<dyn slopos_abi::file_ops::FileBacking>,
) -> Option<KArc<OpenFile>> {
    new_open_file(tty_ops, console_tty.0 as usize, flags, 0, Some(backing))
}

fn bootstrap_console_fds(
    inner: &mut FileTableSlotInner,
    external_ops: &ExternalOpsState,
    account: AccountId,
) {
    let tty_ops = effective_tty_ops(external_ops);
    let console_tty = tty::default_console_tty();

    let Ok(stdin_res) = try_charge::<FdSlot>(account, 1) else {
        return;
    };
    let Ok(stdout_res) = try_charge::<FdSlot>(account, 1) else {
        return;
    };
    let Ok(stderr_res) = try_charge::<FdSlot>(account, 1) else {
        return;
    };

    // One console open shared by all three standard fds; each `OpenFile`
    // holds a clone. All-or-nothing: on any failure the already-built
    // `OpenFile`s (and the backing) drop at scope exit.
    let Ok(backing) = tty::open_tty(console_tty) else {
        return;
    };

    let Some(stdin) = open_console_fd(tty_ops, console_tty, OpenMode::READ, backing.clone()) else {
        return;
    };
    let Some(stdout) = open_console_fd(tty_ops, console_tty, OpenMode::WRITE, backing.clone())
    else {
        return;
    };
    let Some(stderr) = open_console_fd(tty_ops, console_tty, OpenMode::WRITE, backing) else {
        return;
    };

    inner.descriptors[0] = Some(FdEntry::new(stdin, FdFlags::NONE, stdin_res));
    inner.descriptors[1] = Some(FdEntry::new(stdout, FdFlags::NONE, stdout_res));
    inner.descriptors[2] = Some(FdEntry::new(stderr, FdFlags::NONE, stderr_res));
}

/// Lift the lowest-numbered descriptor out of `slot`, holding its lock for
/// exactly that long.
///
/// One entry per acquisition rather than a collected batch: an `OpenFile`
/// teardown reaches an arbitrary backing release — a socket passed over a
/// socket recurses back into this module — so none of it may run under the
/// table lock, and collecting the entries first would mean growing a vector
/// under a cli-lock, which puts the whole heap path beneath the descriptor
/// table. Teardown is not a hot path; a bounded run of uncontended
/// acquisitions is the cheaper half of that trade.
fn take_next_descriptor(slot: &'static FileTableSlot) -> Option<FdEntry> {
    let mut inner = slot.inner.lock();
    inner.descriptors.iter_mut().find_map(|entry| entry.take())
}

/// Create a console-bootstrapped fd table for `process`. Returns 0, or -1 if
/// it already has one or its handle no longer resolves.
///
/// A process that already carries a table is a refusal rather than a silent
/// success: answering "done" would hand the caller the existing table's
/// descriptors.
pub fn fileio_create_table_for_process(process: Handle<Process>) -> i32 {
    // Built before the slot is claimed: the array is the table's one
    // allocation, and it must not happen under the slot lock.
    let Some(descriptors) = new_descriptor_table() else {
        return -1;
    };
    // CAS-claim so two concurrent creates for the same process cannot both win.
    let Some(slot) = claim_process_slot(process) else {
        return -1;
    };
    let external_ops = with_open_files(|state| state.external_ops);
    let account = account_of(process);
    let mut inner = slot.inner.lock();
    inner.in_use = true;
    inner.descriptors = descriptors;
    bootstrap_console_fds(&mut inner, &external_ops, account);
    0
}

/// The account a process's descriptor numbers are charged to.
///
/// [`AccountId::NONE`] for a handle that no longer resolves, which every arena
/// operation treats as a vacuous success — never the root's account, which
/// would bill the kernel for a user process's descriptors.
fn account_of(process: Handle<Process>) -> AccountId {
    slopos_ostd::process::process_for_handle(process)
        .map_or(AccountId::NONE, |process| process.account())
}

/// Claim `process`'s own table slot; the caller then locks it and sets
/// `in_use`. `None` if the slot is already bound.
///
/// Not a search. The slot index is the process's registry slot, so there is
/// exactly one slot a given process may have, and a failed claim means that
/// process already has a table — a caller bug — rather than a full table. The
/// CAS still carries the claim, so two concurrent creates for the same process
/// cannot both win.
fn claim_process_slot(process: Handle<Process>) -> Option<&'static FileTableSlot> {
    let slot = PROCESS_TABLES.get(process.slot() as usize)?;
    // Generation first: a reader that observes an occupied `process_id` must
    // already be able to see the matching generation.
    slot.generation
        .store(process.generation(), Ordering::Release);
    if slot
        .process_id
        .compare_exchange(
            INVALID_PROCESS_ID,
            process_id_of(process),
            Ordering::AcqRel,
            Ordering::Acquire,
        )
        .is_err()
    {
        return None;
    }
    Some(slot)
}

/// The numeric id of the process `handle` names, for the slot's display field.
/// `INVALID_PROCESS_ID` if it no longer resolves — which makes the claim above
/// fail closed rather than binding a slot to a process that has gone.
fn process_id_of(handle: Handle<Process>) -> u32 {
    slopos_ostd::process::process_for_handle(handle)
        .map_or(INVALID_PROCESS_ID, |process| process.id())
}

/// Create an empty fd table for `process` — no console bootstrap. Spawn's
/// fd-action allow-list installs every descriptor the child inherits, so it
/// starts from nothing. Returns 0, or -1 as above.
pub fn fileio_create_empty_table_for_process(process: Handle<Process>) -> i32 {
    let Some(descriptors) = new_descriptor_table() else {
        return -1;
    };
    let Some(slot) = claim_process_slot(process) else {
        return -1;
    };
    let mut inner = slot.inner.lock();
    inner.in_use = true;
    inner.descriptors = descriptors;
    0
}

/// Release the descriptor table `process` owns.
pub fn fileio_destroy_table_for_process(process: Handle<Process>) {
    let Some(slot) = slot_for_process(process) else {
        return;
    };
    destroy_table_in_slot(slot);
}

/// Whether `process` currently owns a descriptor table.
///
/// A generation-checked question, so a handle whose slot has been rebound
/// answers `false` rather than reporting the new occupant's table as its own.
pub fn fileio_table_exists_for_process(process: Handle<Process>) -> bool {
    slot_for_process(process).is_some()
}

/// Release every bound descriptor table. Fixture reset only.
///
/// The counterpart to `slopos_mm::process_vm::init_process_vm`, and the reason
/// mm no longer needs a runtime-installed hook to reach fs: a reset that
/// releases address spaces has to release descriptor tables too, and fs sits
/// *above* mm in the crate graph, so the call goes this way round without an
/// indirection. The hook existed only because the dependency pointed the
/// wrong way for a pid-keyed teardown.
pub fn fileio_reset_all_tables() {
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) != INVALID_PROCESS_ID {
            destroy_table_in_slot(slot);
        }
    }
}

/// Drain and release a bound table slot.
///
/// Split from the two entry points above so the drain protocol — close to new
/// installs, drain off-lock one entry at a time, free the array off-lock — has
/// one implementation rather than one per way of naming the slot.
fn destroy_table_in_slot(slot: &'static FileTableSlot) {
    {
        let mut inner = slot.inner.lock();
        if !inner.in_use {
            return;
        }
        // Closed to new installs before the first descriptor leaves, so the
        // drain below can release the lock between entries without racing one.
        inner.in_use = false;
    }
    while let Some(entry) = take_next_descriptor(slot) {
        drop(entry);
    }
    // The array itself goes back to the heap off-lock, for the same reason it
    // was built off-lock.
    let released = core::mem::take(&mut slot.inner.lock().descriptors);
    // Id first, then generation: a reader checks occupancy before the
    // generation, so clearing in this order never shows an occupied slot with
    // a cleared generation.
    slot.process_id.store(INVALID_PROCESS_ID, Ordering::Release);
    slot.generation.store(0, Ordering::Release);
    drop(released);
}

/// Fork-style clone: every valid descriptor is duplicated except those
/// marked [`FdFlags::close_on_fork`], which the child does not receive at
/// all. Close-on-exec descriptors *are* duplicated (POSIX fork keeps
/// `FD_CLOEXEC`; only `exec` strips them) — the two bits are independent.
/// Spawn does not clone tables; it builds the child's from an fd-action
/// allow-list.
pub fn fileio_clone_table_for_process(src: FdTable, dst: Handle<Process>) -> i32 {
    // Step 1: snapshot src descriptors into a heap `KVec` (NOT a stack
    // array — a `[Option<FdEntry>; 32]` on the frame blows the 2 KiB
    // stack gate). Each clone bumps a `KArc<OpenFile>` strong count
    // (safe under the lock: a clone never runs teardown), tagged with its
    // fd number.
    let src_slot = match slot_for_table(src) {
        Some(slot) => slot,
        None => return -1,
    };
    // The child pays for its own descriptor numbers. A cloned entry carrying
    // the parent's token would refund the parent when the *child* closed it,
    // which is the double-refund the non-`Clone` `FdEntry` exists to prevent.
    let dst_account = account_of(dst);
    let mut snapshot: KVec<(usize, FdEntry)> = KVec::new();
    {
        let guard = src_slot.inner.lock();
        if !guard.in_use {
            return -1;
        }
        for (i, src_fd) in guard.descriptors.iter().enumerate() {
            let Some(src_fd) = src_fd else { continue };
            if src_fd.close_on_fork {
                continue;
            }
            // A child that cannot afford the parent's descriptor population
            // fails the fork rather than starting life with a partial table:
            // an inherited fd that silently went missing is a far worse
            // failure than an `EAGAIN` the caller can see.
            let Some(alias) = src_fd.try_alias(dst_account) else {
                return -1;
            };
            if snapshot.push((i, alias)).is_err() {
                // Allocation failed mid-snapshot: drop the partial clones
                // (decrement and refund — src keeps every `OpenFile` alive).
                return -1;
            }
        }
    }

    // Step 2: build the destination array and claim its slot, both off-lock.
    let Some(descriptors) = new_descriptor_table() else {
        drop(snapshot);
        return -1;
    };
    let Some(dst_slot) = claim_process_slot(dst) else {
        // No free slot: drop the cloned aliases (decrement only).
        drop(snapshot);
        return -1;
    };

    // Step 3: move the cloned snapshot into dst under its lock.
    {
        let mut dst_inner = dst_slot.inner.lock();
        dst_inner.in_use = true;
        dst_inner.descriptors = descriptors;
        for (fd, entry) in snapshot {
            dst_inner.descriptors[fd] = Some(entry);
        }
    }
    0
}

pub fn fileio_close_on_exec(table: FdTable) {
    let Some(slot) = slot_for_table(table) else {
        return;
    };
    // Collect the cloexec entries under the lock, clear their slots, drop
    // the lock, then drop the collected entries (detach-then-drop).
    let closed = {
        let mut inner = slot.inner.lock();
        if !inner.in_use {
            return;
        }
        let mut closed: KVec<FdEntry> = KVec::new();
        for slot in inner.descriptors.iter_mut() {
            let take = matches!(slot, Some(e) if e.cloexec);
            if take && let Some(entry) = slot.take() {
                let _ = closed.push(entry);
            }
        }
        closed
    };
    drop(closed);
}
