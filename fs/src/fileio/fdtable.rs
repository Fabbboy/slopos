use super::open_file_table::{alloc_open_file_entry, incref_open_file, release_open_file};
use super::*;

fn bootstrap_console_fds(
    table: &mut FileTableSlot,
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    external_ops: &ExternalOpsState,
) {
    let tty_ops = effective_tty_ops(external_ops);

    let console_tty = tty::default_console_tty();
    let stdin_flags = FILE_OPEN_READ;
    let stdout_flags = FILE_OPEN_WRITE;

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

    table.descriptors[0] = FdEntry {
        open_file_idx: stdin_idx,
        cloexec: false,
        valid: true,
    };
    table.descriptors[1] = FdEntry {
        open_file_idx: stdout_idx,
        cloexec: false,
        valid: true,
    };
    table.descriptors[2] = FdEntry {
        open_file_idx: stderr_idx,
        cloexec: false,
        valid: true,
    };
}

fn reset_table_entries(
    table: &mut FileTableSlot,
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
) {
    for fd in table.descriptors.iter_mut() {
        if fd.valid {
            release_open_file(open_files, fd.open_file_idx);
        }
        reset_fd_entry(fd);
    }
}

pub fn fileio_create_table_for_process(process_id: u32) -> c_int {
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    with_tables(|kernel, processes, open_files, external_ops| {
        if table_for_pid(kernel, processes, process_id).is_some() {
            return 0;
        }
        let Some(slot) = find_free_table(processes) else {
            return -1;
        };
        reset_table_entries(slot, open_files);
        slot.process_id = process_id;
        slot.in_use = true;
        bootstrap_console_fds(slot, open_files, external_ops);
        0
    })
}

pub fn fileio_destroy_table_for_process(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes, open_files, _| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        if let Some(table) = table_for_pid(kernel, processes, process_id) {
            let table_ptr = table as *mut FileTableSlot;
            if table_ptr == kernel_ptr {
                return;
            }
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            unsafe {
                reset_table_entries(&mut *table_ptr, open_files);
                (*table_ptr).process_id = INVALID_PROCESS_ID;
                (*table_ptr).in_use = false;
            }
            drop(guard);
        }
    });
}

pub fn fileio_clone_table_for_process(src_process_id: u32, dst_process_id: u32) -> c_int {
    if src_process_id == INVALID_PROCESS_ID || dst_process_id == INVALID_PROCESS_ID {
        return -1;
    }
    if src_process_id == dst_process_id {
        return 0;
    }

    with_tables(|kernel, processes, open_files, _| {
        let src_table = match table_for_pid(kernel, processes, src_process_id) {
            Some(t) => t as *const FileTableSlot,
            None => return -1,
        };

        let dst_slot = match find_free_table(processes) {
            Some(s) => s,
            None => return -1,
        };

        reset_table_entries(dst_slot, open_files);
        dst_slot.process_id = dst_process_id;
        dst_slot.in_use = true;

        for (i, src_fd) in unsafe { (*src_table).descriptors.iter().enumerate() } {
            if !src_fd.valid {
                continue;
            }
            if !incref_open_file(open_files, src_fd.open_file_idx) {
                reset_table_entries(dst_slot, open_files);
                dst_slot.process_id = INVALID_PROCESS_ID;
                dst_slot.in_use = false;
                return -1;
            }
            dst_slot.descriptors[i] = *src_fd;
        }

        0
    })
}

pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let table = unsafe { &mut *table_ptr };
        for fd in table.descriptors.iter_mut() {
            if fd.valid && fd.cloexec {
                release_open_file(open_files, fd.open_file_idx);
                reset_fd_entry(fd);
            }
        }
        drop(guard);
    });
}
