//! Minimal `newc` cpio archive reader for the initramfs.
//!
//! Limine loads the initramfs as a module: a single uncompressed `newc`
//! (SVR4) cpio archive packed at build time by `scripts/gen_initramfs.py`.
//! At boot the kernel unpacks it into the RAM-backed root filesystem so the
//! whole userland (`/sbin/init`, `/bin/*`, fonts, …) is available with **no
//! storage drivers** — the boot path is identical in QEMU and on real
//! hardware.
//!
//! The archive is bootloader-supplied data, so every offset is computed with
//! `checked_add` and bounds-checked against the slice: a malformed archive
//! yields a [`CpioError`], never a panic or an out-of-bounds read.
//!
//! Parsing ([`for_each_cpio_entry`]) is kept free of side effects so it is
//! unit-testable in isolation; [`unpack_cpio_into_root`] is the thin wrapper
//! that materializes each entry into the VFS via [`vfs_mkdir`] /
//! [`vfs_open_flags`] / [`vfs_set_mode`].

use slopos_ostd::klog_info;

use crate::vfs::{VfsError, VfsOpenFlags, vfs_mkdir, vfs_open_flags, vfs_set_mode};
use crate::{MAX_NAME_LEN, MAX_PATH_LEN};

/// Fixed `newc` header size: 6-byte magic + 13 fields × 8 ASCII-hex chars.
const HEADER_LEN: usize = 110;
const MAGIC: &[u8] = b"070701";
/// Sentinel entry name that terminates the archive.
const TRAILER: &[u8] = b"TRAILER!!!";

/// `st_mode` type mask and the two types we materialize.
const S_IFMT: u32 = 0o170000;
const S_IFDIR: u32 = 0o040000;
const S_IFREG: u32 = 0o100000;

/// Field byte offsets within the header (each field is 8 ASCII-hex chars).
const OFF_MODE: usize = 6 + 8; // after magic + c_ino
const OFF_FILESIZE: usize = 6 + 8 * 6; // after c_nlink + c_mtime
const OFF_NAMESIZE: usize = 6 + 8 * 11;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpioError {
    /// The archive ended before a record (header/name/data) was complete.
    Truncated,
    /// A record header did not start with the `070701` magic.
    BadMagic,
    /// A header field was not valid ASCII-hex.
    BadField,
    /// A path component exceeded [`MAX_NAME_LEN`] (or the path [`MAX_PATH_LEN`]).
    NameTooLong,
    /// A VFS operation failed while materializing an entry.
    Vfs(VfsError),
}

/// One decoded archive entry. `path` and `data` borrow into the archive.
pub struct CpioEntry<'a> {
    pub path: &'a [u8],
    pub mode: u32,
    pub data: &'a [u8],
}

const fn align4(n: usize) -> Option<usize> {
    match n.checked_add(3) {
        Some(x) => Some(x & !3),
        None => None,
    }
}

/// Parse an 8-character ASCII-hex field. The caller always passes an 8-byte
/// slice, so the result fits in `u32`.
fn parse_hex8(field: &[u8]) -> Result<u32, CpioError> {
    let mut val: u32 = 0;
    for &b in field {
        let digit = match b {
            b'0'..=b'9' => b - b'0',
            b'a'..=b'f' => b - b'a' + 10,
            b'A'..=b'F' => b - b'A' + 10,
            _ => return Err(CpioError::BadField),
        };
        val = (val << 4) | digit as u32;
    }
    Ok(val)
}

/// The stored name is NUL-terminated within its `namesize` field; return the
/// bytes up to the first NUL.
fn nul_terminated(bytes: &[u8]) -> &[u8] {
    match bytes.iter().position(|&b| b == 0) {
        Some(i) => &bytes[..i],
        None => bytes,
    }
}

/// Walk every record of a `newc` cpio archive, invoking `f` for each entry up
/// to (but not including) the `TRAILER!!!` sentinel. Returns the number of
/// entries visited. Pure with respect to the VFS — used both by
/// [`unpack_cpio_into_root`] and by the unit tests.
pub fn for_each_cpio_entry<F>(archive: &[u8], mut f: F) -> Result<usize, CpioError>
where
    F: FnMut(&CpioEntry) -> Result<(), CpioError>,
{
    let mut pos = 0usize;
    let mut count = 0usize;

    loop {
        let Some(record) = parse_record(archive, pos)? else {
            return Ok(count);
        };
        f(&record.entry)?;
        count += 1;
        pos = pos
            .checked_add(record.advance)
            .ok_or(CpioError::Truncated)?;
    }
}

struct CpioRecord<'a> {
    entry: CpioEntry<'a>,
    advance: usize,
}

