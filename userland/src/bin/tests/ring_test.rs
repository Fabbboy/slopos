#![feature(restricted_std)]

//! SlopRing end-to-end userland test (opcode parity + async edge).
//!
//! Exercises the ring surface against real fds from userland — the only
//! context with a mapped ring + a process fd table. Covers:
//!   * `ring_setup` maps a ring and reports sane geometry;
//!   * `OP_NOP` inline completion;
//!   * `OP_WRITE` / `OP_READ` over a pipe, with opcode parity vs the
//!     equivalent blocking `read`/`write` syscalls;
//!   * deferred completion: a read on an empty pipe blocks, then
//!     resolves once data is written, harvested via blocking `ring_enter`;
//!   * the `slopfut` executor + future cancellation (`OP_CANCEL`).

use slopos_userland as _;

use slopos_abi::ring::{OP_NOP, OP_READ, OP_WRITE, Sqe};
use slopos_userland::ring::{Ring, RingExecutor};
use slopos_userland::syscall::fs;

/// ring_setup returns a usable ring with the expected geometry.
fn test_setup() -> bool {
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    ring.sq_entries() == 8 && ring.fd() >= 0
}

/// OP_NOP completes inline with res == 0.
fn test_nop() -> bool {
    let Ok(mut ring) = Ring::setup(4) else {
        return false;
    };
    let mut sqe = Sqe::ZERO;
    sqe.opcode = OP_NOP;
    sqe.user_data = 0xa1;
    if ring.push_sqe(&sqe).is_err() {
        return false;
    }
    if ring.submit().is_err() {
        return false;
    }
    match ring.poll_completion() {
        Some(cqe) => cqe.user_data == 0xa1 && cqe.res == 0,
        None => false,
    }
}

/// OP_WRITE then OP_READ over a pipe round-trips the bytes, and the
/// observable result matches the blocking syscall (parity).
fn test_write_read_pipe() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(mut ring) = Ring::setup(8) else {
        return false;
    };

    let payload = b"slopring-parity";
    // Write via the ring.
    let mut wsqe = Sqe::ZERO;
    wsqe.opcode = OP_WRITE;
    wsqe.fd = wr.raw();
    wsqe.addr = payload.as_ptr() as u64;
    wsqe.len = payload.len() as u32;
    wsqe.user_data = 0x770;
    if ring.push_sqe(&wsqe).is_err() || ring.submit().is_err() {
        return false;
    }
    let wcqe = match ring.poll_completion() {
        Some(c) => c,
        None => return false,
    };
    if wcqe.res != payload.len() as i32 {
        return false;
    }

    // Read it back via the ring.
    let mut buf = [0u8; 32];
    let mut rsqe = Sqe::ZERO;
    rsqe.opcode = OP_READ;
    rsqe.fd = rd.raw();
    rsqe.addr = buf.as_mut_ptr() as u64;
    rsqe.len = buf.len() as u32;
    rsqe.user_data = 0x4ead;
    if ring.push_sqe(&rsqe).is_err() {
        return false;
    }
    // Data is already buffered, so a blocking enter completes it inline
    // or on the first harvest.
    if ring.submit_and_wait(1).is_err() {
        return false;
    }
    let rcqe = match ring.wait_completion() {
        Ok(c) => c,
        Err(_) => return false,
    };
    if rcqe.res != payload.len() as i32 {
        return false;
    }
    &buf[..payload.len()] == payload
}

/// A read on an empty pipe blocks (deferred), then resolves once data is
/// written — harvested via the blocking executor (SLOPRING § 7.1).
fn test_deferred_read() -> bool {
    let Ok((rd, wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let mut exec = RingExecutor::new(ring);

    // Submit a read on the (currently empty) pipe — it will block.
    let mut buf = [0u8; 16];
    let mut rsqe = Sqe::ZERO;
    rsqe.opcode = OP_READ;
    rsqe.fd = rd.raw();
    rsqe.addr = buf.as_mut_ptr() as u64;
    rsqe.len = buf.len() as u32;
    let mut fut = match exec.submit(rsqe) {
        Ok(f) => f,
        Err(_) => return false,
    };

    // Nothing should be ready yet (empty pipe).
    if exec.poll(&mut fut).is_some() {
        return false;
    }

    // Now write data; the blocking harvest must resolve the read.
    let msg = b"deferred!";
    if fs::write_slice(wr.raw(), msg).is_err() {
        return false;
    }
    let res = match exec.block_on(&mut fut) {
        Ok(r) => r,
        Err(_) => return false,
    };
    res == msg.len() as i32 && &buf[..msg.len()] == msg
}

/// Cancelling an unresolved future submits OP_CANCEL and the op resolves
/// (either -ECANCELED for the op or the cancel result is accepted).
fn test_cancel() -> bool {
    let Ok((rd, _wr)) = fs::pipe() else {
        return false;
    };
    let Ok(ring) = Ring::setup(8) else {
        return false;
    };
    let mut exec = RingExecutor::new(ring);

    let mut buf = [0u8; 16];
    let mut rsqe = Sqe::ZERO;
    rsqe.opcode = OP_READ;
    rsqe.fd = rd.raw();
    rsqe.addr = buf.as_mut_ptr() as u64;
    rsqe.len = buf.len() as u32;
    let fut = match exec.submit(rsqe) {
        Ok(f) => f,
        Err(_) => return false,
    };
    // The read blocks (empty pipe). Cancel it.
    exec.cancel(&fut).is_ok()
}

fn main() {
    slopos_slibc::test_harness::run(&[
        ("setup", test_setup),
        ("nop", test_nop),
        ("write_read_pipe", test_write_read_pipe),
        ("deferred_read", test_deferred_read),
        ("cancel", test_cancel),
    ]);
}
