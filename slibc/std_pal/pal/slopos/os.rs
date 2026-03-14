use crate::ffi::{OsStr, OsString};
use crate::marker::PhantomData;
use crate::path::{self, PathBuf};
use crate::{fmt, io};

mod ffi {
    unsafe extern "C" {
        pub fn getcwd(buf: *mut u8, size: usize) -> *mut u8;
        pub fn chdir(path: *const u8) -> i32;
        pub fn getpid() -> i32;
        pub fn _exit(code: i32) -> !;
    }
}

pub fn getcwd() -> io::Result<PathBuf> {
    let mut buf = [0u8; 4096];
    let ret = unsafe { ffi::getcwd(buf.as_mut_ptr(), buf.len()) };
    if ret.is_null() {
        Err(io::Error::last_os_error())
    } else {
        let len = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
        let s = core::str::from_utf8(&buf[..len]).unwrap_or("/");
        Ok(PathBuf::from(s))
    }
}

pub fn chdir(p: &path::Path) -> io::Result<()> {
    let path_str = p.to_str().ok_or_else(|| {
        io::const_error!(io::ErrorKind::InvalidInput, "path contains invalid UTF-8")
    })?;
    let mut buf = [0u8; 4096];
    let bytes = path_str.as_bytes();
    if bytes.len() >= buf.len() {
        return Err(io::const_error!(
            io::ErrorKind::InvalidInput,
            "path too long"
        ));
    }
    buf[..bytes.len()].copy_from_slice(bytes);
    buf[bytes.len()] = 0;
    let ret = unsafe { ffi::chdir(buf.as_ptr()) };
    if ret < 0 {
        Err(io::Error::from_raw_os_error(-ret))
    } else {
        Ok(())
    }
}

pub struct SplitPaths<'a>(!, PhantomData<&'a ()>);

pub fn split_paths(_unparsed: &OsStr) -> SplitPaths<'_> {
    panic!("unsupported")
}

impl<'a> Iterator for SplitPaths<'a> {
    type Item = PathBuf;
    fn next(&mut self) -> Option<PathBuf> {
        self.0
    }
}

#[derive(Debug)]
pub struct JoinPathsError;

pub fn join_paths<I, T>(_paths: I) -> Result<OsString, JoinPathsError>
where
    I: Iterator<Item = T>,
    T: AsRef<OsStr>,
{
    Err(JoinPathsError)
}

impl fmt::Display for JoinPathsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        "not supported on SlopOS yet".fmt(f)
    }
}

impl crate::error::Error for JoinPathsError {}

pub fn current_exe() -> io::Result<PathBuf> {
    Err(io::const_error!(
        io::ErrorKind::Unsupported,
        "current_exe not supported on SlopOS"
    ))
}

pub fn temp_dir() -> PathBuf {
    PathBuf::from("/tmp")
}

pub fn home_dir() -> Option<PathBuf> {
    None
}

pub fn exit(code: i32) -> ! {
    unsafe { ffi::_exit(code) }
}

pub fn getpid() -> u32 {
    unsafe { ffi::getpid() as u32 }
}
