use super::*;

pub fn file_open_for_process(process_id: u32, path: *const c_char, posix_flags: u32) -> c_int {
    let flags = posix_to_internal_flags(posix_flags);
    if path.is_null() || (flags & (FILE_OPEN_READ | FILE_OPEN_WRITE)) == 0 {
        return -1;
    }
    if (flags & FILE_OPEN_APPEND) != 0 && (flags & FILE_OPEN_WRITE) == 0 {
        return -1;
    }

    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };

    if path_bytes == b"/dev/tty" {
        return with_tables(|kernel, processes| {
            let tty_idx = match current_task_controlling_tty() {
                Some(idx) => idx,
                None => return -6,
            };

            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.in_use = true;
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(tty_idx);
            desc.pipe_id = pipe::INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            if tty::open_ref(tty_idx) < 0 {
                reset_descriptor(desc);
                drop(guard);
                return -1;
            }

            drop(guard);
            slot_idx as c_int
        });
    }

    if path_bytes == b"/dev/ptmx" {
        return with_tables(|kernel, processes| {
            let master_idx_raw = tty::alloc_pty();
            if master_idx_raw < 0 {
                return -1;
            }
            let master_idx = TtyIndex(master_idx_raw as u8);

            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.in_use = true;
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags | O_NOCTTY as u32;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(master_idx);
            desc.pipe_id = pipe::INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            if tty::open_ref(master_idx) < 0 {
                reset_descriptor(desc);
                drop(guard);
                return -1;
            }

            drop(guard);
            slot_idx as c_int
        });
    }

    if let Some(slave_idx) = parse_pts_path(path_bytes) {
        return with_tables(|kernel, processes| {
            let kernel_ptr = kernel as *mut FileTableSlot;
            let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
                t as *mut FileTableSlot
            } else if let Some(t) = find_free_table(processes) {
                t as *mut FileTableSlot
            } else {
                kernel_ptr
            };
            let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

            if !table.in_use {
                table.process_id = process_id;
                reset_table(table);
            }

            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };

            let Some(slot_idx) = find_free_slot(table) else {
                drop(guard);
                return -1;
            };

            if tty::open_pty_slave(slave_idx) < 0 {
                drop(guard);
                return -1;
            }

            let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
            desc.inode = 0;
            desc.fs = None;
            desc.flags = flags;
            desc.position = 0;
            desc.valid = true;
            desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
            desc.tty_index = Some(slave_idx);
            desc.pipe_id = pipe::INVALID_PIPE_ID;
            desc.socket_idx = INVALID_SOCKET_IDX;
            desc.pipe_read_end = false;
            desc.pipe_write_end = false;

            drop(guard);
            maybe_acquire_controlling_tty_on_open(slave_idx, flags);
            slot_idx as c_int
        });
    }

    let create = (flags & FILE_OPEN_CREAT) != 0;

    let handle = match vfs_open(path_bytes, create) {
        Ok(h) => h,
        Err(_) => return -1,
    };

    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };
        let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };

        let position = if (flags & FILE_OPEN_APPEND) != 0 {
            match handle.size() {
                Ok(size) => size as usize,
                Err(_) => {
                    drop(guard);
                    return -1;
                }
            }
        } else {
            0
        };

        desc.inode = handle.inode;
        desc.fs = Some(handle.fs);
        desc.flags = flags;
        desc.position = position;
        desc.valid = true;
        desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
        desc.tty_index = None;
        desc.pipe_id = pipe::INVALID_PIPE_ID;
        desc.socket_idx = INVALID_SOCKET_IDX;
        desc.pipe_read_end = false;
        desc.pipe_write_end = false;

        drop(guard);
        slot_idx as c_int
    })
}

