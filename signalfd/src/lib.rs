//! `slopos-signalfd` — a `FileKind::Signalfd` that turns pending signals
//! into in-band ring/poll events.
//!
//! `signalfd(mask)` returns an fd that becomes `POLLIN`-ready when a signal in
//! `mask` is pending for the calling task; combined with the caller blocking
//! those signals (`rt_sigprocmask`), a reactor harvests them as completions
//! instead of being interrupted out-of-band with `EINTR`.

#![no_std]
#![forbid(unsafe_code)]

pub mod file_ops;
pub mod registry;

pub use file_ops::SIGNALFD_FILE_OPS;

use slopos_abi::Errno;
use slopos_fs::fileio::FdTable;

/// Create a signalfd owned by `owner_task_id` watching the signals in `mask`.
/// Returns the new fd (`>= 0`) or a negated errno.
pub fn signalfd_create(table: FdTable, owner_task_id: u32, mask: u64) -> i32 {
    let Ok(reservation) =
        slopos_ostd::process::quota::try_charge::<slopos_abi::quota::ObjectRow>(table.account(), 1)
    else {
        return Errno::ENFILE.raw();
    };
    let Some(raw_handle) = registry::insert(registry::SignalfdState {
        owner_task_id,
        // Bits outside the signal range name kernel-private state, which a
        // signalfd must never observe or drain.
        mask: mask & slopos_abi::signal::SIGNAL_MASK,
    }) else {
        return Errno::ENOMEM.raw();
    };
    // A failed allocation leaves no backing to run the entry's `Drop`, so the
    // orphaned entry has to be removed here.
    let backing: slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking> =
        match slopos_ostd::KArc::try_new(file_ops::SignalfdBacking {
            handle: raw_handle,
            object_charge: slopos_ostd::process::quota::Charge::commit(reservation),
        }) {
            Ok(b) => b,
            Err(_) => {
                registry::remove(raw_handle);
                return Errno::ENOMEM.raw();
            }
        };
    // On install failure the fd layer drops the backing, which removes the
    // registry entry; no manual cleanup here.
    slopos_fs::fileio_open_fd_with_ops(
        table,
        &file_ops::SIGNALFD_FILE_OPS,
        raw_handle,
        Some(backing),
        slopos_fs::FdFlags::NONE,
    )
}
