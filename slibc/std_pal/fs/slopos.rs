#![deny(unsafe_op_in_unsafe_fn)]

use crate::ffi::OsString;
use crate::fmt;
use crate::fs::TryLockError;
use crate::hash::{Hash, Hasher};
use crate::io::{self, BorrowedCursor, Error, ErrorKind, IoSlice, IoSliceMut, SeekFrom};
use crate::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use crate::path::{Path, PathBuf};
pub use crate::sys::fs::common::Dir;
use crate::sys::time::SystemTime;
use crate::sys::{unsupported, unsupported_err, AsInner, FromInner, IntoInner};
use crate::vec::Vec;

const O_RDONLY: i32 = 0;
const O_WRONLY: i32 = 1;
const O_RDWR: i32 = 2;
const O_CREAT: i32 = 0x40;
const O_EXCL: i32 = 0x80;
const O_TRUNC: i32 = 0x200;
const O_APPEND: i32 = 0x400;

const SEEK_SET: i32 = 0;
const SEEK_CUR: i32 = 1;
const SEEK_END: i32 = 2;

const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;
const S_IFLNK: u32 = 0o120000;

const ENOENT: i32 = 2;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
struct SloposStat {
    st_mode: u32,
    st_size: u64,
    st_atime: i64,
    st_mtime: i64,
    st_ctime: i64,
}

unsafe extern "C" {
    fn open(path: *const u8, flags: i32) -> i32;
    fn close(fd: i32) -> i32;
    fn read(fd: i32, buf: *mut u8, count: usize) -> isize;
    fn write(fd: i32, buf: *const u8, count: usize) -> isize;
    fn slopos_lseek(fd: i32, offset: i64, whence: i32) -> i64;
    fn slopos_fstat(fd: i32, stat_buf: *mut SloposStat) -> i32;
    fn slopos_stat(path: *const u8, stat_buf: *mut SloposStat) -> i32;
    fn slopos_mkdir(path: *const u8, mode: u32) -> i32;
    fn slopos_unlink(path: *const u8) -> i32;
    fn slopos_rename(old: *const u8, new: *const u8) -> i32;
    fn slopos_dup(fd: i32) -> i32;
    fn slopos_list(path: *const u8, buf: *mut u8, buf_len: usize) -> isize;
}

pub struct File(crate::sys::fd::FileDesc);

impl File {
    pub fn as_raw_fd(&self) -> i32 {
        self.0.as_raw_fd()
    }

    fn fd(&self) -> i32 {
        self.0.as_raw_fd()
    }
}

#[derive(Clone)]
pub struct FileAttr {
    stat: SloposStat,
}

pub struct ReadDir {
    root: PathBuf,
    names: Vec<OsString>,
    pos: usize,
}

pub struct DirEntry {
    parent: PathBuf,
    name: OsString,
}

#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

