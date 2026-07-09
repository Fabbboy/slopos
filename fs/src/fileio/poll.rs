use core::ffi::c_int;
use core::sync::atomic::{AtomicU64, Ordering};

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KBTreeMap, KVec, KWeak};

use super::*;

pub fn file_poll_register_pipes(process_id: u32, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    let _ = with_pid_slot(process_id, |inner| {
        for &(fd, events) in fds {
            let Some(open_file) = snapshot_open_file(inner, fd) else {
                continue;
            };
            let ops = open_file.ops;
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
    registered
}

pub fn file_poll_unregister_pipes(process_id: u32, fds: &[(c_int, u16)]) {
    let _ = with_pid_slot(process_id, |inner| {
        for &(fd, events) in fds {
            let Some(open_file) = snapshot_open_file(inner, fd) else {
                continue;
            };
            let ops = open_file.ops;
            match ops.kind() {
                FileKind::PipeRead if (events & POLLIN) != 0 => ops.poll_unwait(open_file.handle),
                FileKind::PipeWrite if (events & POLLOUT) != 0 => ops.poll_unwait(open_file.handle),
                _ => {}
            }
        }
    });
}

/// Clone the `KArc<OpenFile>` behind `fd` (or `None` if the fd is bad).
/// A helper for the poll paths that want the open file without going
/// through the full `FdSnapshot`.
fn snapshot_open_file(inner: &FileTableSlotInner, fd: c_int) -> Option<KArc<OpenFile>> {
    Some(get_fd_entry(inner, fd)?.open_file.clone())
}

pub fn file_poll_register_fd(process_id: u32, fd: c_int, events: u16) -> PollRegInfo {
    with_pid_slot(process_id, |inner| {
        let Some(open_file) = snapshot_open_file(inner, fd) else {
            return PollRegInfo::none();
        };
        let ops = open_file.ops;
        let registered = match ops.kind() {
            FileKind::Tty => ops.poll_wait(open_file.handle),
            FileKind::Socket if (events & POLLIN) != 0 => ops.poll_wait(open_file.handle),
            _ => false,
        };
        PollRegInfo {
            open_file: KArc::downgrade(&open_file),
            registered,
        }
    })
    .unwrap_or_else(PollRegInfo::none)
}

pub fn file_poll_unregister_fd(reg: &PollRegInfo) {
    if !reg.registered {
        return;
    }
    // Upgrade-or-skip: if the open file is gone, the backing was torn
    // down (which already cleared its wait queue), so there is nothing to
    // unregister and no chance of touching a reused slot.
    if let Some(open_file) = reg.open_file.upgrade() {
        open_file.ops.poll_unwait(open_file.handle);
    }
}

// ── Poll-registration table (KWeak-keyed opaque tokens) ─────────────────────
//
// `file_poll_fused` is the kernel-internal poll ABI used by `poll`/`select`
// and the SlopRing harvest. The ABI carries an opaque `u64` token
// (`FusedPollResult::open_file_token`) the caller later hands back to
// `file_poll_unfused_by_token` to unregister from the wait queue. The token is
// now an opaque *registration id* resolving (in this table) to a
// `KWeak<OpenFile>`: the registration NEVER keeps the open file alive, and a
// dead registration upgrades to `None` so it can never touch a reused slot or
// double-release a backing object (the single-owner invariant — there is no
// extra strong reference for a stale token to drop).

struct PollRegTable {
    next_id: AtomicU64,
    entries: SpinLock<KBTreeMap<u64, KWeak<OpenFile>>>,
}

static POLL_REG_TABLE: PollRegTable = PollRegTable {
    // Start at 1 so 0 stays a never-registered sentinel (matching the
    // `open_file_token: 0` default the backends return).
    next_id: AtomicU64::new(1),
    entries: SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE),
};

/// Record a weak handle to `open_file` and return its opaque token.
fn poll_reg_insert(open_file: &KArc<OpenFile>) -> u64 {
    let id = POLL_REG_TABLE.next_id.fetch_add(1, Ordering::Relaxed);
    let mut entries = POLL_REG_TABLE.entries.lock();
    let _ = entries.insert(id, KArc::downgrade(open_file));
    id
}

/// Remove a registration by token, returning the weak handle it held (if
/// the token was live).
fn poll_reg_take(token: u64) -> Option<KWeak<OpenFile>> {
    if token == 0 {
        return None;
    }
    let mut entries = POLL_REG_TABLE.entries.lock();
    entries.remove(&token)
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
        let Some(open_file) = snapshot_open_file(inner, fd) else {
            return invalid;
        };
        let mut r = open_file.ops.poll_fused(open_file.handle, events);
        // Hand the caller an opaque registration token backed by a weak
        // reference. The weak does not keep the open file alive; if the
        // fd is closed before unregister, the token upgrades to None.
        if r.registered {
            r.open_file_token = poll_reg_insert(&open_file);
        }
        r
    })
    .unwrap_or(invalid)
}

