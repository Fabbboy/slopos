#![forbid(unsafe_op_in_unsafe_fn)]

use crate::fmt;
use crate::io::{self, BorrowedCursor, IoSlice, IoSliceMut};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use crate::sys::{AsInner, FromInner, IntoInner};

unsafe extern "C" {
    fn slopos_pipe(fds: *mut i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
}

pub struct Pipe(OwnedFd);

impl Pipe {
    pub fn from_raw_fd(fd: i32) -> Pipe {
        Pipe(unsafe { OwnedFd::from_raw_fd(fd) })
    }

    pub fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}

pub fn pipe() -> io::Result<(Pipe, Pipe)> {
    let mut fds = [0i32; 2];
    let ret = unsafe { slopos_pipe(fds.as_mut_ptr()) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok((
            Pipe(unsafe { OwnedFd::from_raw_fd(fds[0]) }),
            Pipe(unsafe { OwnedFd::from_raw_fd(fds[1]) }),
        ))
    }
}

impl Pipe {
    pub fn try_clone(&self) -> io::Result<Self> {
        Err(io::const_error!(
            io::ErrorKind::Unsupported,
            "pipe clone not supported"
        ))
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let ret = unsafe { read(self.0.as_raw_fd(), buf.as_mut_ptr(), buf.len()) };
        if ret < 0 {
            Err(io::Error::from_raw_os_error(-ret as i32))
        } else {
            Ok(ret as usize)
        }
    }

    pub fn read_buf(&self, mut cursor: BorrowedCursor<'_>) -> io::Result<()> {
        let buf = unsafe { cursor.as_mut() };
        let ret = unsafe { read(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
        if ret < 0 {
            Err(io::Error::from_raw_os_error(-ret as i32))
        } else {
            unsafe { cursor.advance(ret as usize) };
            Ok(())
        }
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            match self.read(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if total > 0 => return Ok(total),
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    pub fn is_read_vectored(&self) -> bool {
        false
    }

    pub fn read_to_end(&self, buf: &mut Vec<u8>) -> io::Result<usize> {
        let mut total = 0;
        let mut tmp = [0u8; 4096];
        loop {
            match self.read(&mut tmp) {
                Ok(0) => break,
                Ok(n) => {
                    buf.extend_from_slice(&tmp[..n]);
                    total += n;
                }
                Err(e) if total > 0 => return Ok(total),
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        let ret = unsafe { write(self.0.as_raw_fd(), buf.as_ptr(), buf.len()) };
        if ret < 0 {
            Err(io::Error::from_raw_os_error(-ret as i32))
        } else {
            Ok(ret as usize)
        }
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        let mut total = 0;
        for buf in bufs {
            match self.write(buf) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if total > 0 => return Ok(total),
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    pub fn is_write_vectored(&self) -> bool {
        false
    }

    pub fn diverge(&self) -> ! {
        panic!("pipe diverge called on live pipe")
    }
}

// OwnedFd handles close on drop — no explicit Drop needed.

impl fmt::Debug for Pipe {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pipe")
            .field("fd", &self.0.as_raw_fd())
            .finish()
    }
}

impl AsInner<OwnedFd> for Pipe {
    fn as_inner(&self) -> &OwnedFd {
        &self.0
    }
}

impl IntoInner<OwnedFd> for Pipe {
    fn into_inner(self) -> OwnedFd {
        self.0
    }
}

impl FromInner<OwnedFd> for Pipe {
    fn from_inner(fd: OwnedFd) -> Self {
        Self(fd)
    }
}

impl AsFd for Pipe {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl crate::os::fd::AsRawFd for Pipe {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl crate::os::fd::IntoRawFd for Pipe {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl crate::os::fd::FromRawFd for Pipe {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}