#[derive(Copy, Clone, Debug, Default)]
pub struct FileTimes {
    accessed: Option<SystemTime>,
    modified: Option<SystemTime>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct FilePermissions {
    mode: u32,
}

#[derive(Copy, Clone, Eq)]
pub struct FileType {
    mode: u32,
}

#[derive(Debug)]
pub struct DirBuilder {
    mode: u32,
}

fn io_err_from_neg(ret: i32) -> io::Error {
    Error::from_raw_os_error(-ret)
}

fn cvt_i32(ret: i32) -> io::Result<i32> {
    if ret < 0 {
        Err(io_err_from_neg(ret))
    } else {
        Ok(ret)
    }
}

fn cvt_i64(ret: i64) -> io::Result<i64> {
    if ret < 0 {
        Err(Error::from_raw_os_error((-ret) as i32))
    } else {
        Ok(ret)
    }
}

fn cvt_isize(ret: isize) -> io::Result<isize> {
    if ret < 0 {
        Err(Error::from_raw_os_error((-ret) as i32))
    } else {
        Ok(ret)
    }
}

fn stat_from_path(path: &Path) -> io::Result<SloposStat> {
    let cpath = path_to_cstr(path)?;
    let mut st = SloposStat::default();
    let rc = unsafe { slopos_stat(cpath.as_ptr(), &mut st as *mut SloposStat) };
    cvt_i32(rc)?;
    Ok(st)
}

fn normalize_absolute(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    let mut pieces: Vec<OsString> = Vec::new();

    for comp in path.components() {
        match comp {
            crate::path::Component::RootDir => {
                pieces.clear();
            }
            crate::path::Component::CurDir => {}
            crate::path::Component::ParentDir => {
                let _ = pieces.pop();
            }
            crate::path::Component::Normal(name) => {
                pieces.push(name.to_os_string());
            }
            crate::path::Component::Prefix(_) => {}
        }
    }

    out.push(Path::new("/"));
    for piece in pieces {
        out.push(piece);
    }
    out
}

fn open_flags(opts: &OpenOptions) -> io::Result<i32> {
    let access = match (opts.read, opts.write, opts.append) {
        (true, false, false) => O_RDONLY,
        (false, true, false) => O_WRONLY,
        (true, true, false) => O_RDWR,
        (false, _, true) => O_WRONLY | O_APPEND,
        (true, _, true) => O_RDWR | O_APPEND,
        (false, false, false) => {
            return Err(io::const_error!(
                ErrorKind::InvalidInput,
                "invalid access mode"
            ));
        }
    };

    match (opts.write, opts.append) {
        (true, false) => {}
        (false, false) => {
            if opts.truncate || opts.create || opts.create_new {
                return Err(io::const_error!(
                    ErrorKind::InvalidInput,
                    "invalid creation mode"
                ));
            }
        }
        (_, true) => {
            if opts.truncate && !opts.create_new {
                return Err(io::const_error!(
                    ErrorKind::InvalidInput,
                    "invalid creation mode"
                ));
            }
        }
    }

    let creation = match (opts.create, opts.truncate, opts.create_new) {
        (false, false, false) => 0,
        (true, false, false) => O_CREAT,
        (false, true, false) => O_TRUNC,
        (true, true, false) => O_CREAT | O_TRUNC,
        (_, _, true) => O_CREAT | O_EXCL,
    };

    Ok(access | creation)
}

fn os_string_from_bytes_lossy(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

pub fn path_to_cstr(path: &Path) -> io::Result<Vec<u8>> {
    let bytes = path.as_os_str().as_encoded_bytes();
    if bytes.contains(&0) {
        return Err(io::const_error!(
            ErrorKind::InvalidInput,
            "path contains NUL byte"
        ));
    }
    let mut out = bytes.to_vec();
    out.push(0);
    Ok(out)
}

impl FileAttr {
    pub fn size(&self) -> u64 {
        self.stat.st_size
    }

    pub fn perm(&self) -> FilePermissions {
        FilePermissions {
            mode: self.stat.st_mode,
        }
    }

    pub fn file_type(&self) -> FileType {
        FileType {
            mode: self.stat.st_mode,
        }
    }

    pub fn modified(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(self.stat.st_mtime, 0))
    }

    pub fn accessed(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(self.stat.st_atime, 0))
    }

    pub fn created(&self) -> io::Result<SystemTime> {
        Ok(SystemTime::new(self.stat.st_ctime, 0))
    }
}

impl FilePermissions {
    pub fn readonly(&self) -> bool {
        self.mode & 0o222 == 0
    }

    pub fn set_readonly(&mut self, readonly: bool) {
        if readonly {
            self.mode &= !0o222;
        } else {
            self.mode |= 0o222;
        }
    }
}

impl fmt::Debug for FilePermissions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilePermissions")
            .field("mode", &self.mode)
            .finish()
    }
}

impl FileTimes {
    pub fn set_accessed(&mut self, t: SystemTime) {
        self.accessed = Some(t);
    }

    pub fn set_modified(&mut self, t: SystemTime) {
        self.modified = Some(t);
    }
}

impl FileType {
    pub fn is_dir(&self) -> bool {
        self.mode & S_IFMT == S_IFDIR
    }

    pub fn is_file(&self) -> bool {
        self.mode & S_IFMT == S_IFREG
    }

    pub fn is_symlink(&self) -> bool {
        self.mode & S_IFMT == S_IFLNK
    }
}

impl PartialEq for FileType {
    fn eq(&self, other: &Self) -> bool {
        (self.mode & S_IFMT) == (other.mode & S_IFMT)
    }
}

impl Hash for FileType {
    fn hash<H: Hasher>(&self, state: &mut H) {
        (self.mode & S_IFMT).hash(state);
    }
}

impl fmt::Debug for FileType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FileType")
            .field("mode", &self.mode)
            .finish()
    }
}

impl fmt::Debug for ReadDir {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.root, f)
    }
}

impl Iterator for ReadDir {
    type Item = io::Result<DirEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.pos >= self.names.len() {
            return None;
        }

        let name = self.names[self.pos].clone();
        self.pos += 1;
        Some(Ok(DirEntry {
            parent: self.root.clone(),
            name,
        }))
    }
}

impl DirEntry {
    pub fn path(&self) -> PathBuf {
        self.parent.join(&self.name)
    }

    pub fn file_name(&self) -> OsString {
        self.name.clone()
    }

    pub fn metadata(&self) -> io::Result<FileAttr> {
        stat(&self.path())
    }

    pub fn file_type(&self) -> io::Result<FileType> {
        Ok(self.metadata()?.file_type())
    }
}

