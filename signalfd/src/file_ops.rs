//! `FileKind::Signalfd` file operations: a pollable view of the owner task's
//! pending signals, filtered to a subscribed mask.
//!
//! Paired with the caller blocking those signals (`rt_sigprocmask`), delivery
//! becomes in-band: `(pending & !blocked)` excludes them from the harvest's
//! EINTR check, while `poll_events` tests raw `pending` and still reports them.

use slopos_abi::Errno;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::quota::ObjectRow;
use slopos_abi::signal::{SignalfdSiginfo, sig_bit};
use slopos_abi::syscall::{POLLIN, POLLNVAL};
use slopos_ostd::process::quota::{Charge, FileBacking};
use slopos_ostd::sync::PollWaiterRef;
use slopos_ostd::sync::event_bus::BUS;
use slopos_ostd::task::ops::signal_pending_event;
use slopos_sched::task::task_find_by_id;

use crate::registry::{self, SignalfdState};

pub struct SignalfdFileOps;

pub static SIGNALFD_FILE_OPS: SignalfdFileOps = SignalfdFileOps;

/// Owns one signalfd registry entry; dropping the last fd alias removes it.
#[derive(slopos_ostd::Charged)]
pub(crate) struct SignalfdBacking {
    pub(crate) handle: usize,
    pub(crate) object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(SignalfdBacking);

impl FileBacking for SignalfdBacking {}

impl Drop for SignalfdBacking {
    fn drop(&mut self) {
        registry::remove(self.handle);
    }
}

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
            // Never blocks: readiness comes from poll_events, so an empty read
            // is EAGAIN rather than a sleep.
            return Errno::EAGAIN.as_isize();
        }
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
            Some(state) => PollWaiterRef::current().is_some_and(|w| {
                BUS.subscribe_current(w, signal_pending_event(state.owner_task_id))
            }),
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
