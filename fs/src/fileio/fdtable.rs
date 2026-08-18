use core::sync::atomic::Ordering;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::KVec;
use slopos_ostd::handle::Handle;
use slopos_ostd::process::Process;

use super::*;

fn open_console_fd(
    tty_ops: &'static dyn FileOps,
    console_tty: TtyIndex,
    flags: OpenMode,
    backing: KArc<dyn slopos_ostd::process::quota::FileBacking>,
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

    // One console open shared by all three standard fds. All-or-nothing: on any
    // failure the already-built `OpenFile`s and the backing drop at scope exit.
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
/// exactly that long: an `OpenFile` teardown reaches an arbitrary backing
/// release — a socket passed over a socket recurses back into this module — so
/// none of it may run under the table lock, and collecting the entries first
/// would grow a vector under a cli-lock.
fn take_next_descriptor(slot: &'static FileTableSlot) -> Option<FdEntry> {
    let mut inner = slot.inner.lock();
    inner.descriptors.iter_mut().find_map(|entry| entry.take())
}

/// Create a console-bootstrapped fd table for `process`. Returns 0, or -1 if it
/// already has one or its handle no longer resolves — an existing table is a
/// refusal rather than a silent success.
pub fn fileio_create_table_for_process(process: Handle<Process>) -> i32 {
    // Built before the slot is claimed: the array is the table's one
    // allocation, and it must not happen under the slot lock.
    let Some(descriptors) = new_descriptor_table() else {
        return -1;
    };
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
/// [`AccountId::NONE`] for a stale handle — never the root's account, which
/// would bill the kernel for a user process's descriptors.
fn account_of(process: Handle<Process>) -> AccountId {
    slopos_ostd::process::process_for_handle(process)
        .map_or(AccountId::NONE, |process| process.account())
}

/// Claim `process`'s own table slot; the caller then locks it and sets
/// `in_use`. `None` if the slot is already bound.
///
/// Not a search: the slot index is the process's registry slot, so a failed
/// claim means that process already has a table — a caller bug — rather than a
/// full table. The CAS keeps two concurrent creates from both winning.
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

/// The numeric id `handle` names, for the slot's display field.
/// `INVALID_PROCESS_ID` for a stale handle, which makes the claim above fail
/// closed rather than binding a slot to a process that has gone.
fn process_id_of(handle: Handle<Process>) -> u32 {
    slopos_ostd::process::process_for_handle(handle)
        .map_or(INVALID_PROCESS_ID, |process| process.id())
}

/// Create an empty fd table — no console bootstrap: spawn's fd-action
/// allow-list installs every descriptor the child inherits. Returns 0 or -1.
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

pub fn fileio_destroy_table_for_process(process: Handle<Process>) {
    let Some(slot) = slot_for_process(process) else {
        return;
    };
    destroy_table_in_slot(slot);
}

/// Generation-checked, so a handle whose slot has been rebound answers `false`
/// rather than reporting the new occupant's table as its own.
pub fn fileio_table_exists_for_process(process: Handle<Process>) -> bool {
    slot_for_process(process).is_some()
}

/// Release every bound descriptor table. Fixture reset only.
pub fn fileio_reset_all_tables() {
    for slot in PROCESS_TABLES.iter() {
        if slot.process_id.load(Ordering::Acquire) != INVALID_PROCESS_ID {
            destroy_table_in_slot(slot);
        }
    }
}

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
    // Id first, then generation: a reader checks occupancy first, so this order
    // never shows an occupied slot with a cleared generation.
    slot.process_id.store(INVALID_PROCESS_ID, Ordering::Release);
    slot.generation.store(0, Ordering::Release);
    drop(released);
}

/// Fork-style clone: every valid descriptor is duplicated except those marked
/// [`FdFlags::close_on_fork`]. Close-on-exec descriptors *are* duplicated
/// (POSIX fork keeps `FD_CLOEXEC`; only `exec` strips them).
pub fn fileio_clone_table_for_process(src: FdTable, dst: Handle<Process>) -> i32 {
    // Heap `KVec`, not a stack array: a `[Option<FdEntry>; 32]` on the frame
    // blows the 2 KiB stack gate.
    let src_slot = match slot_for_table(src) {
        Some(slot) => slot,
        None => return -1,
    };
    // The child pays for its own descriptor numbers: an entry carrying the
    // parent's token would refund the parent when the *child* closed it.
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
            // A child that cannot afford the parent's descriptors fails the
            // fork rather than starting life with a partial table.
            let Some(alias) = src_fd.try_alias(dst_account) else {
                return -1;
            };
            if snapshot.push((i, alias)).is_err() {
                // Partial clones drop here; src keeps every `OpenFile` alive.
                return -1;
            }
        }
    }

    let Some(descriptors) = new_descriptor_table() else {
        drop(snapshot);
        return -1;
    };
    let Some(dst_slot) = claim_process_slot(dst) else {
        drop(snapshot);
        return -1;
    };

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