impl OpenOptions {
    pub fn new() -> OpenOptions {
        OpenOptions {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    pub fn read(&mut self, read: bool) {
        self.read = read;
    }

    pub fn write(&mut self, write: bool) {
        self.write = write;
    }

    pub fn append(&mut self, append: bool) {
        self.append = append;
    }

    pub fn truncate(&mut self, truncate: bool) {
        self.truncate = truncate;
    }

    pub fn create(&mut self, create: bool) {
        self.create = create;
    }

    pub fn create_new(&mut self, create_new: bool) {
        self.create_new = create_new;
    }
}

impl File {
    pub fn open(path: &Path, opts: &OpenOptions) -> io::Result<File> {
        let cpath = path_to_cstr(path)?;
        let flags = open_flags(opts)?;
        let fd = unsafe { open(cpath.as_ptr(), flags) };
        let fd = cvt_i32(fd)?;
        Ok(File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) }))
    }

    pub fn file_attr(&self) -> io::Result<FileAttr> {
        let mut st = SloposStat::default();
        let rc = unsafe { slopos_fstat(self.fd(), &mut st as *mut SloposStat) };
        cvt_i32(rc)?;
        Ok(FileAttr { stat: st })
    }

    pub fn fsync(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn datasync(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn lock_shared(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn try_lock(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(unsupported_err()))
    }

    pub fn try_lock_shared(&self) -> Result<(), TryLockError> {
        Err(TryLockError::Error(unsupported_err()))
    }

    pub fn unlock(&self) -> io::Result<()> {
        unsupported()
    }

    pub fn truncate(&self, _size: u64) -> io::Result<()> {
        unsupported()
    }

    pub fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe { read(self.fd(), buf.as_mut_ptr(), buf.len()) };
        Ok(cvt_isize(n)? as usize)
    }

    pub fn read_vectored(&self, bufs: &mut [IoSliceMut<'_>]) -> io::Result<usize> {
        for buf in bufs {
            if !buf.is_empty() {
                return self.read(buf);
            }
        }
        Ok(0)
    }

    pub fn is_read_vectored(&self) -> bool {
        true
    }

    pub fn read_buf(&self, cursor: BorrowedCursor<'_>) -> io::Result<()> {
        io::default_read_buf(|buf| self.read(buf), cursor)
    }

    pub fn write(&self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let n = unsafe { write(self.fd(), buf.as_ptr(), buf.len()) };
        Ok(cvt_isize(n)? as usize)
    }

    pub fn write_vectored(&self, bufs: &[IoSlice<'_>]) -> io::Result<usize> {
        for buf in bufs {
            if !buf.is_empty() {
                return self.write(buf);
            }
        }
        Ok(0)
    }

    pub fn is_write_vectored(&self) -> bool {
        true
    }

    pub fn flush(&self) -> io::Result<()> {
        Ok(())
    }

    pub fn seek(&self, pos: SeekFrom) -> io::Result<u64> {
        let (offset, whence) = match pos {
            SeekFrom::Start(off) => {
                if off > i64::MAX as u64 {
                    return Err(io::const_error!(
                        ErrorKind::InvalidInput,
                        "seek offset out of range"
                    ));
                }
                (off as i64, SEEK_SET)
            }
            SeekFrom::End(off) => (off, SEEK_END),
            SeekFrom::Current(off) => (off, SEEK_CUR),
        };
        let out = unsafe { slopos_lseek(self.fd(), offset, whence) };
        Ok(cvt_i64(out)? as u64)
    }

    pub fn size(&self) -> Option<io::Result<u64>> {
        Some(self.file_attr().map(|a| a.size()))
    }

    pub fn tell(&self) -> io::Result<u64> {
        let out = unsafe { slopos_lseek(self.fd(), 0, SEEK_CUR) };
        Ok(cvt_i64(out)? as u64)
    }

    pub fn duplicate(&self) -> io::Result<File> {
        let fd = unsafe { slopos_dup(self.fd()) };
        let fd = cvt_i32(fd)?;
        Ok(File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) }))
    }

    pub fn set_permissions(&self, _perm: FilePermissions) -> io::Result<()> {
        unsupported()
    }

    pub fn set_times(&self, _times: FileTimes) -> io::Result<()> {
        unsupported()
    }
}

// FileDesc (via OwnedFd) handles close on drop — no explicit Drop needed.

impl DirBuilder {
    pub fn new() -> DirBuilder {
        DirBuilder { mode: 0o777 }
    }

    pub fn mkdir(&self, p: &Path) -> io::Result<()> {
        let cpath = path_to_cstr(p)?;
        let rc = unsafe { slopos_mkdir(cpath.as_ptr(), self.mode) };
        cvt_i32(rc).map(|_| ())
    }
}

impl fmt::Debug for File {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("File").field("fd", &self.fd()).finish()
    }
}

impl AsInner<crate::sys::fd::FileDesc> for File {
    fn as_inner(&self) -> &crate::sys::fd::FileDesc {
        &self.0
    }
}

