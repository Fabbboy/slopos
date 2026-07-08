use core::sync::atomic::Ordering;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::KVec;

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

fn bootstrap_console_fds(inner: &mut FileTableSlotInner, external_ops: &ExternalOpsState) {
    let tty_ops = effective_tty_ops(external_ops);
    let console_tty = tty::default_console_tty();

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

    inner.descriptors[0] = Some(FdEntry {
        open_file: stdin,
        cloexec: false,
    });
    inner.descriptors[1] = Some(FdEntry {
        open_file: stdout,
        cloexec: false,
    });
    inner.descriptors[2] = Some(FdEntry {
        open_file: stderr,
        cloexec: false,
    });
}

/// Take every descriptor out of `inner` under the caller-held lock and
/// return them so the caller can drop them *after* releasing the slot
/// lock (detach-then-drop: the `OpenFile` `Drop` → backing release must
/// not run while the fileio table lock is held).
fn drain_descriptors(inner: &mut FileTableSlotInner) -> KVec<FdEntry> {
    let mut drained: KVec<FdEntry> = KVec::new();
    for slot in inner.descriptors.iter_mut() {
        if let Some(entry) = slot.take() {
            let _ = drained.push(entry);
        }
    }
    drained
}

pub fn fileio_create_table_for_process(process_id: u32) -> i32 {
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    if slot_for_pid(process_id).is_some() {
        return 0;
    }
    // Claim a free slot via CAS so two concurrent creates can't pick
    // the same one.
    for slot in PROCESS_TABLES.iter() {
        if slot
            .process_id
            .compare_exchange(
                INVALID_PROCESS_ID,
                process_id,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .is_ok()
        {
            let mut inner = slot.inner.lock();
            inner.in_use = true;
            for entry in inner.descriptors.iter_mut() {
                *entry = None;
            }
            let external_ops = with_open_files(|state| state.external_ops);
            bootstrap_console_fds(&mut inner, &external_ops);
            return 0;
        }
    }
    -1
}

pub fn fileio_destroy_table_for_process(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    let Some(slot) = slot_for_pid(process_id) else {
        return;
    };
    let drained = {
        let mut inner = slot.inner.lock();
        if !inner.in_use {
            return;
        }
        let drained = drain_descriptors(&mut inner);
        inner.in_use = false;
        drained
    };
    slot.process_id.store(INVALID_PROCESS_ID, Ordering::Release);
    // Drop the table lock above before dropping the entries here so each
    // `OpenFile` teardown runs lock-free (detach-then-drop).
    drop(drained);
}

/// Fork-style clone: every valid descriptor is duplicated, including
/// close-on-exec ones (POSIX fork keeps `FD_CLOEXEC` descriptors; only
/// `exec` strips them).
pub fn fileio_clone_table_for_process(src_process_id: u32, dst_process_id: u32) -> i32 {
    clone_table_inner(src_process_id, dst_process_id, false)
}

/// Spawn-style clone: descriptors marked close-on-exec are skipped.
/// `spawn` is fork+exec in one step, so a `FD_CLOEXEC` descriptor must
/// never appear in the spawned image.
pub fn fileio_clone_table_for_spawn(src_process_id: u32, dst_process_id: u32) -> i32 {
    clone_table_inner(src_process_id, dst_process_id, true)
}

fn clone_table_inner(src_process_id: u32, dst_process_id: u32, skip_cloexec: bool) -> i32 {
    if src_process_id == INVALID_PROCESS_ID || dst_process_id == INVALID_PROCESS_ID {
        return -1;
    }
    if src_process_id == dst_process_id {
        return 0;
    }

    // Step 1: snapshot src descriptors into a heap `KVec` (NOT a stack
    // array — a `[Option<FdEntry>; 32]` on the frame blows the 2 KiB
    // stack gate). Each clone bumps a `KArc<OpenFile>` strong count
    // (safe under the lock: a clone never runs teardown), tagged with its
    // fd number.
    let src_slot = match slot_for_pid(src_process_id) {
        Some(s) => s,
        None => return -1,
    };
    let mut snapshot: KVec<(usize, FdEntry)> = KVec::new();
    {
        let guard = src_slot.inner.lock();
        if !guard.in_use {
            return -1;
        }
        for (i, src_fd) in guard.descriptors.iter().enumerate() {
            let Some(src_fd) = src_fd else { continue };
            if skip_cloexec && src_fd.cloexec {
                continue;
            }
            if snapshot.push((i, src_fd.clone())).is_err() {
                // Allocation failed mid-snapshot: drop the partial clones
                // (decrement only — src keeps every `OpenFile` alive).
                return -1;
            }
        }
    }

    // Step 2: claim a free slot for the destination.
    let Some(dst_slot) = (|| -> Option<&'static FileTableSlot> {
        for slot in PROCESS_TABLES.iter() {
            if slot
                .process_id
                .compare_exchange(
                    INVALID_PROCESS_ID,
                    dst_process_id,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok()
            {
                return Some(slot);
            }
        }
        None
    })() else {
        // No free slot: drop the cloned aliases (decrement only).
        drop(snapshot);
        return -1;
    };

    // Step 3: move the cloned snapshot into dst under its lock.
    {
        let mut dst_inner = dst_slot.inner.lock();
        dst_inner.in_use = true;
        for (fd, entry) in snapshot {
            dst_inner.descriptors[fd] = Some(entry);
        }
    }
    0
}

pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    let Some(slot) = slot_for_pid(process_id) else {
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
