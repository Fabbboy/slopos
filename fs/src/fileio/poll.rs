use core::ffi::c_int;

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KBTreeMap, KVec};

use super::open_file_table::{
    get_open_file_mut, incref_open_file, pack_open_file_token, release_open_file,
    unpack_open_file_token,
};
use super::*;

pub fn file_poll_register_pipes(process_id: u32, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    let _ = with_pid_slot(process_id, |inner| {
        with_open_files(|state| {
            for &(fd, events) in fds {
                let Some(fd_entry) = get_fd_entry(inner, fd) else {
                    continue;
                };
                let Some(open_file) = get_open_file_mut(&mut state.open_files, fd_entry.open_file)
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
                let Some(open_file) = get_open_file_mut(&mut state.open_files, fd_entry.open_file)
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
        let open_file_handle = fd_entry.open_file;
        with_open_files(|state| {
            let Some(open_file) = get_open_file_mut(&mut state.open_files, open_file_handle) else {
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
                open_file: open_file_handle,
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
        let Some(open_file) = get_open_file_mut(&mut state.open_files, reg.open_file) else {
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
        open_file_token: 0,
    };
    with_pid_slot(process_id, |inner| {
        let open_file_handle = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file,
            None => return invalid,
        };
        with_open_files(|state| {
            let result = match get_open_file_mut(&mut state.open_files, open_file_handle) {
                Some(open_file) => match open_file.ops {
                    Some(ops) => {
                        let mut r = ops.poll_fused(open_file.handle, events);
                        r.open_file_token = pack_open_file_token(open_file_handle);
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
                incref_open_file(&mut state.open_files, open_file_handle);
            }
            result
        })
    })
    .unwrap_or(invalid)
}

/// Unregister from a wait queue after fused poll.
pub fn file_poll_unfused(process_id: u32, fd: c_int) {
    let _ = with_pid_slot(process_id, |inner| {
        let open_file_handle = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file,
            None => return,
        };
        with_open_files(|state| {
            if let Some(open_file) = get_open_file_mut(&mut state.open_files, open_file_handle) {
                if let Some(ops) = open_file.ops {
                    ops.poll_unwait(open_file.handle);
                }
            }
        });
    });
}

/// Unregister from a wait queue using the open-file token directly.
pub fn file_poll_unfused_by_idx(open_file_token: u64) {
    let open_file_handle = unpack_open_file_token(open_file_token);
    with_open_files(|state| {
        let Some(open_file) = get_open_file_mut(&mut state.open_files, open_file_handle) else {
            return;
        };
        if let Some(ops) = open_file.ops {
            ops.poll_unwait(open_file.handle);
        }
        // Drop the extra reference that file_poll_fused() took.
        release_open_file(&mut state.open_files, open_file_handle);
    });
}

// ── Poll-registration leak guard (task-lifecycle teardown) ──────────────────
//
// `file_poll_fused` takes an extra `incref_open_file` per registered fd to
// keep the OpenFile (and its backend) alive while the caller is parked on a
// wait queue. Normally `syscall_poll` / `syscall_select` release those refs
// via `file_poll_unfused_by_idx` the moment the task wakes. But a task that is
// SIGKILL'd *while blocked* never resumes its syscall, so that release is
// skipped and the extra refs leak — the OpenFile's refcount never reaches
// zero, so `ops.release()` (e.g. `unix_close`, which signals peer EOF) never
// runs and the backend lingers forever (manifesting as a compositor window
// that survives the death of its client).
//
// The fix mirrors `futex_remove_task`: every outstanding poll registration is
// recorded per-task here, and the registered cleanup hook
// (`fileio_poll_cleanup_task`) releases them during task termination — which
// runs *before* the fd table is torn down, so the entry-ref release that
// follows can drive the refcount to zero and fire the backend release. This
// ties poll-reference cleanup to the task's kernel-object lifecycle rather
// than to cooperative syscall return.
static POLL_REGISTRATIONS: SpinLock<KBTreeMap<u32, KVec<u64>>> =
    SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE);

/// Record the set of open-file tokens `task_id` registered for poll, replacing
/// any previously-recorded set. Called immediately before the task blocks.
pub fn file_poll_track_registrations(task_id: u32, tokens: &[u64]) {
    let mut map = POLL_REGISTRATIONS.lock();
    match map.get_mut(&task_id) {
        Some(existing) => {
            existing.clear();
            let _ = existing.extend_from_slice(tokens);
        }
        None => {
            if let Ok(mut v) = KVec::with_capacity(tokens.len()) {
                let _ = v.extend_from_slice(tokens);
                let _ = map.insert(task_id, v);
            }
        }
    }
}

/// Clear `task_id`'s recorded registrations after the task has released them
/// itself (the normal poll/select wake path). The (now-empty) entry is kept so
/// its capacity is reused on the next poll iteration.
pub fn file_poll_clear_registrations(task_id: u32) {
    let mut map = POLL_REGISTRATIONS.lock();
    if let Some(existing) = map.get_mut(&task_id) {
        existing.clear();
    }
}

/// Task-resource cleanup hook: release any poll-registration refs a dying task
/// never got to release. Registered via `register_task_resource_cleanup_hook`
/// at fs init. Safe to call for any task (no-op if it had none).
pub fn fileio_poll_cleanup_task(task_id: u32) {
    // Take the token list out under the tracker lock, then drop the lock
    // before reaching into the open-files table (RESOURCE) to keep the two
    // locks from ever being held simultaneously.
    let tokens = {
        let mut map = POLL_REGISTRATIONS.lock();
        map.remove(&task_id)
    };
    if let Some(tokens) = tokens {
        for &token in tokens.iter() {
            file_poll_unfused_by_idx(token);
        }
    }
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_pid_slot(process_id, |inner| {
        let open_file_handle = match get_fd_entry(inner, fd) {
            Some(fd_entry) => fd_entry.open_file,
            None => return POLLNVAL,
        };
        with_open_files(|state| {
            get_open_file_mut(&mut state.open_files, open_file_handle)
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