/// Decode one `newc` record, or `None` at the `TRAILER!!!` sentinel.
///
/// Out of line: the dozen `checked_*(…)?` chains are a stack slot each at
/// opt-level 0, and inlined they were charged to every instantiation of the
/// generic, putting all of them over the frame ceiling.
#[inline(never)]
fn parse_record(archive: &[u8], pos: usize) -> Result<Option<CpioRecord<'_>>, CpioError> {
    let header_end = pos.checked_add(HEADER_LEN).ok_or(CpioError::Truncated)?;
    if header_end > archive.len() {
        return Err(CpioError::Truncated);
    }
    let header = &archive[pos..header_end];
    if header[0..6] != *MAGIC {
        return Err(CpioError::BadMagic);
    }

    let mode = parse_hex8(&header[OFF_MODE..OFF_MODE + 8])?;
    let filesize = parse_hex8(&header[OFF_FILESIZE..OFF_FILESIZE + 8])? as usize;
    let namesize = parse_hex8(&header[OFF_NAMESIZE..OFF_NAMESIZE + 8])? as usize;
    if namesize == 0 {
        return Err(CpioError::BadField);
    }

    let name_end = header_end
        .checked_add(namesize)
        .ok_or(CpioError::Truncated)?;
    if name_end > archive.len() {
        return Err(CpioError::Truncated);
    }
    let name = nul_terminated(&archive[header_end..name_end]);

    if name == TRAILER {
        return Ok(None);
    }

    // File data starts after the header+name, padded (from the record
    // start) to a 4-byte boundary; the data itself is padded likewise.
    let after_name = HEADER_LEN
        .checked_add(namesize)
        .ok_or(CpioError::Truncated)?;
    let data_rel = align4(after_name).ok_or(CpioError::Truncated)?;
    let data_start = pos.checked_add(data_rel).ok_or(CpioError::Truncated)?;
    let data_end = data_start
        .checked_add(filesize)
        .ok_or(CpioError::Truncated)?;
    if data_end > archive.len() {
        return Err(CpioError::Truncated);
    }
    let data = &archive[data_start..data_end];

    let padded = align4(filesize).ok_or(CpioError::Truncated)?;
    let advance = data_rel.checked_add(padded).ok_or(CpioError::Truncated)?;

    Ok(Some(CpioRecord {
        entry: CpioEntry {
            path: name,
            mode,
            data,
        },
        advance,
    }))
}

/// Unpack a `newc` cpio archive into the currently mounted root filesystem,
/// creating directories and files via the VFS. Returns the number of entries
/// materialized (directories + regular files; other types are skipped).
pub fn unpack_cpio_into_root(archive: &[u8]) -> Result<usize, CpioError> {
    let mut created = 0usize;

    for_each_cpio_entry(archive, |entry| {
        let mut buf = [0u8; MAX_PATH_LEN];
        let path = match normalize_path(entry.path, &mut buf)? {
            Some(len) => &buf[..len],
            None => return Ok(()), // root / "." — nothing to materialize
        };
        validate_components(path)?;

        match entry.mode & S_IFMT {
            S_IFDIR => {
                ensure_parents(path)?;
                mkdir_ignore_exists(path)?;
                created += 1;
            }
            S_IFREG => {
                ensure_parents(path)?;
                write_file(path, entry.data, entry.mode)?;
                created += 1;
            }
            other => {
                klog_info!("initramfs: skipping unsupported entry (mode {:#o})", other);
            }
        }
        Ok(())
    })
    .map(|_| created)
}

/// Normalize a cpio entry name into an absolute VFS path written into `out`.
/// Strips a leading `./` and collapses leading slashes to exactly one. Returns
/// the byte length written, `None` for the root / `.`, or [`CpioError::NameTooLong`].
fn normalize_path(name: &[u8], out: &mut [u8; MAX_PATH_LEN]) -> Result<Option<usize>, CpioError> {
    let mut n = name;
    // Strip a leading "." or "./" (gen_init_cpio convention).
    if n == b"." {
        return Ok(None);
    }
    if n.len() >= 2 && n[0] == b'.' && n[1] == b'/' {
        n = &n[2..];
    }
    // Collapse leading slashes; we re-add exactly one.
    while n.first() == Some(&b'/') {
        n = &n[1..];
    }
    if n.is_empty() {
        return Ok(None);
    }
    let total = n.len().checked_add(1).ok_or(CpioError::NameTooLong)?;
    if total > MAX_PATH_LEN {
        return Err(CpioError::NameTooLong);
    }
    out[0] = b'/';
    out[1..total].copy_from_slice(n);
    Ok(Some(total))
}

/// Verify every path component fits in [`MAX_NAME_LEN`] so the VFS does not
/// silently truncate a name (which would corrupt the directory tree).
fn validate_components(path: &[u8]) -> Result<(), CpioError> {
    let mut comp_len = 0usize;
    for &b in &path[1..] {
        if b == b'/' {
            if comp_len > MAX_NAME_LEN {
                return Err(CpioError::NameTooLong);
            }
            comp_len = 0;
        } else {
            comp_len += 1;
        }
    }
    if comp_len > MAX_NAME_LEN {
        return Err(CpioError::NameTooLong);
    }
    Ok(())
}

/// Create every intermediate directory of an absolute path (not the final
/// component), tolerating directories that already exist.
fn ensure_parents(path: &[u8]) -> Result<(), CpioError> {
    let mut i = 1;
    while i < path.len() {
        if path[i] == b'/' {
            mkdir_ignore_exists(&path[..i])?;
        }
        i += 1;
    }
    Ok(())
}

fn mkdir_ignore_exists(path: &[u8]) -> Result<(), CpioError> {
    match vfs_mkdir(path) {
        Ok(()) | Err(VfsError::AlreadyExists) => Ok(()),
        Err(e) => Err(CpioError::Vfs(e)),
    }
}

fn write_file(path: &[u8], data: &[u8], mode: u32) -> Result<(), CpioError> {
    let handle = vfs_open_flags(
        path,
        VfsOpenFlags {
            create: true,
            exclusive: false,
            truncate: true,
            writable: true,
        },
    )
    .map_err(CpioError::Vfs)?;

    let mut off = 0usize;
    while off < data.len() {
        let n = handle
            .write(off as u64, &data[off..])
            .map_err(CpioError::Vfs)?;
        if n == 0 {
            return Err(CpioError::Vfs(VfsError::NoSpace));
        }
        off += n;
    }

    // Restore the permission bits (notably the exec bit on binaries; the VFS
    // create path defaults regular files to 0o644).
    vfs_set_mode(path, (mode & 0o7777) as u16).map_err(CpioError::Vfs)?;
    Ok(())
}
