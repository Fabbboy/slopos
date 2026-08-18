//! Directional zero-copy buffer traits for FileOps I/O paths.

use crate::errno::Errno;

pub const IO_STAGING_SIZE: usize = 4096;

pub trait IoBufRead {
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub trait IoBufWrite {
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

pub struct KernelIoBuf<'a> {
    buf: &'a mut [u8],
}

impl<'a> KernelIoBuf<'a> {
    #[inline]
    pub fn new(buf: &'a mut [u8]) -> Self {
        Self { buf }
    }
}

impl IoBufWrite for KernelIoBuf<'_> {
    #[inline]
    fn copy_in(&mut self, offset: usize, src: &[u8]) -> Result<usize, Errno> {
        let Some(dest) = self.buf.get_mut(offset..) else {
            return Err(Errno::EFAULT);
        };
        let n = src.len().min(dest.len());
        dest[..n].copy_from_slice(&src[..n]);
        Ok(n)
    }

    #[inline]
    fn len(&self) -> usize {
        self.buf.len()
    }
}

pub struct KernelIoBufRef<'a> {
    buf: &'a [u8],
}

impl<'a> KernelIoBufRef<'a> {
    #[inline]
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf }
    }
}

impl IoBufRead for KernelIoBufRef<'_> {
    #[inline]
    fn copy_out(&self, offset: usize, dst: &mut [u8]) -> Result<usize, Errno> {
        let Some(src) = self.buf.get(offset..) else {
            return Err(Errno::EFAULT);
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