/// Fused poll against a held [`FileRef`] — the reference analog of
/// [`file_poll_fused`], used by the ring harvest to register the calling
/// task on an in-flight op's wait queue by open-file identity rather than
/// by an fd number that may have been closed or reused.
pub fn file_poll_fused_ref(file: &FileRef, events: u16) -> slopos_abi::file_ops::FusedPollResult {
    let mut r = file.open_file.ops.poll_fused(file.open_file.handle, events);
    if r.registered {
        r.open_file_token = poll_reg_insert(&file.open_file);
    }
    r
}

/// Unregister from a wait queue using the opaque registration token from
/// [`file_poll_fused`]. Upgrade-or-skip: a token whose open file was
/// already dropped is silently discarded — the backing teardown cleared
/// its own wait queue, and the weak can never resurrect a reused slot.
pub fn file_poll_unfused_by_token(open_file_token: u64) {
    let Some(weak) = poll_reg_take(open_file_token) else {
        return;
    };
    if let Some(open_file) = weak.upgrade() {
        open_file.ops.poll_unwait(open_file.handle);
    }
}

// ── Poll-registration leak guard (task-lifecycle teardown) ──────────────────
//
// `poll`/`select`/`ring` register fds on wait queues via `file_poll_fused` and
// normally unregister via `file_poll_unfused_by_token` the moment the task wakes.
// A task that is SIGKILL'd *while blocked* never resumes its syscall, so that
// unregister is skipped and a stale wait-queue entry would linger. Every
// outstanding registration token is recorded per-task here; the registered
// cleanup hook (`fileio_poll_cleanup_task`) drains them during task
// termination. Because the token only carries a `KWeak`, this is purely
// wait-queue hygiene — it can never drive a refcount underflow or a premature
// backing release.
static POLL_REGISTRATIONS: SpinLock<KBTreeMap<u32, KVec<u64>>> =
    SpinLock::new(KBTreeMap::new(), LOCK_LEVEL_RESOURCE);

/// Record the set of registration tokens `task_id` holds for poll,
/// replacing any previously-recorded set. Called immediately before the
/// task blocks.
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

/// Clear `task_id`'s recorded registrations after the task has released
/// them itself (the normal poll/select wake path). The (now-empty) entry
/// is kept so its capacity is reused on the next poll iteration.
pub fn file_poll_clear_registrations(task_id: u32) {
    let mut map = POLL_REGISTRATIONS.lock();
    if let Some(existing) = map.get_mut(&task_id) {
        existing.clear();
    }
}

/// Task-resource cleanup hook: unregister any poll tokens a dying task
/// never got to release. Registered via `register_task_resource_cleanup_hook`
/// at fs init. Safe to call for any task (no-op if it had none).
pub fn fileio_poll_cleanup_task(task_id: u32) {
    // Take the token list out under the tracker lock, then drop the lock
    // before reaching into the registration table (RESOURCE) to keep the
    // two locks from ever being held simultaneously.
    let tokens = {
        let mut map = POLL_REGISTRATIONS.lock();
        map.remove(&task_id)
    };
    if let Some(tokens) = tokens {
        for &token in tokens.iter() {
            file_poll_unfused_by_token(token);
        }
    }
}

pub fn file_poll_fd(process_id: u32, fd: c_int, events: u16) -> u16 {
    with_pid_slot(process_id, |inner| {
        let Some(open_file) = snapshot_open_file(inner, fd) else {
            return POLLNVAL;
        };
        open_file.ops.poll_events(open_file.handle, events)
    })
    .unwrap_or(POLLNVAL)
}

/// Level readiness of a held [`FileRef`] — the reference analog of
/// [`file_poll_fd`], for the ring's multishot poll re-arm.
pub fn file_poll_ref(file: &FileRef, events: u16) -> u16 {
    file.open_file
        .ops
        .poll_events(file.open_file.handle, events)
}