impl IntoInner<crate::sys::fd::FileDesc> for File {
    fn into_inner(self) -> crate::sys::fd::FileDesc {
        self.0
    }
}

impl FromInner<crate::sys::fd::FileDesc> for File {
    fn from_inner(fd: crate::sys::fd::FileDesc) -> Self {
        File(fd)
    }
}

impl AsFd for File {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

impl AsRawFd for File {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}

impl IntoRawFd for File {
    fn into_raw_fd(self) -> RawFd {
        self.0.into_raw_fd()
    }
}

impl FromRawFd for File {
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        File(unsafe { crate::sys::fd::FileDesc::from_raw_fd(fd) })
    }
}

pub fn readdir(p: &Path) -> io::Result<ReadDir> {
    let cpath = path_to_cstr(p)?;
    let mut cap = 4096usize;

    loop {
        let mut buf = vec![0u8; cap];
        let rc = unsafe { slopos_list(cpath.as_ptr(), buf.as_mut_ptr(), buf.len()) };

        if rc < 0 {
            let err = (-rc) as i32;
            if err == 34 && cap < (1 << 20) {
                cap *= 2;
                continue;
            }
            return Err(Error::from_raw_os_error(err));
        }

        let used = rc as usize;
        buf.truncate(used);

        let mut names = Vec::new();
        for part in buf.split(|b| *b == b'\n') {
            if part.is_empty() || part == b"." || part == b".." {
                continue;
            }
            names.push(os_string_from_bytes_lossy(part));
        }

        return Ok(ReadDir {
            root: p.to_path_buf(),
            names,
            pos: 0,
        });
    }
}

pub fn unlink(p: &Path) -> io::Result<()> {
    let cpath = path_to_cstr(p)?;
    let rc = unsafe { slopos_unlink(cpath.as_ptr()) };
    cvt_i32(rc).map(|_| ())
}

pub fn rename(old: &Path, new: &Path) -> io::Result<()> {
    let cold = path_to_cstr(old)?;
    let cnew = path_to_cstr(new)?;
    let rc = unsafe { slopos_rename(cold.as_ptr(), cnew.as_ptr()) };
    cvt_i32(rc).map(|_| ())
}

pub fn set_perm(_p: &Path, _perm: FilePermissions) -> io::Result<()> {
    unsupported()
}

pub fn set_times(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn set_times_nofollow(_p: &Path, _times: FileTimes) -> io::Result<()> {
    unsupported()
}

pub fn rmdir(p: &Path) -> io::Result<()> {
    unlink(p)
}

pub fn remove_dir_all(path: &Path) -> io::Result<()> {
    for entry_res in readdir(path)? {
        let entry = entry_res?;
        let child = entry.path();
        let ty = entry.file_type()?;
        if ty.is_dir() {
            remove_dir_all(&child)?;
        } else {
            unlink(&child)?;
        }
    }
    rmdir(path)
}

pub fn exists(path: &Path) -> io::Result<bool> {
    match stat(path) {
        Ok(_) => Ok(true),
        Err(e) if e.raw_os_error() == Some(ENOENT) => Ok(false),
        Err(e) => Err(e),
    }
}

pub fn readlink(_p: &Path) -> io::Result<PathBuf> {
    unsupported()
}

pub fn symlink(_original: &Path, _link: &Path) -> io::Result<()> {
    unsupported()
}

pub fn link(_src: &Path, _dst: &Path) -> io::Result<()> {
    unsupported()
}

pub fn stat(p: &Path) -> io::Result<FileAttr> {
    Ok(FileAttr {
        stat: stat_from_path(p)?,
    })
}

pub fn lstat(p: &Path) -> io::Result<FileAttr> {
    stat(p)
}

pub fn canonicalize(p: &Path) -> io::Result<PathBuf> {
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        Path::new("/").join(p)
    };
    let normalized = normalize_absolute(&abs);
    stat(&normalized)?;
    Ok(normalized)
}

pub fn copy(from: &Path, to: &Path) -> io::Result<u64> {
    let mut from_opts = OpenOptions::new();
    from_opts.read(true);
    let src = File::open(from, &from_opts)?;

    let mut to_opts = OpenOptions::new();
    to_opts.write(true);
    to_opts.create(true);
    to_opts.truncate(true);
    let dst = File::open(to, &to_opts)?;

    let mut total = 0u64;
    let mut buf = [0u8; 8192];

    loop {
        let n = src.read(&mut buf)?;
        if n == 0 {
            break;
        }

        let mut written = 0usize;
        while written < n {
            let m = dst.write(&buf[written..n])?;
            if m == 0 {
                return Err(io::const_error!(
                    ErrorKind::WriteZero,
                    "failed to write whole buffer"
                ));
            }
            written += m;
        }

        total = total.saturating_add(n as u64);
    }

    Ok(total)
}
