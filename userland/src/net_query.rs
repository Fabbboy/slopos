//! Reading `net_query`'s fixed-stride buffers, shared by `/bin/ip` and the
//! compositor's status indicator. Every query answers in the same shape: a
//! [`UserNetQueryHdr`] followed by an array of one record type.
//!
//! **The header's `record_size` is the stride, not this build's `size_of`.**
//! That is the ABI's forward-compatibility lever: a newer kernel may grow a
//! record, and a client that strides by the kernel's number keeps reading the
//! prefix it understands.
//!
//! Sizing is a two-call protocol: a header-sized buffer returns
//! `record_count == 0` and `total_count == N`.

use std::vec::Vec;

use slopos_abi::net::{NET_IFINDEX_NONE, NET_IFNAMSIZ, NET_Q_IFACES, UserIface, UserNetQueryHdr};

use crate::syscall::SyscallResult;
use crate::syscall::net::net_query;

pub struct Query<T> {
    pub hdr: UserNetQueryHdr,
    pub records: Vec<T>,
}

impl<T> Query<T> {
    /// Whether the kernel had more to say than fit. Not an error: the state can
    /// grow between the sizing call and the reading call.
    pub fn truncated(&self) -> bool {
        self.hdr.total_count as usize > self.records.len()
    }
}

/// Copy one record out of a byte buffer.
///
/// Byte-wise because neither the kernel's stride nor this build's alignment for
/// `T` is something the other side promised. A stride longer than `T` keeps the
/// prefix; a shorter one leaves the tail at its `Default` value.
fn decode<T: Copy + Default>(bytes: &[u8]) -> T {
    let mut out = T::default();
    let n = bytes.len().min(core::mem::size_of::<T>());
    // SAFETY: `T` is a `#[repr(C)]` ABI struct of plain integers and byte
    // arrays, so every bit pattern of its first `n` bytes is a valid value;
    // `out` is uniquely borrowed and at least `size_of::<T>() >= n` bytes long;
    // and `bytes` is a distinct slice of at least `n` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), (&raw mut out).cast::<u8>(), n);
    }
    out
}

/// Run one query to completion.
///
/// `ifindex` filters to a single interface, or [`NET_IFINDEX_NONE`] for all.
pub fn fetch<T: Copy + Default>(what: u32, ifindex: u32) -> SyscallResult<Query<T>> {
    const HDR: usize = core::mem::size_of::<UserNetQueryHdr>();

    let mut probe = [0u8; HDR];
    net_query(what, ifindex, &mut probe)?;
    let sizing = decode::<UserNetQueryHdr>(&probe);

    let want = sizing.total_count as usize;
    if want == 0 {
        return Ok(Query {
            hdr: sizing,
            records: Vec::new(),
        });
    }

    let stride = (sizing.record_size as usize).max(1);
    let mut buf = std::vec![0u8; HDR + want * stride];
    net_query(what, ifindex, &mut buf)?;

    // A second snapshot: these counts and stride, not the sizing call's,
    // describe the bytes actually in `buf`.
    let hdr = decode::<UserNetQueryHdr>(&buf);
    let stride = (hdr.record_size as usize).max(1);
    let count = (hdr.record_count as usize).min(buf.len().saturating_sub(HDR) / stride);

    let mut records = Vec::with_capacity(count);
    for i in 0..count {
        let start = HDR + i * stride;
        records.push(decode::<T>(&buf[start..start + stride]));
    }
    Ok(Query { hdr, records })
}

/// The whole interface table: routes and addresses name an interface by index,
/// and a person reads names.
///
/// Deliberately unfiltered even when a `dev` operand is present — resolving a
/// name to an index needs every row, and the per-object query that follows
/// filters kernel-side.
pub struct Ifaces {
    pub rows: Vec<UserIface>,
    /// How many interfaces the kernel had, which may exceed what fit.
    pub total: u32,
}

impl Ifaces {
    pub fn fetch() -> SyscallResult<Ifaces> {
        let q = fetch::<UserIface>(NET_Q_IFACES, NET_IFINDEX_NONE)?;
        Ok(Ifaces {
            total: q.hdr.total_count,
            rows: q.records,
        })
    }

    pub fn truncated(&self) -> bool {
        self.total as usize > self.rows.len()
    }

    pub fn name_of(&self, ifindex: u32) -> Option<&str> {
        self.rows
            .iter()
            .find(|row| row.ifindex == ifindex)
            .map(name_of)
    }

    /// The interface called `name`. Exact match, never a prefix: abbreviating a
    /// device name would change a command's meaning when a new interface
    /// appears.
    pub fn find(&self, name: &[u8]) -> Option<&UserIface> {
        self.rows.iter().find(|row| name_of(row).as_bytes() == name)
    }
}

/// An interface's name as text. The ABI field is NUL-*padded* and not
/// NUL-terminated when the name fills it exactly.
pub fn name_of(iface: &UserIface) -> &str {
    let end = iface
        .name
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(NET_IFNAMSIZ);
    core::str::from_utf8(&iface.name[..end]).unwrap_or("?")
}

/// How a record's interface is named, falling back to `if#N` when the table
/// lacks the index: a route can name an interface that went away between two
/// queries.
pub fn name_or_index(ifaces: &Ifaces, ifindex: u32) -> std::string::String {
    match ifaces.name_of(ifindex) {
        Some(name) => std::string::String::from(name),
        None => std::format!("if#{ifindex}"),
    }
}
