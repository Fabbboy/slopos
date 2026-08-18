//! `ring_register(2)` entry points (SLOPRING § 13, ABI v2): registered fixed
//! buffers + provided buffer rings.
//!
//! The `core` syscall handler validates and copies the typed argument out of
//! user memory before calling these; each re-checks ownership and runs the pin
//! / drop under the per-ring serialization lock via [`registry::with_ring`].

use slopos_abi::Errno;
use slopos_abi::ring::RegisterBufRingCmd;
use slopos_fs::fileio::FdTable;

use crate::registry;

fn eno(e: Errno) -> i32 {
    e.raw()
}

/// `RING_REGISTER_BUFFERS`: pin `iovecs` (`(addr, len)` pairs) as the ring's
/// registered fixed-buffer set. Returns 0 or a negated errno.
pub fn ring_register_buffers(table: FdTable, raw_handle: usize, iovecs: &[(u64, u32)]) -> i32 {
    if !registry::owner_is(raw_handle, table) {
        return eno(Errno::EBADF);
    }
    let Some(vm_process) = table.process() else {
        return eno(Errno::EINVAL);
    };
    match registry::with_ring(raw_handle, |ring| {
        ring.buffers.register_fixed(vm_process, iovecs)
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => eno(e),
        Err(_) => eno(Errno::EBADF),
    }
}

/// `RING_UNREGISTER_BUFFERS`: drop the fixed-buffer set. `-EBUSY` if a buffer
/// is still held by an in-flight op.
pub fn ring_unregister_buffers(table: FdTable, raw_handle: usize) -> i32 {
    if !registry::owner_is(raw_handle, table) {
        return eno(Errno::EBADF);
    }
    match registry::with_ring(raw_handle, |ring| ring.buffers.unregister_fixed()) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => eno(e),
        Err(_) => eno(Errno::EBADF),
    }
}

/// `RING_REGISTER_PBUF_RING`: pin and register a provided buffer ring for
/// `cmd.buf_group`. Returns 0 or a negated errno.
pub fn ring_register_pbuf_ring(table: FdTable, raw_handle: usize, cmd: &RegisterBufRingCmd) -> i32 {
    if !registry::owner_is(raw_handle, table) {
        return eno(Errno::EBADF);
    }
    let Some(vm_process) = table.process() else {
        return eno(Errno::EINVAL);
    };
    match registry::with_ring(raw_handle, |ring| {
        ring.buffers.register_provided(vm_process, cmd)
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => eno(e),
        Err(_) => eno(Errno::EBADF),
    }
}

/// `RING_UNREGISTER_PBUF_RING`: drop the provided ring for `group`. `-EBUSY`
/// while any in-flight row still selects that group; userland must cancel or
/// drain those first.
pub fn ring_unregister_pbuf_ring(table: FdTable, raw_handle: usize, group: u16) -> i32 {
    if !registry::owner_is(raw_handle, table) {
        return eno(Errno::EBADF);
    }
    match registry::with_ring(raw_handle, |ring| {
        if ring.inflight.iter().any(|r| r.buf_group == group) {
            return Err(Errno::EBUSY);
        }
        ring.buffers.unregister_provided(group)
    }) {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => eno(e),
        Err(_) => eno(Errno::EBADF),
    }
}
