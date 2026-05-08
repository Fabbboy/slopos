use core::ffi::c_int;

use super::open_file_table::{get_open_file_mut, incref_open_file, release_open_file};
use super::*;

pub fn file_poll_register_pipes(process_id: u32, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    let _ = with_pid_slot(process_id, |inner| {
        with_open_files(|state| {
            for &(fd, events) in fds {
                let Some(fd_entry) = get_fd_entry(inner, fd) else {
                    continue;
                };
                let Some(open_file) =
                    get_open_file_mut(&mut state.open_files, fd_entry.open_file_idx)
                else {
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
        });
    });
    registered
}

pub fn file_poll_unregister_pipes(process_id: u32, fds: &[(c_int, u16)]) {
    let _ = with_pid_slot(process_id, |inner| {
        with_open_files(|state| {
            for &(fd, events) in fds {
                let Some(fd_entry) = get_fd_entry(inner, fd) else {
                    continue;
                };
                let Some(open_file) =
                    get_open_file_mut(&mut state.open_files, fd_entry.open_file_idx)
                else {
                    continue;
                };
                let Some(ops) = open_file.ops else {
                    continue;
                };
                match ops.kind() {
                    FileKind::PipeRead if (events & POLLIN) != 0 => {
                        ops.poll_unwait(open_file.handle)
                    }
                    FileKind::PipeWrite if (events & POLLOUT) != 0 => {
                        ops.poll_unwait(open_file.handle)
                    }
                    _ => {}
                }
            }
        });
    });
}

pub fn file_poll_register_fd(process_id: u32, fd: c_int, events: u16) -> PollRegInfo {
    with_pid_slot(process_id, |inner| {
        let Some(fd_entry) = get_fd_entry(inner, fd) else {
            return PollRegInfo::NONE;
        };
        let open_file_idx = fd_entry.open_file_idx;
        with_open_files(|state| {
            let Some(open_file) = get_open_file_mut(&mut state.open_files, open_file_idx) else {
                return PollRegInfo::NONE;
            };
            let Some(ops) = open_file.ops else {
                return PollRegInfo::NONE;
            };
            let registered = match ops.kind() {
                FileKind::Tty => ops.poll_wait(open_file.handle),
                FileKind::Socket if (events & POLLIN) != 0 => ops.poll_wait(open_file.handle),
                _ => false,
            };
            PollRegInfo {
                open_file_idx,
                registered,
            }
        })
    })
    .unwrap_or(PollRegInfo::NONE)
}

pub fn file_poll_unregister_fd(reg: &PollRegInfo) {
    if !reg.registered {
        return;
    }
    with_open_files(|state| {
        let Some(open_file) = get_open_file_mut(&mut state.open_files, reg.open_file_idx) else {
            return;
        };
        if let Some(ops) = open_file.ops {
            ops.poll_unwait(open_file.handle);
        }
    });
}

/// Fused poll: register waiter + check readiness in one call.
pub fn file_poll_fused(
    process_id: u32,
    fd: c_int,
    events: u16,
) -> slopos_abi::file_ops::FusedPollResult {
    use slopos_abi::file_ops::FusedPollResult;
    let invalid = FusedPollResult {
        revents: POLLNVAL,
        registered: false,
        open_file_idx: 0,
    };
    with_pid_slot(process_id, |inner| {
        let ofi = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file_idx,
            None => return invalid,
        };
        with_open_files(|state| {
            let result = match get_open_file_mut(&mut state.open_files, ofi) {
                Some(open_file) => match open_file.ops {
                    Some(ops) => {
                        let mut r = ops.poll_fused(open_file.handle, events);
                        r.open_file_idx = ofi as u32;
                        r
                    }
                    None => invalid,
                },
                None => invalid,
            };

            // Hold an extra reference while the caller is registered on
            // a wait queue, preventing the open file from being freed if
            // a concurrent close drops the last FD-level reference.
            if result.registered {
                incref_open_file(&mut state.open_files, ofi);
            }
            result
        })
    })
    .unwrap_or(invalid)
}

/// Unregister from a wait queue after fused poll.
pub fn file_poll_unfused(process_id: u32, fd: c_int) {
    let _ = with_pid_slot(process_id, |inner| {
        let ofi = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file_idx,
            None => return,
        };
        with_open_files(|state| {
            if let Some(open_file) = get_open_file_mut(&mut state.open_files, ofi) {
                if let Some(ops) = open_file.ops {
                    ops.poll_unwait(open_file.handle);
                }
            }
        });
    });
}

/// Unregister from a wait queue using the open-file-table index directly.
pub fn file_poll_unfused_by_idx(open_file_idx: u32) {
    with_open_files(|state| {
        let Some(open_file) = get_open_file_mut(&mut state.open_files, open_file_idx as u16) else {
            return;
        };
        if let Some(ops) = open_file.ops {
            ops.poll_unwait(open_file.handle);
        }
        // Drop the extra reference that file_poll_fused() took.
        release_open_file(&mut state.open_files, open_file_idx as u16);
    });
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_pid_slot(process_id, |inner| {
        let ofi = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file_idx,
            None => return POLLNVAL,
        };
        with_open_files(|state| {
            get_open_file_mut(&mut state.open_files, ofi)
                .and_then(|open_file| {
                    open_file
                        .ops
                        .map(|ops| ops.poll_events(open_file.handle, events))
                })
                .unwrap_or(POLLNVAL)
        })
    })
    .unwrap_or(POLLNVAL)
}
