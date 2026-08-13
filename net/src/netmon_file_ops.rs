//! [`FileKind::Netmon`] file operations.
//!
//! A monitor fd is a pollable view of one [`NetMonTable`](crate::netmon::NetMonTable)
//! ring. Its
//! `handle: usize` is the packed registry handle; resolution validates the
//! slot's generation, so a stale handle is a typed `EBADF` rather than a read
//! of whoever recycled the slot.
//!
//! - `poll_fused` registers the caller on the ring's wait queue **before**
//!   testing readiness, which is what closes the window in which a post
//!   between the test and the block would be missed.
//! - `read` drains whole [`NetEvent`] records: a buffer shorter than one record
//!   is `EINVAL`, an empty ring is `EAGAIN`, and anything longer takes as many
//!   whole records as fit. There is no partial record and no framing to parse.
//! - `write` is meaningless (`EINVAL`): the stream runs one way, and
//!   configuration is changed through the `net_*_ctl` syscalls.
//!
//! Each record is peeked, copied out, and only then consumed, so a user buffer
//! that faults mid-drain costs the reader nothing it has not already received.

use slopos_abi::Errno;
use slopos_abi::event::KernelEvent;
use slopos_abi::file_ops::{FileKind, FileOps, FusedPollResult};
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::net::NET_EVENT_LEN;
use slopos_abi::quota::ObjectRow;
use slopos_abi::syscall::{POLLIN, POLLNVAL};
use slopos_fs::fileio::FdTable;
use slopos_ostd::KArc;
use slopos_ostd::process::quota::{Charge, FileBacking, try_charge};
use slopos_ostd::sync::event_bus::BUS;

use crate::netmon::NETMON_TABLE;

pub struct NetmonFileOps;

pub static NETMON_FILE_OPS: NetmonFileOps = NetmonFileOps;

/// Owns one monitor registry entry. The open-file layer holds it as a
/// `KArc<dyn FileBacking>`; dropping the last fd alias releases the ring.
#[derive(slopos_ostd::Charged)]
pub(crate) struct NetmonBacking {
    pub(crate) handle: usize,
    pub(crate) object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(NetmonBacking);

impl FileBacking for NetmonBacking {}

impl Drop for NetmonBacking {
    fn drop(&mut self) {
        NETMON_TABLE.close(self.handle);
    }
}

impl FileOps for NetmonFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Netmon
    }

    fn read(&self, handle: usize, buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        if buf.len() < NET_EVENT_LEN {
            return Errno::EINVAL.as_isize();
        }
        let mut written = 0usize;
        while written + NET_EVENT_LEN <= buf.len() {
            let event = match NETMON_TABLE.peek(handle) {
                Ok(Some(event)) => event,
                Ok(None) => break,
                Err(e) => {
                    if written == 0 {
                        return e.as_isize();
                    }
                    break;
                }
            };
            if let Err(e) = buf.copy_in(written, &event.to_bytes()) {
                if written == 0 {
                    return e.as_isize();
                }
                break;
            }
            // The record is delivered; only now does the reader lose it.
            let _ = NETMON_TABLE.commit(handle, &event);
            written += NET_EVENT_LEN;
        }
        if written == 0 {
            // Readiness is reported by poll, so a bare read of an empty ring
            // does not block.
            return Errno::EAGAIN.as_isize();
        }
        written as isize
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        Errno::EINVAL.as_isize()
    }

    fn poll_fused(&self, handle: usize, _events: u16) -> FusedPollResult {
        let Some(slot) = NETMON_TABLE.slot_of(handle) else {
            return FusedPollResult {
                revents: POLLNVAL,
                registered: false,
                open_file_token: 0,
            };
        };
        // Register first, test second: a post landing in between has already
        // marked this task.
        let registered = BUS.subscribe_current(KernelEvent::NetMonitor { mon: slot });
        let revents = if NETMON_TABLE.is_readable(handle) {
            POLLIN
        } else {
            0
        };
        FusedPollResult {
            revents,
            registered,
            open_file_token: 0,
        }
    }

    fn poll_events(&self, handle: usize, _events: u16) -> u16 {
        match NETMON_TABLE.slot_of(handle) {
            Some(_) if NETMON_TABLE.is_readable(handle) => POLLIN,
            Some(_) => 0,
            None => POLLNVAL,
        }
    }

    fn poll_wait(&self, handle: usize) -> bool {
        match NETMON_TABLE.slot_of(handle) {
            Some(slot) => BUS.subscribe_current(KernelEvent::NetMonitor { mon: slot }),
            None => false,
        }
    }

    fn poll_unwait(&self, handle: usize) {
        if let Some(slot) = NETMON_TABLE.slot_of(handle) {
            BUS.unsubscribe_current(KernelEvent::NetMonitor { mon: slot });
        }
    }
}

/// Open a network-state monitor for `table`'s owner subscribed to `mask`, and
/// install it as an fd. Returns the fd (`>= 0`) or a negated errno — see
/// [`NetMonTable::open`](crate::netmon::NetMonTable::open) for which.
pub fn netmon_create(table: FdTable, mask: u32) -> i32 {
    let Ok(reservation) = try_charge::<ObjectRow>(table.account(), 1) else {
        return Errno::ENFILE.raw();
    };
    let raw_handle = match NETMON_TABLE.open(table, mask) {
        Ok(handle) => handle,
        Err(e) => return e.raw(),
    };
    // The backing owns the registry entry: dropping the last fd alias runs its
    // `Drop`, which releases the ring. If the allocation itself fails there is
    // no backing to hand off, so release the orphaned entry here.
    let backing: KArc<dyn FileBacking> = match KArc::try_new(NetmonBacking {
        handle: raw_handle,
        object_charge: Charge::commit(reservation),
    }) {
        Ok(backing) => backing,
        Err(_) => {
            NETMON_TABLE.close(raw_handle);
            return Errno::ENOMEM.raw();
        }
    };
    // On install failure the fd layer drops the backing, which releases the
    // entry — no manual cleanup on the error arm.
    slopos_fs::fileio_open_fd_with_ops(
        table,
        &NETMON_FILE_OPS,
        raw_handle,
        Some(backing),
        slopos_fs::FdFlags::NONE,
    )
}