pub fn file_read_fd(process_id: u32, fd: c_int, buffer: *mut c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }

    let pipe_info: Option<(u32, bool)> = with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return None;
        };
        if (desc.flags & FILE_OPEN_READ) == 0 {
            drop(guard);
            return None;
        }
        let is_pipe = desc.pipe_id != pipe::INVALID_PIPE_ID;
        let pipe_id = desc.pipe_id;
        let is_read_end = desc.pipe_read_end;
        let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
        drop(guard);
        if is_pipe {
            if !is_read_end {
                return None;
            }
            Some((pipe_id, is_nonblock))
        } else if desc.socket_idx != INVALID_SOCKET_IDX {
            Some((pipe::INVALID_PIPE_ID, is_nonblock))
        } else {
            Some((pipe::INVALID_PIPE_ID, false))
        }
    });

    let Some((pipe_id, is_nonblock)) = pipe_info else {
        return -1;
    };

    if pipe_id == pipe::INVALID_PIPE_ID {
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };

            if let Some(tty_idx) = desc.tty_index {
                let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
                drop(guard);
                return tty::read_cooked(tty_idx, buffer as *mut u8, count, is_nonblock);
            }

            if desc.socket_idx != INVALID_SOCKET_IDX {
                let socket_idx = desc.socket_idx;
                drop(guard);
                return socket::socket_recv(socket_idx, buffer as *mut u8, count) as ssize_t;
            }

            let fs = match desc.fs {
                Some(fs) => fs,
                None => {
                    drop(guard);
                    return -1;
                }
            };

            let buf = unsafe { slice::from_raw_parts_mut(buffer as *mut u8, count) };
            let rc = fs.read(desc.inode, desc.position as u64, buf);
            if let Ok(read_len) = rc {
                desc.position = desc.position.saturating_add(read_len);
                drop(guard);
                return read_len as ssize_t;
            }
            drop(guard);
            -1
        });
    }

    let mut local = [0u8; 512];
    let mut total = 0usize;
    let mut remaining = count;

    loop {
        let mut need_block = false;

        {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
                return if total > 0 { total as ssize_t } else { -1 };
            };

            while remaining > 0 && slot.len > 0 {
                let chunk = remaining.min(local.len());
                let copied = slot.read_into(&mut local[..chunk]);
                if copied == 0 {
                    break;
                }
                unsafe {
                    core::ptr::copy_nonoverlapping(
                        local.as_ptr(),
                        (buffer as *mut u8).add(total),
                        copied,
                    );
                }
                total += copied;
                remaining -= copied;

                pipe::writer_wq(pipe_id).wake_one();
            }

            if total > 0 {
                return total as ssize_t;
            }

            if slot.writers == 0 {
                return 0;
            }

            if is_nonblock {
                return -11;
            }

            if scheduler_is_enabled() != 0 {
                need_block = true;
            }
        }

        if need_block {
            pipe::reader_wq(pipe_id).enqueue_current();
            prepare_to_wait();
            block_current_task();
            finish_wait();
            pipe::reader_wq(pipe_id).remove_current();
            continue;
        }

        return -1;
    }
}

pub fn file_write_fd(process_id: u32, fd: c_int, buffer: *const c_char, count: usize) -> ssize_t {
    if buffer.is_null() || count == 0 {
        return 0;
    }

    let pipe_info: Option<(u32, bool)> = with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return None;
        };
        if (desc.flags & FILE_OPEN_WRITE) == 0 {
            drop(guard);
            return None;
        }
        let is_pipe = desc.pipe_id != pipe::INVALID_PIPE_ID;
        let pipe_id = desc.pipe_id;
        let is_write_end = desc.pipe_write_end;
        let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
        drop(guard);
        if is_pipe {
            if !is_write_end {
                return None;
            }
            Some((pipe_id, is_nonblock))
        } else if desc.socket_idx != INVALID_SOCKET_IDX {
            Some((pipe::INVALID_PIPE_ID, is_nonblock))
        } else {
            Some((pipe::INVALID_PIPE_ID, false))
        }
    });

    let Some((pipe_id, is_nonblock)) = pipe_info else {
        return -1;
    };

    if pipe_id == pipe::INVALID_PIPE_ID {
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };

            if let Some(tty_idx) = desc.tty_index {
                let is_nonblock = (desc.flags & O_NONBLOCK as u32) != 0;
                drop(guard);
                let result = tty::write_bytes(tty_idx, buffer as *const u8, count, is_nonblock);
                return result as ssize_t;
            }

            if desc.socket_idx != INVALID_SOCKET_IDX {
                let socket_idx = desc.socket_idx;
                drop(guard);
                return socket::socket_send(socket_idx, buffer as *const u8, count) as ssize_t;
            }

            let fs = match desc.fs {
                Some(fs) => fs,
                None => {
                    drop(guard);
                    return -1;
                }
            };

            let buf = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
            let rc = fs.write(desc.inode, desc.position as u64, buf);
            if let Ok(written) = rc {
                desc.position = desc.position.saturating_add(written);
                drop(guard);
                return written as ssize_t;
            }
            drop(guard);
            -1
        });
    }

    let input = unsafe { slice::from_raw_parts(buffer as *const u8, count) };
    let mut total = 0usize;

    loop {
        let mut need_block = false;

        {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
                return if total > 0 { total as ssize_t } else { -1 };
            };

            if slot.readers == 0 {
                return if total > 0 { total as ssize_t } else { -1 };
            }

            if total < count && slot.len < pipe::PIPE_BUFFER_SIZE {
                let written = slot.write_from(&input[total..]);
                if written > 0 {
                    total += written;
                    pipe::reader_wq(pipe_id).wake_one();
                }
            }

            if total >= count {
                return total as ssize_t;
            }

            if total > 0 {
                return total as ssize_t;
            }

            if is_nonblock {
                return -11;
            }

            if scheduler_is_enabled() != 0 {
                need_block = true;
            }
        }

        if need_block {
            pipe::writer_wq(pipe_id).enqueue_current();
            prepare_to_wait();
            block_current_task();
            finish_wait();
            pipe::writer_wq(pipe_id).remove_current();
            continue;
        }

        return -1;
    }
}

