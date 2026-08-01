//! `FileKind::Signalfd` file operations.
//!
//! A signalfd is a pollable view of the owner task's pending signals,
//! filtered to a subscribed mask. Its `handle: usize` resolves
//! [`SignalfdState`](crate::registry::SignalfdState) (owner task id + mask).
//!
//! - `poll_events` returns `POLLIN` when `(owner.signal_pending & mask) != 0`.
//! - `poll_wait` subscribes the calling task to the owner's `SignalPending`
//!   event (published by every signal-raise via `task_signal_raise`), so a
//!   raised signal wakes the poller as an in-band ring/poll event.
//! - `read` drains the lowest pending masked signal and emits one
//!   [`SignalfdSiginfo`]; `write` is meaningless (`-EINVAL`).
//!
//! Paired with the caller blocking those signals (`rt_sigprocmask`), this
//! turns signal delivery from an out-of-band `EINTR` into an in-band
//! completion — `(pending & !blocked)` excludes the masked signals from the
//! harvest's `has_pending_signal()` EINTR check, while `poll_events` (which
//! tests raw `pending`) still reports them.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileBacking, FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::signal::{SignalfdSiginfo, sig_bit};
use slopos_abi::syscall::{POLLIN, POLLNVAL};
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::task::ops::signal_pending_event;
use slopos_sched::task::task_find_by_id;

use crate::registry::{self, SignalfdState};

pub struct SignalfdFileOps;

pub static SIGNALFD_FILE_OPS: SignalfdFileOps = SignalfdFileOps;

/// Owns one signalfd registry entry. The open-file layer holds it as a
/// `KArc<dyn FileBacking>`; dropping the last fd alias removes the entry.
pub(crate) struct SignalfdBacking {
    pub(crate) handle: usize,
}

impl FileBacking for SignalfdBacking {}

impl Drop for SignalfdBacking {
    fn drop(&mut self) {
        registry::remove(self.handle);
    }
}

/// Signals in `state.mask` currently pending for the owner task.
fn pending_masked(state: &SignalfdState) -> u64 {
    task_find_by_id(state.owner_task_id)
        .map(|task| task.signal_pending() & slopos_abi::signal::SIGNAL_MASK & state.mask)
        .unwrap_or(0)
}

impl FileOps for SignalfdFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Signalfd
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        let Some(state) = registry::get(handle) else {
            return Errno::EBADF.as_isize();
        };
        if buf.len() < SignalfdSiginfo::SERIALIZED_LEN {
            return Errno::EINVAL.as_isize();
        }
        let pending = pending_masked(&state);
        if pending == 0 {
            // Readiness is reported by poll_events; a bare read with nothing
            // pending is non-blocking EAGAIN (the reactor polls first).
            return Errno::EAGAIN.as_isize();
        }
        // Drain the lowest pending masked signal.
        let signum = (pending.trailing_zeros() as u8).wrapping_add(1);
        let Some(task) = task_find_by_id(state.owner_task_id) else {
            return Errno::EBADF.as_isize();
        };
        let _ = task.clear_signal_pending(sig_bit(signum));
        let info = SignalfdSiginfo {
            ssi_signo: signum as u32,
            ..Default::default()
        };
        match buf.copy_in(0, &info.to_bytes()) {
            Ok(n) => n as isize,
            Err(e) => e.as_isize(),
        }
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match registry::get(handle) {
            Some(state) => BUS.subscribe_current(signal_pending_event(state.owner_task_id)),
            None => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Some(state) = registry::get(handle) {
            BUS.unsubscribe_current(signal_pending_event(state.owner_task_id));
        }
    }

    fn poll_events(&self, handle: usize, _events: u16) -> u16 {
        match registry::get(handle) {
            Some(state) if pending_masked(&state) != 0 => POLLIN,
            Some(_) => 0,
            None => POLLNVAL,
        }
    }
}
