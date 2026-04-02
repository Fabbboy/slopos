use super::open_file_table::{get_open_file_mut, incref_open_file, release_open_file};
use super::*;

pub fn file_poll_register_pipes(process_id: u32, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        for &(fd, events) in fds {
            let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
                continue;
            };
            let Some(open_file) = get_open_file_mut(open_files, fd_entry.open_file_idx) else {
                continue;
            };
            let Some(ops) = open_file.ops else {
                continue;
            };
            match ops.kind() {
                FileKind::PipeRead if (events & POLLIN) != 0 => {
                    if ops.poll_wait(open_file.handle) {
                        registered += 1;
                    }
                }
                FileKind::PipeWrite if (events & POLLOUT) != 0 => {
                    if ops.poll_wait(open_file.handle) {
                        registered += 1;
                    }
                }
                _ => {}
            }
        }

        drop(guard);
    });
    registered
}

pub fn file_poll_unregister_pipes(process_id: u32, fds: &[(c_int, u16)]) {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        for &(fd, events) in fds {
            let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
                continue;
            };
            let Some(open_file) = get_open_file_mut(open_files, fd_entry.open_file_idx) else {
                continue;
            };
            let Some(ops) = open_file.ops else {
                continue;
            };
            match ops.kind() {
                FileKind::PipeRead if (events & POLLIN) != 0 => ops.poll_unwait(open_file.handle),
                FileKind::PipeWrite if (events & POLLOUT) != 0 => ops.poll_unwait(open_file.handle),
                _ => {}
            }
        }

        drop(guard);
    });
}

pub fn file_poll_register_fd(process_id: u32, fd: c_int, events: u16) -> PollRegInfo {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return PollRegInfo::NONE;
        };
        if !table.in_use {
            return PollRegInfo::NONE;
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let Some(fd_entry) = (unsafe { get_fd_entry(&mut *table_ptr, fd) }) else {
            drop(guard);
            return PollRegInfo::NONE;
        };
        let Some(open_file) = get_open_file_mut(open_files, fd_entry.open_file_idx) else {
            drop(guard);
            return PollRegInfo::NONE;
        };
        let Some(ops) = open_file.ops else {
            drop(guard);
            return PollRegInfo::NONE;
        };

        let registered = match ops.kind() {
            FileKind::Tty => ops.poll_wait(open_file.handle),
            FileKind::Socket if (events & POLLIN) != 0 => ops.poll_wait(open_file.handle),
            _ => false,
        };
        let reg = PollRegInfo {
            open_file_idx: fd_entry.open_file_idx,
            registered,
        };
        drop(guard);
        reg
    })
}

pub fn file_poll_unregister_fd(reg: &PollRegInfo) {
    if !reg.registered {
        return;
    }
    with_tables(|_, _, open_files, _| {
        let Some(open_file) = get_open_file_mut(open_files, reg.open_file_idx) else {
            return;
        };
        if let Some(ops) = open_file.ops {
            ops.poll_unwait(open_file.handle);
        }
    });
}

/// Fused poll: register waiter + check readiness in one call.
///
/// Replaces the separate `file_poll_register_fd` + `file_poll_fd` pattern.
/// Delegates to `FileOps::poll_fused()` which implementations can override
/// to perform registration and readiness check under a single subsystem lock.
pub fn file_poll_fused(
    process_id: u32,
    fd: c_int,
    events: u16,
) -> slopos_abi::file_ops::FusedPollResult {
    use slopos_abi::file_ops::FusedPollResult;
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return FusedPollResult {
                revents: POLLNVAL,
                registered: false,
                open_file_idx: 0,
            };
        };
        if !table.in_use {
            return FusedPollResult {
                revents: POLLNVAL,
                registered: false,
                open_file_idx: 0,
            };
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };

        let ofi = match unsafe { get_fd_entry(&mut *table_ptr, fd) } {
            Some(fd_entry) => fd_entry.open_file_idx,
            None => {
                drop(guard);
                return FusedPollResult {
                    revents: POLLNVAL,
                    registered: false,
                    open_file_idx: 0,
                };
            }
        };

        let result = match get_open_file_mut(open_files, ofi) {
            Some(open_file) => match open_file.ops {
                Some(ops) => {
                    let mut r = ops.poll_fused(open_file.handle, events);
                    r.open_file_idx = ofi as u32;
                    r
                }
                None => FusedPollResult {
                    revents: POLLNVAL,
                    registered: false,
                    open_file_idx: 0,
                },
            },
            None => FusedPollResult {
                revents: POLLNVAL,
                registered: false,
                open_file_idx: 0,
            },
        };

        // Hold an extra reference while the caller is registered on
        // a wait queue, preventing the open file from being freed if
        // a concurrent close drops the last FD-level reference.
        if result.registered {
            incref_open_file(open_files, ofi);
        }

        drop(guard);
        result
    })
}

/// Unregister from a wait queue after fused poll.
///
/// Calls `poll_unwait()` for the given FD. Safe to call even if
/// `poll_fused` did not register (no-op in that case).
pub fn file_poll_unfused(process_id: u32, fd: c_int) {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return;
        };
        if !table.in_use {
            return;
        }
        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        if let Some(open_file) = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
        {
            if let Some(ops) = open_file.ops {
                ops.poll_unwait(open_file.handle);
            }
        }
        drop(guard);
    });
}

/// Unregister from a wait queue using the open-file-table index directly.
///
/// Unlike `file_poll_unfused`, this does NOT re-look up the FD number —
/// it targets the exact open file that was registered on.  This prevents
/// unwaiting from the wrong file if the FD was closed and reassigned
/// between registration and cleanup.
pub fn file_poll_unfused_by_idx(open_file_idx: u32) {
    with_tables(|_, _, open_files, _| {
        let Some(open_file) = get_open_file_mut(open_files, open_file_idx as u16) else {
            return;
        };
        if let Some(ops) = open_file.ops {
            ops.poll_unwait(open_file.handle);
        }
        // Drop the extra reference that file_poll_fused() took to keep the
        // open file alive while we were registered on its wait queue.
        release_open_file(open_files, open_file_idx as u16);
    });
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_tables(|kernel, processes, open_files, _| {
        let Some(table) = table_for_pid(kernel, processes, process_id) else {
            return POLLNVAL;
        };
        if !table.in_use {
            return POLLNVAL;
        }

        let table_ptr: *mut FileTableSlot = table;
        let guard = unsafe { (&(*table_ptr).lock).lock() };
        let revents = (unsafe { get_fd_entry(&mut *table_ptr, fd) })
            .and_then(|fd_entry| get_open_file_mut(open_files, fd_entry.open_file_idx))
            .and_then(|open_file| {
                open_file
                    .ops
                    .map(|ops| ops.poll_events(open_file.handle, events))
            })
            .unwrap_or(POLLNVAL);
        drop(guard);
        revents
    })
}