pub fn file_close_fd(process_id: u32, fd: c_int) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };
        if desc.socket_idx != INVALID_SOCKET_IDX {
            let _ = socket::socket_close(desc.socket_idx);
            desc.socket_idx = INVALID_SOCKET_IDX;
        }
        reset_descriptor(desc);
        drop(guard);
        0
    })
}

pub fn file_seek_fd(process_id: u32, fd: c_int, offset: i64, whence: u32) -> i64 {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        if desc.tty_index.is_some() {
            drop(guard);
            return -1;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        let size = match fs.stat(desc.inode) {
            Ok(stat) => stat.size as i64,
            Err(_) => {
                drop(guard);
                return -1;
            }
        };

        let new_pos = match whence as u64 {
            SEEK_SET => offset,
            SEEK_CUR => (desc.position as i64).saturating_add(offset),
            SEEK_END => size.saturating_add(offset),
            _ => {
                drop(guard);
                return -1;
            }
        };

        if new_pos < 0 {
            drop(guard);
            return -1;
        }

        desc.position = new_pos as usize;
        drop(guard);
        new_pos
    })
}

pub fn file_get_size_fd(process_id: u32, fd: c_int) -> usize {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return usize::MAX;
        };
        if !table.in_use {
            return usize::MAX;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let desc = unsafe { get_descriptor(&mut *table_ptr, fd) };
        let size = if let Some(desc) = desc {
            if let Some(fs) = desc.fs {
                match fs.stat(desc.inode) {
                    Ok(stat) => stat.size as usize,
                    Err(_) => usize::MAX,
                }
            } else {
                usize::MAX
            }
        } else {
            usize::MAX
        };
        drop(guard);
        size
    })
}

pub fn file_exists_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return 0;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return 0,
    };
    let rc = vfs_stat(path_bytes);
    if let Ok((kind, _)) = rc {
        return if kind == FS_TYPE_FILE { 1 } else { 0 };
    }
    0
}

pub fn file_unlink_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_unlink(path_bytes).is_ok() {
        0
    } else {
        -1
    }
}

pub fn file_mkdir_path(path: *const c_char) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if vfs_mkdir(path_bytes).is_ok() { 0 } else { -1 }
}

pub fn file_stat_path(path: *const c_char, out_type: &mut u8, out_size: &mut u32) -> c_int {
    if path.is_null() {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    if let Ok((kind, size)) = vfs_stat(path_bytes) {
        *out_type = kind;
        *out_size = size;
        return 0;
    }
    -1
}

pub fn file_list_path(
    path: *const c_char,
    entries: *mut UserFsEntry,
    max: u32,
    out_count: &mut u32,
) -> c_int {
    if path.is_null() || entries.is_null() || max == 0 {
        return -1;
    }
    let path_bytes = match unsafe { path_bytes(path) } {
        Some(p) => p,
        None => return -1,
    };
    let cap = max as usize;
    let out_slice = unsafe { slice::from_raw_parts_mut(entries, cap) };
    match vfs_list(path_bytes, out_slice) {
        Ok(count) => {
            *out_count = count as u32;
            0
        }
        Err(_) => -1,
    }
}

pub fn file_is_console_fd(process_id: u32, fd: c_int) -> bool {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return false;
        };
        if !table.in_use {
            return false;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let is_console = unsafe { get_descriptor(&mut *table_ptr, fd) }
            .map(|d| d.tty_index.is_some())
            .unwrap_or(false);
        drop(guard);
        is_console
    })
}

pub fn file_get_tty_index(process_id: u32, fd: c_int) -> Option<TtyIndex> {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return None;
        };
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let tty_idx = unsafe { get_descriptor(&mut *table_ptr, fd) }.and_then(|d| d.tty_index);
        drop(guard);
        tty_idx
    })
}

