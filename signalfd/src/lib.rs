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
    let fd =
        slopos_fs::fileio_open_fd_with_ops(process_id, &file_ops::SIGNALFD_FILE_OPS, raw_handle);
    if fd < 0 {
        // fd-table install failed — drop the orphaned registry entry.
        registry::remove(raw_handle);
    }
    fd
}
