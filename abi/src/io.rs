//! Zero-copy I/O buffer abstraction (inspired by Linux `iov_iter`).
//!
//! This module provides [`IoBuf`], a trait that abstracts over I/O
//! destinations and sources.  Kernel subsystems that transfer data
//! (VFS, block drivers, pipes, TTY) accept `&mut dyn IoBuf` instead
//! of raw `&mut [u8]`, which lets the same code path write directly
//! into either a kernel buffer or a validated user-space region —
//! eliminating the bounce-buffer overhead of the old 512-byte
//! `USER_IO_MAX_BYTES` design.
//!
//! ## Implementations
//!
//! | Type            | Crate  | Backing              |
//! |-----------------|--------|----------------------|
//! | [`KernelIoBuf`] | `abi`  | `&mut [u8]` slice    |
//! | `UserIoBuf`     | `mm`   | user-space address   |
//!
//! ## Example (kernel-side read)
//!
//! ```ignore
//! fn read(&self, inode: u64, offset: u64, buf: &mut dyn IoBuf) -> Result<usize, i32> {
//!     let block = self.read_block(block_num)?;
//!     buf.write_at(pos, &block[start..end])
//! }
//! ```

/// Abstraction over I/O destinations and sources.
///
/// Both reads (kernel→buffer) and writes (buffer→kernel) go through
/// this trait so that the underlying transport — `memcpy` for kernel
/// buffers, `copy_to_user`/`copy_from_user` for user-space — is
/// selected at the point of construction, not at every call site.
///
/// Error values are negative POSIX errno (e.g. `−14` = `EFAULT`).
pub trait IoBuf {
    /// Copy `src` into this buffer starting at byte `offset`.
    ///
    /// Returns the number of bytes actually written (may be less than
    /// `src.len()` if the buffer is shorter).  Returns `Err(-EFAULT)`
    /// if the destination is unreachable.
    fn write_at(&mut self, offset: usize, src: &[u8]) -> Result<usize, i32>;

    /// Copy bytes from this buffer starting at byte `offset` into `dst`.
    ///
    /// Returns the number of bytes actually read.
    fn read_at(&self, offset: usize, dst: &mut [u8]) -> Result<usize, i32>;

    /// Total capacity (bytes) of this buffer.
    fn len(&self) -> usize;

    /// Whether this buffer has zero capacity.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// ---------------------------------------------------------------------------
// KernelIoBuf — wraps a plain kernel `&mut [u8]`
// ---------------------------------------------------------------------------

/// An [`IoBuf`] backed by a kernel-owned byte slice.
///
/// Used by kernel-internal callers that already have a `&mut [u8]`
/// (e.g. block-cache reads, in-kernel file copies).  All operations
/// are plain `copy_from_slice` — no user-space validation needed.
pub struct KernelIoBuf<'a> {
    buf: &'a mut [u8],
}

impl<'a> KernelIoBuf<'a> {
    /// Wrap an existing kernel slice.
    #[inline]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }
}

impl IoBuf for KernelIoBuf<'_> {
    #[inline]
    fn write_at(&mut self, offset: usize, src: &[u8]) -> Result<usize, i32> {
        let Some(dest) = self.buf.get_mut(offset..) else {
            return Err(-14); // EFAULT
        };
        let n = src.len().min(dest.len());
        dest[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    #[inline]
    fn read_at(&self, offset: usize, dst: &mut [u8]) -> Result<usize, i32> {
        let Some(src) = self.buf.get(offset..) else {
            return Err(-14); // EFAULT
        };
        let n = dst.len().min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    #[inline]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

// ---------------------------------------------------------------------------
// KernelIoBufRef — read-only variant for writes (buffer→device)
// ---------------------------------------------------------------------------

/// A read-only [`IoBuf`] backed by a kernel-owned byte slice.
///
/// Used when the kernel needs to provide data to a write path but
/// the caller only has `&[u8]` (e.g., syscall write where the data
/// has already been copied from user-space, or kernel-internal
/// writes).  `write_at` always returns `EFAULT` since this buffer
/// is read-only.
pub struct KernelIoBufRef<'a> {
    buf: &'a [u8],
}

impl<'a> KernelIoBufRef<'a> {
    /// Wrap an existing read-only kernel slice.
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl IoBuf for KernelIoBufRef<'_> {
    #[inline]
    fn write_at(&mut self, _offset: usize, _src: &[u8]) -> Result<usize, i32> {
        Err(-14) // EFAULT — read-only buffer
    }

    #[inline]
    fn read_at(&self, offset: usize, dst: &mut [u8]) -> Result<usize, i32> {
        let Some(src) = self.buf.get(offset..) else {
            return Err(-14); // EFAULT
        };
        let n = dst.len().min(src.len());
        dst[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    #[inline]
    fn len(&self) -> usize {
        self.buf.len()
    }
}