pub fn file_open_tty_fd(process_id: u32, tty_idx: TtyIndex, flags: u32) -> c_int {
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };
        let table: &mut FileTableSlot = unsafe { &mut *table_ptr };

        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        let desc = unsafe { &mut (*table_ptr).descriptors[slot_idx] };
        desc.inode = 0;
        desc.fs = None;
        desc.flags = flags;
        desc.position = 0;
        desc.valid = true;
        desc.cloexec = (flags & O_CLOEXEC as u32) != 0;
        desc.tty_index = Some(tty_idx);
        desc.pipe_id = pipe::INVALID_PIPE_ID;
        desc.socket_idx = INVALID_SOCKET_IDX;
        desc.pipe_read_end = false;
        desc.pipe_write_end = false;

        drop(guard);
        maybe_acquire_controlling_tty_on_open(tty_idx, flags);
        slot_idx as c_int
    })
}

pub fn file_pipe_create(
    process_id: u32,
    flags: u32,
    out_read_fd: &mut c_int,
    out_write_fd: &mut c_int,
) -> c_int {
    if flags & !(O_NONBLOCK as u32 | O_CLOEXEC as u32) != 0 {
        return -1;
    }

    let pipe_id = match pipe::alloc_slot() {
        Some(id) => id,
        None => return -1,
    };

    let rc = with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };

        let table = unsafe { &mut *table_ptr };
        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(read_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };
        table.descriptors[read_idx].valid = true;

        let Some(write_idx) = find_free_slot(table) else {
            reset_descriptor(&mut table.descriptors[read_idx]);
            drop(guard);
            return -1;
        };

        let nonblock = (flags & O_NONBLOCK as u32) != 0;
        let cloexec = (flags & O_CLOEXEC as u32) != 0;

        table.descriptors[read_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_READ | if nonblock { O_NONBLOCK as u32 } else { 0 },
            valid: true,
            cloexec,
            tty_index: None,
            pipe_id,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: true,
            pipe_write_end: false,
        };

        table.descriptors[write_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_WRITE | if nonblock { O_NONBLOCK as u32 } else { 0 },
            valid: true,
            cloexec,
            tty_index: None,
            pipe_id,
            socket_idx: INVALID_SOCKET_IDX,
            pipe_read_end: false,
            pipe_write_end: true,
        };

        {
            let mut pipe_state = pipe::PIPE_STATE.lock();
            let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) else {
                reset_descriptor(&mut table.descriptors[read_idx]);
                reset_descriptor(&mut table.descriptors[write_idx]);
                drop(guard);
                return -1;
            };
            slot.readers = 1;
            slot.writers = 1;
        }

        *out_read_fd = read_idx as c_int;
        *out_write_fd = write_idx as c_int;
        drop(guard);
        0
    });

    if rc != 0 {
        let mut pipe_state = pipe::PIPE_STATE.lock();
        if let Some(slot) = pipe::slot_mut(&mut pipe_state, pipe_id) {
            *slot = pipe::PipeSlot::new();
        }
    }

    rc
}

pub fn file_dup_fd(process_id: u32, old_fd: c_int) -> c_int {
    file_dup_fd_min(process_id, old_fd, 0)
}

fn file_dup_fd_min(process_id: u32, old_fd: c_int, min_fd: usize) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        let Some(new_idx) = find_free_slot_from(table, min_fd) else {
            drop(guard);
            return -1;
        };

        table.descriptors[new_idx] = copy;
        table.descriptors[new_idx].cloexec = false;
        drop(guard);
        new_idx as c_int
    })
}

pub fn file_dup2_fd(process_id: u32, old_fd: c_int, new_fd: c_int) -> c_int {
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }
    if old_fd == new_fd {
        return with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let valid = unsafe { get_descriptor(&mut *table_ptr, old_fd) }.is_some();
            drop(guard);
            if valid { new_fd } else { -1 }
        });
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        table.descriptors[new_fd as usize].cloexec = false;
        drop(guard);
        new_fd
    })
}

pub fn file_dup3_fd(process_id: u32, old_fd: c_int, new_fd: c_int, flags: u32) -> c_int {
    if old_fd == new_fd {
        return -1;
    }
    if new_fd < 0 || new_fd as usize >= FILEIO_MAX_OPEN_FILES {
        return -1;
    }

    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let src = unsafe { get_descriptor(&mut *table_ptr, old_fd) };
        let Some(src) = src else {
            drop(guard);
            return -1;
        };
        let Some(copy) = clone_descriptor_for_dup(src) else {
            drop(guard);
            return -1;
        };

        let table = unsafe { &mut *table_ptr };
        if table.descriptors[new_fd as usize].valid {
            reset_descriptor(&mut table.descriptors[new_fd as usize]);
        }
        table.descriptors[new_fd as usize] = copy;
        table.descriptors[new_fd as usize].cloexec = (flags & FD_CLOEXEC as u32) != 0;
        drop(guard);
        new_fd
    })
}

