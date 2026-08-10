use core::ffi::c_int;
use core::sync::atomic::{AtomicU64, Ordering};
use slopos_ostd::lock_class;

use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_ostd::{KArc, KBTreeMap, KWeak};

use super::*;

pub fn file_poll_register_pipes(table: FdTable, fds: &[(c_int, u16)]) -> usize {
    let mut registered = 0usize;
    let _ = with_table_slot(table, |inner| {
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

pub fn file_poll_unregister_pipes(table: FdTable, fds: &[(c_int, u16)]) {
    let _ = with_table_slot(table, |inner| {
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

pub fn file_poll_register_fd(table: FdTable, fd: c_int, events: u16) -> PollRegInfo {
    with_table_slot(table, |inner| {
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
    entries: SpinLock::new(
        KBTreeMap::new(),
        lock_class!("POLL_REG_TABLE", LOCK_LEVEL_RESOURCE),
    ),
};

/// Live registrations the table will hold at once.
///
/// An entry leaves only through [`poll_reg_take`], so a caller that never hands
/// its token back holds one for the rest of the boot. A bound turns that into a
/// refusal the caller can act on rather than growth nothing reclaims.
const POLL_REG_MAX: usize = 4096;

/// Record a weak handle to `open_file` and return its opaque token, or 0 when
/// the table is full.
fn poll_reg_insert(open_file: &KArc<OpenFile>) -> u64 {
    let id = POLL_REG_TABLE.next_id.fetch_add(1, Ordering::Relaxed);
    let mut entries = POLL_REG_TABLE.entries.lock();
    if entries.len() >= POLL_REG_MAX {
        return 0;
    }
    entries.insert(id, KArc::downgrade(open_file));
    id
}

/// Hand back a token for a registration that was just made, undoing the
/// registration when the table had no room for it.
///
/// Reporting `registered` without a token would leave the caller parked on a
/// wait queue with no way to name the entry that takes it off again.
fn poll_reg_token_or_unwait(
    result: &mut slopos_abi::file_ops::FusedPollResult,
    open_file: &KArc<OpenFile>,
) {
    if !result.registered {
        return;
    }
    result.open_file_token = poll_reg_insert(open_file);
    if result.open_file_token == 0 {
        open_file.ops.poll_unwait(open_file.handle);
        result.registered = false;
    }
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
    table: FdTable,
    fd: c_int,
    events: u16,
) -> slopos_abi::file_ops::FusedPollResult {
    use slopos_abi::file_ops::FusedPollResult;
    let invalid = FusedPollResult {
        revents: POLLNVAL,
        registered: false,
        open_file_token: 0,
    };
    with_table_slot(table, |inner| {
        let Some(open_file) = snapshot_open_file(inner, fd) else {
            return invalid;
        };
        let mut r = open_file.ops.poll_fused(open_file.handle, events);
        // Hand the caller an opaque registration token backed by a weak
        // reference. The weak does not keep the open file alive; if the
        // fd is closed before unregister, the token upgrades to None.
        poll_reg_token_or_unwait(&mut r, &open_file);
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
    poll_reg_token_or_unwait(&mut r, &file.open_file);
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

pub fn file_poll_fd(table: FdTable, fd: c_int, events: u16) -> u16 {
    with_table_slot(table, |inner| {
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
