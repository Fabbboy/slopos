//! SlopOS file descriptor abstraction.
//!
//! Mirrors `sys/fd/unix.rs` but calls slibc C functions instead of libc.

#![unstable(reason = "not public", issue = "none", feature = "fd")]
#![deny(unsafe_op_in_unsafe_fn)]

use crate::cmp;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut, Read};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use crate::sys::pal::cvt;
use crate::sys::{AsInner, FromInner, IntoInner};

unsafe extern "C" {
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn close(fd: i32) -> i32;
    fn dup(oldfd: i32) -> i32;
    fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64;
    fn fcntl(fd: i32, cmd: i32, arg: i64) -> i32;
}

const READ_LIMIT: usize = isize::MAX as usize;

const F_GETFD: i32 = 1;
const F_SETFD: i32 = 2;
const F_GETFL: i32 = 3;
const F_SETFL: i32 = 4;
const FD_CLOEXEC: i32 = 1;
const O_NONBLOCK: i32 = 0x800;

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;

#[derive(Debug)]
pub struct FileDesc(OwnedFd);

impl FileDesc {
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self(self.0.try_clone()?))
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = cvt(unsafe {
            read(self.as_raw_fd(), buf.as_mut_ptr(), cmp::min(buf.len(), READ_LIMIT))
        })?;
        Ok(ret as usize)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            if buf.is_empty() {
                continue;
            }
            match self.read(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if total > 0 => {
                    let _ = e;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    #[inline]
    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut me = self;
        (&mut me).read_to_end(buf)
    }

    pub fn read_at(&self, buf: &mut [u8], offset: u64) -> io::Result<usize> {
        // No pread — emulate with lseek + read (not atomic, but sufficient)
        let saved = cvt(unsafe { slopos_lseek(self.as_raw_fd(), 0, SEEK_CUR) })?;
        cvt(unsafe { slopos_lseek(self.as_raw_fd(), offset as i64, SEEK_SET) })?;
        let result = self.read(buf);
        let _ = unsafe { slopos_lseek(self.as_raw_fd(), saved as i64, SEEK_SET) };
        result
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let ret = cvt(unsafe {
            read(
                self.as_raw_fd(),
                cursor.as_mut().as_mut_ptr() as *mut u8,
                cmp::min(cursor.capacity(), READ_LIMIT),
            )
        })?;
        unsafe {
            cursor.advance(ret as usize);
        }
        Ok(())
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let ret = cvt(unsafe {
            write(self.as_raw_fd(), buf.as_ptr(), cmp::min(buf.len(), READ_LIMIT))
        })?;
        Ok(ret as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            if buf.is_empty() {
                continue;
            }
            match self.write(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if total > 0 => {
                    let _ = e;
                    break;
                }
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    #[inline]
    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn write_at(&self, buf: &[u8], offset: u64) -> io::Result<usize> {
        let saved = cvt(unsafe { slopos_lseek(self.as_raw_fd(), 0, SEEK_CUR) })?;
        cvt(unsafe { slopos_lseek(self.as_raw_fd(), offset as i64, SEEK_SET) })?;
        let result = self.write(buf);
        let _ = unsafe { slopos_lseek(self.as_raw_fd(), saved as i64, SEEK_SET) };
        result
    }

    pub fn set_cloexec(&self) -> io::Result<()> {
        cvt(unsafe { fcntl(self.as_raw_fd(), F_SETFD, FD_CLOEXEC as i64) })?;
        Ok(())
    }

    pub fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        let flags = cvt(unsafe { fcntl(self.as_raw_fd(), F_GETFL, 0) })?;
        let flags = if nonblocking {
            flags | O_NONBLOCK
        } else {
            flags & !O_NONBLOCK
        };
        cvt(unsafe { fcntl(self.as_raw_fd(), F_SETFL, flags as i64) })?;
        Ok(())
    }

    pub fn duplicate(&self) -> io::Result<FileDesc> {
        let fd = cvt(unsafe { dup(self.as_raw_fd()) })?;
        Ok(FileDesc(unsafe { OwnedFd::from_raw_fd(fd) }))
    }
}

impl<'a> Read for &'a FileDesc {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read(buf)
    }
}

impl AsInner<OwnedFd> for FileDesc {
    #[inline]
    fn as_inner(&self) -> &OwnedFd {
        &self.0
    }
}

impl IntoInner<OwnedFd> for FileDesc {
    fn into_inner(self) -> OwnedFd {
        self.0
    }
}

impl FromInner<OwnedFd> for FileDesc {
    fn from_inner(owned_fd: OwnedFd) -> Self {
        Self(owned_fd)
    }
}

impl AsFd for FileDesc {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for FileDesc {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for FileDesc {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for FileDesc {
    unsafe fn from_raw_fd(raw_fd: RawFd) -> Self {
        Self(unsafe { OwnedFd::from_raw_fd(raw_fd) })
    }
}