pub fn file_fcntl_fd(process_id: u32, fd: c_int, cmd: u64, arg: u64) -> i64 {
    match cmd {
        F_DUPFD => file_dup_fd_min(process_id, fd, arg as usize) as i64,
        F_GETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = if desc.cloexec { FD_CLOEXEC as i64 } else { 0 };
            drop(guard);
            val
        }),
        F_SETFD => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            desc.cloexec = (arg & FD_CLOEXEC) != 0;
            drop(guard);
            0
        }),
        F_GETFL => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let val = desc.flags as i64;
            drop(guard);
            val
        }),
        F_SETFL => with_tables(|kernel, processes| {
            let Some(table) = table_for_pid(kernel, processes, process_id) else {
                return -1i64;
            };
            if !table.in_use {
                return -1;
            }
            let table_ptr: *mut FileTableSlot = table;
            let guard = unsafe { (&(*table_ptr).lock).lock() };
            let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
                drop(guard);
                return -1;
            };
            let mode_bits = desc.flags & (FILE_OPEN_READ | FILE_OPEN_WRITE);
            let sticky_flags = desc.flags & (O_NOCTTY as u32);
            let mut next_flags = mode_bits | sticky_flags | (arg as u32 & FILE_OPEN_APPEND);
            if (arg & O_NONBLOCK) != 0 {
                next_flags |= O_NONBLOCK as u32;
            }
            desc.flags = next_flags;
            if desc.socket_idx != INVALID_SOCKET_IDX {
                let _ = socket::socket_set_nonblocking(desc.socket_idx, (arg & O_NONBLOCK) != 0);
            }
            drop(guard);
            0
        }),
        _ => -1,
    }
}

pub fn file_fstat_fd(process_id: u32, fd: c_int, out_stat: &mut UserFsStat) -> c_int {
    with_tables(|kernel, processes| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return -1;
        };
        if !table.in_use {
            return -1;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(desc) = (unsafe { get_descriptor(&mut *table_ptr, fd) }) else {
            drop(guard);
            return -1;
        };

        if desc.tty_index.is_some() {
            out_stat.type_ = slopos_abi::fs::FS_TYPE_CHARDEV;
            out_stat.size = 0;
            drop(guard);
            return 0;
        }

        let fs = match desc.fs {
            Some(fs) => fs,
            None => {
                drop(guard);
                return -1;
            }
        };

        match fs.stat(desc.inode) {
            Ok(stat) => {
                out_stat.type_ = stat.file_type as u8;
                out_stat.size = stat.size as u32;
                drop(guard);
                0
            }
            Err(_) => {
                drop(guard);
                -1
            }
        }
    })
}

pub fn fileio_open_socket_fd(process_id: u32, socket_idx: u32) -> i32 {
    with_tables(|kernel, processes| {
        let kernel_ptr = kernel as *mut FileTableSlot;
        let table_ptr = if let Some(t) = table_for_pid(kernel, processes, process_id) {
            t as *mut FileTableSlot
        } else if let Some(t) = find_free_table(processes) {
            t as *mut FileTableSlot
        } else {
            kernel_ptr
        };

        let table = unsafe { &mut *table_ptr };
        if !table.in_use {
            table.in_use = true;
            table.process_id = process_id;
            reset_table(table);
        }

        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(slot_idx) = find_free_slot(table) else {
            drop(guard);
            return -1;
        };

        table.descriptors[slot_idx] = FileDescriptor {
            inode: 0,
            fs: None,
            position: 0,
            flags: FILE_OPEN_READ | FILE_OPEN_WRITE,
            valid: true,
            cloexec: false,
            tty_index: None,
            pipe_id: pipe::INVALID_PIPE_ID,
            socket_idx,
            pipe_read_end: false,
            pipe_write_end: false,
        };
        let _ = socket::socket_set_nonblocking(socket_idx, false);
        drop(guard);
        slot_idx as i32
    })
}

pub fn fileio_get_socket_idx(process_id: u32, fd: i32) -> Option<u32> {
    with_tables(|kernel, processes| {
        let table = table_for_pid(kernel, processes, process_id)?;
        if !table.in_use {
            return None;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let out = unsafe { get_descriptor(&mut *table_ptr, fd) }
            .map(|d| d.socket_idx)
            .filter(|idx| *idx != INVALID_SOCKET_IDX);
        drop(guard);
        out
    })
}
