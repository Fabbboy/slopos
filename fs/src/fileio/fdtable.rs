use super::*;

pub fn fileio_create_table_for_process(process_id: u32) -> c_int {
    if process_id == INVALID_PROCESS_ID {
        return 0;
    }
    with_tables(|kernel, processes| {
        if table_for_pid(kernel, processes, process_id).is_some() {
            return 0;
        }
        let Some(slot) = find_free_table(processes) else {
            return -1;
        };
        reset_table(slot);
        slot.process_id = process_id;
        slot.in_use = true;
        bootstrap_console_fds(slot);
        0
    })
}

pub fn fileio_destroy_table_for_process(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        if let Some(table) = table_for_pid(kernel, processes, process_id) {
            let table_ptr = table as *mut FileTableSlot;
            if table_ptr == kernel_ptr {
                return;
            }
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            unsafe {
                reset_table(&mut *table_ptr);
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

    with_tables(|kernel, processes| {
        let src_table = match table_for_pid(kernel, processes, src_process_id) {
            Some(t) => t as *const FileTableSlot,
            None => return -1,
        };

        let dst_slot = match find_free_table(processes) {
            Some(s) => s,
            None => return -1,
        };

        reset_table(dst_slot);
        dst_slot.process_id = dst_process_id;
        dst_slot.in_use = true;

        for (i, src_desc) in unsafe { (*src_table).descriptors.iter().enumerate() } {
            if src_desc.valid {
                let Some(copy) = clone_descriptor_for_dup(src_desc) else {
                    reset_table(dst_slot);
                    dst_slot.process_id = INVALID_PROCESS_ID;
                    dst_slot.in_use = false;
                    return -1;
                };
                dst_slot.descriptors[i] = copy;
            }
        }

        0
    })
}

pub fn fileio_close_on_exec(process_id: u32) {
    if process_id == INVALID_PROCESS_ID {
        return;
    }
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let table = unsafe { &mut *table_ptr };
        for desc in table.descriptors.iter_mut() {
            if desc.valid && desc.cloexec {
                reset_descriptor(desc);
            }
        }
        drop(guard);
    });
}
