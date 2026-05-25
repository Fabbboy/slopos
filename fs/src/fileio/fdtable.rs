use core::sync::atomic::Ordering;

use slopos_abi::task::INVALID_PROCESS_ID;
use slopos_kernel_services::syscall_services::tty;
use slopos_ostd::handle::HandleTable;

use super::open_file_table::{alloc_open_file_entry, incref_open_file, release_open_file};
use super::*;

fn bootstrap_console_fds(
    inner: &mut FileTableSlotInner,
    open_files: &mut HandleTable<OpenFile>,
    external_ops: &ExternalOpsState,
) {
    let tty_ops = effective_tty_ops(external_ops);

    let console_tty = tty::default_console_tty();
    let stdin_flags = OpenMode::READ;
    let stdout_flags = OpenMode::WRITE;

    let mut opened_refs = 0u8;
    for _ in 0..3 {
        if tty::open_ref(console_tty).is_err() {
            for _ in 0..opened_refs {
                let _ = tty::close_ref(console_tty);
            }
            return;
        }
        opened_refs = opened_refs.saturating_add(1);
    }

    let Some(stdin_idx) =
        alloc_open_file_entry(open_files, tty_ops, console_tty.0 as usize, stdin_flags, 0)
    else {
        let _ = tty::close_ref(console_tty);
        let _ = tty::close_ref(console_tty);
        let _ = tty::close_ref(console_tty);
        return;
    };
    let Some(stdout_idx) =
        alloc_open_file_entry(open_files, tty_ops, console_tty.0 as usize, stdout_flags, 0)
    else {
        release_open_file(open_files, stdin_idx);
        return;
    };
    let Some(stderr_idx) =
        alloc_open_file_entry(open_files, tty_ops, console_tty.0 as usize, stdout_flags, 0)
    else {
        release_open_file(open_files, stdin_idx);
        release_open_file(open_files, stdout_idx);
        return;
    };

    inner.descriptors[0] = FdEntry {
        open_file: stdin_idx,
        cloexec: false,
        valid: true,
    };
    inner.descriptors[1] = FdEntry {
        open_file: stdout_idx,
        cloexec: false,
        valid: true,
    };
    inner.descriptors[2] = FdEntry {
        open_file: stderr_idx,
        cloexec: false,
        valid: true,
    };
}

fn reset_inner_descriptors(inner: &mut FileTableSlotInner, open_files: &mut HandleTable<OpenFile>) {
    for fd in inner.descriptors.iter_mut() {
        if fd.valid {
            release_open_file(open_files, fd.open_file);
        }
        reset_fd_entry(fd);
    }
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
                *entry = FdEntry::new();
            }
            with_open_files(|state| {
                bootstrap_console_fds(&mut inner, &mut state.open_files, &state.external_ops);
            });
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
    let mut inner = slot.inner.lock();
    if !inner.in_use {
        return;
    }
    with_open_files(|state| {
        reset_inner_descriptors(&mut inner, &mut state.open_files);
    });
    inner.in_use = false;
    drop(inner);
    slot.process_id.store(INVALID_PROCESS_ID, Ordering::Release);
}

pub fn fileio_clone_table_for_process(src_process_id: u32, dst_process_id: u32) -> i32 {
    if src_process_id == INVALID_PROCESS_ID || dst_process_id == INVALID_PROCESS_ID {
        return -1;
    }
    if src_process_id == dst_process_id {
        return 0;
    }

    // Step 1: snapshot src descriptors under its own lock, then drop.
    let src_slot = match slot_for_pid(src_process_id) {
        Some(s) => s,
        None => return -1,
    };
    let snapshot: [FdEntry; FILEIO_MAX_OPEN_FILES] = {
        let guard = src_slot.inner.lock();
        if !guard.in_use {
            return -1;
        }
        guard.descriptors
    };

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
        return -1;
    };

    // Step 3: write snapshot into dst, increfing each open file under
    // the open-files lock.
    let mut dst_inner = dst_slot.inner.lock();
    dst_inner.in_use = true;
    for entry in dst_inner.descriptors.iter_mut() {
        *entry = FdEntry::new();
    }
    let result = with_open_files(|state| {
        for (i, src_fd) in snapshot.iter().enumerate() {
            if !src_fd.valid {
                continue;
            }
            if !incref_open_file(&mut state.open_files, src_fd.open_file) {
                // Roll back any increfs done so far.
                for prev in &dst_inner.descriptors[..i] {
                    if prev.valid {
                        release_open_file(&mut state.open_files, prev.open_file);
                    }
                }
                for entry in dst_inner.descriptors.iter_mut() {
                    *entry = FdEntry::new();
                }
                return -1;
            }
            dst_inner.descriptors[i] = *src_fd;
        }
        0
    });
    if result != 0 {
        dst_inner.in_use = false;
        drop(dst_inner);
        dst_slot
            .process_id
            .store(INVALID_PROCESS_ID, Ordering::Release);
    }
    result
}

pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    let Some(slot) = slot_for_pid(process_id) else {
        return;
    };
    let mut inner = slot.inner.lock();
    if !inner.in_use {
        return;
    }
    with_open_files(|state| {
        for fd in inner.descriptors.iter_mut() {
            if fd.valid && fd.cloexec {
                release_open_file(&mut state.open_files, fd.open_file);
                reset_fd_entry(fd);
            }
        }
    });
}
