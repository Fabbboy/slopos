#![feature(restricted_std)]

//! fd-based clipboard: wire format + memfd transport.
//!
//! Regression target: the clipboard used to round-trip through a fixed
//! `[u8; 4096]` inline array, silently truncating large selections. It now
//! carries only a `u32` length on the wire with the bytes in a memfd passed
//! via SCM_RIGHTS. These tests pin the new wire shape (tag + u32, no inline
//! array) and prove a >4 KiB payload survives the memfd transport.

use slopos_userland as _;

use slopos_protocol::connection::MAX_PENDING_FDS;
use slopos_protocol::types::{Event, Request};
use slopos_protocol::{Decode, Encode, FdFifo};
use slopos_userland::syscall::memory;
use slopos_userland::syscall::{CachedShmMapping, ShmBuffer, fs};

/// ClipboardCopy encodes to exactly `[tag=13][len:u32 LE]` — no 4 KiB array.
fn test_clipboard_copy_wire_is_tag_plus_len() -> bool {
    let req = Request::ClipboardCopy {
        len: 0x00AB_CDEF,
        buffer_fd: None,
    };
    let mut buf = [0u8; 64];
    let n = match req.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    n == 5 && buf[0] == 13 && u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) == 0x00AB_CDEF
}

/// ClipboardRead encodes to `[tag=17][len:u32 LE]`.
fn test_clipboard_read_wire_is_tag_plus_len() -> bool {
    let req = Request::ClipboardRead {
        len: 1_048_576,
        buffer_fd: None,
    };
    let mut buf = [0u8; 64];
    let n = match req.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    n == 5 && buf[0] == 17 && u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) == 1_048_576
}

/// PasteReady / PasteResult events encode to `[tag][len:u32 LE]`.
fn test_paste_events_wire_is_tag_plus_len() -> bool {
    let mut buf = [0u8; 64];
    let ready = Event::PasteReady { len: 5000 };
    let n = match ready.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    if n != 5 || buf[0] != 17 || u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) != 5000 {
        return false;
    }
    let result = Event::PasteResult { len: 5000 };
    let n = match result.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    n == 5 && buf[0] == 15 && u32::from_le_bytes([buf[1], buf[2], buf[3], buf[4]]) == 5000
}

/// Decode with an empty fd FIFO yields `buffer_fd: None`; the length survives.
fn test_clipboard_copy_decode_without_fd() -> bool {
    let req = Request::ClipboardCopy {
        len: 777_777,
        buffer_fd: None,
    };
    let mut buf = [0u8; 64];
    let n = match req.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => return false,
    };
    let mut fds = [-1i32; MAX_PENDING_FDS];
    let mut count = 0u8;
    let mut fifo = FdFifo::new(&mut fds, &mut count);
    match Request::decode(&buf[..n], &mut fifo) {
        Ok((Request::ClipboardCopy { len, buffer_fd }, _)) => len == 777_777 && buffer_fd.is_none(),
        _ => false,
    }
}

/// Decode pulls the SCM_RIGHTS fd out of the FIFO into `buffer_fd`.
fn test_clipboard_copy_decode_with_fd() -> bool {
    // A standalone memfd to prime the FIFO; the decoded OwnedFd owns and
    // closes it (so we must NOT also close it here).
    let fd = memory::memfd_create(0);
    if fd < 0 {
        return false;
    }
    let req = Request::ClipboardCopy {
        len: 42,
        buffer_fd: None,
    };
    let mut buf = [0u8; 64];
    let n = match req.encode(&mut buf) {
        Ok(n) => n,
        Err(_) => {
            memory::close(fd);
            return false;
        }
    };
    let mut fds = [-1i32; MAX_PENDING_FDS];
    fds[0] = fd;
    let mut count = 1u8;
    let mut fifo = FdFifo::new(&mut fds, &mut count);
    match Request::decode(&buf[..n], &mut fifo) {
        Ok((Request::ClipboardCopy { len, buffer_fd }, _)) => {
            let ok = len == 42 && buffer_fd.as_ref().map(|f| f.raw()) == Some(fd);
            // `buffer_fd` (OwnedFd) drops here, closing `fd`.
            ok
        }
        _ => {
            memory::close(fd);
            false
        }
    }
}

/// A >4 KiB payload survives the memfd transport: fill a 100 KiB source memfd,
/// map a duplicate of its fd read-only (as the compositor does on copy), and
/// confirm every byte matches — the old 4096 cap is gone end to end.
fn test_memfd_transport_above_4kib() -> bool {
    const SIZE: usize = 100 * 1024;
    let mut src = match ShmBuffer::create(SIZE) {
        Ok(s) => s,
        Err(_) => return false,
    };
    for (i, b) in src.as_mut_slice().iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    // Dup the fd so the read-only mapping (which closes its fd on drop) does
    // not collide with the ShmBuffer's own fd.
    let dup = match fs::dup(src.fd()) {
        Ok(f) => f.into_raw(),
        Err(_) => return false,
    };
    let mapping = match CachedShmMapping::map_readonly_fd(dup, SIZE) {
        Some(m) => m,
        None => return false,
    };
    mapping.as_slice() == src.as_slice()
}

fn main() {
    slopos_slibc::test_harness::run(&[
        (
            "clipboard_copy_wire_is_tag_plus_len",
            test_clipboard_copy_wire_is_tag_plus_len,
        ),
        (
            "clipboard_read_wire_is_tag_plus_len",
            test_clipboard_read_wire_is_tag_plus_len,
        ),
        (
            "paste_events_wire_is_tag_plus_len",
            test_paste_events_wire_is_tag_plus_len,
        ),
        (
            "clipboard_copy_decode_without_fd",
            test_clipboard_copy_decode_without_fd,
        ),
        (
            "clipboard_copy_decode_with_fd",
            test_clipboard_copy_decode_with_fd,
        ),
        (
            "memfd_transport_above_4kib",
            test_memfd_transport_above_4kib,
        ),
    ]);
}
