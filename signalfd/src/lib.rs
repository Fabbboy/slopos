//! `slopos-signalfd` — a `FileKind::Signalfd` that turns pending signals
//! into in-band ring/poll events.
//!
//! `signalfd(mask)` returns an fd that becomes `POLLIN`-ready when a signal
//! in `mask` is pending for the calling task; `read` drains one
//! `SignalfdSiginfo`. Combined with the caller blocking those signals
//! (`rt_sigprocmask`), it lets a reactor harvest signals — including
//! `SIGCHLD` on child exit — as completions instead of being interrupted
//! out-of-band with `EINTR`. This is the structural fix to the
//! signal-interrupts-the-wait footgun.
//!
//! Strictly synchronous, no `unsafe`: built on the existing per-task signal
//! state + event bus. See [`file_ops`] for the readiness contract.

#![no_std]
#![forbid(unsafe_code)]

pub mod file_ops;
pub mod registry;

pub use file_ops::SIGNALFD_FILE_OPS;

use slopos_abi::Errno;

/// Create a signalfd owned by `owner_task_id` (the caller) in `process_id`,
/// watching the signals in `mask`. Returns the new fd (`>= 0`) or a negated
/// errno (`-ENOMEM` if the registry is full, or an fd-table error).
pub fn signalfd_create(process_id: u32, owner_task_id: u32, mask: u64) -> i32 {
    let Some(raw_handle) = registry::insert(registry::SignalfdState {
        owner_task_id,
        mask,
    }) else {
        return Errno::ENOMEM.raw();
    };
    // The backing owns the registry entry: dropping the last fd alias runs its
    // `Drop`, which removes the entry. If the allocation itself fails there is
    // no backing to hand off, so drop the orphaned entry here.
    let backing: slopos_ostd::KArc<dyn slopos_abi::file_ops::FileBacking> =
        match slopos_ostd::KArc::try_new(file_ops::SignalfdBacking { handle: raw_handle }) {
            Ok(b) => b,
            Err(_) => {
                registry::remove(raw_handle);
                return Errno::ENOMEM.raw();
            }
        };
    // On install failure the fd layer drops the backing, which removes the
    // registry entry — no manual cleanup on the error arm.
    slopos_fs::fileio_open_fd_with_ops(
        process_id,
        &file_ops::SIGNALFD_FILE_OPS,
        raw_handle,
        Some(backing),
        slopos_fs::FdFlags::NONE,
    )
}
