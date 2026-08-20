//! Lexical path canonicalisation.
//!
//! Mount resolution is a prefix match against the mount table, so it must run
//! on a normalised path: `//tmp/x`, `/./tmp/x` and `/a/../tmp/x` all name
//! `/tmp/x` but none of them share its byte prefix, and each would otherwise
//! miss the `/tmp` mount and land on the root filesystem's shadowed directory.

use crate::MAX_PATH_LEN;
use crate::vfs::traits::{VfsError, VfsResult};

/// A canonicalised absolute path, owned by value so it can outlive the caller's
/// borrow of the original.
#[derive(Clone, Copy)]
pub struct CanonPath {
    buf: [u8; MAX_PATH_LEN],
    len: usize,
}

impl CanonPath {
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

/// Collapse `//`, drop `.`, and resolve `..` lexically against the path root.
///
/// `..` above the root is absorbed, as POSIX requires of an absolute path.
/// Rejects a relative path and one that does not fit the fixed buffer.
pub fn canonicalise(path: &[u8]) -> VfsResult<CanonPath> {
    if path.is_empty() || path[0] != b'/' {
        return Err(VfsError::InvalidPath);
    }
    if path.len() > MAX_PATH_LEN {
        return Err(VfsError::NameTooLong);
    }

    let mut out = CanonPath {
        buf: [0; MAX_PATH_LEN],
        len: 0,
    };
    // Component start offsets in `out.buf`, so `..` can pop the last one.
    // A component costs at least two bytes (separator + one character), so
    // half the buffer bounds the count. `u16` rather than `usize`: the array
    // is a stack frame the 2 KiB kernel budget has to hold.
    let mut starts = [0u16; MAX_PATH_LEN / 2];
    let mut depth = 0usize;

    out.buf[0] = b'/';
    out.len = 1;

    for component in path.split(|&c| c == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            if depth > 0 {
                depth -= 1;
                out.len = starts[depth] as usize;
            }
            continue;
        }
        if component.len() > MAX_PATH_LEN {
            return Err(VfsError::NameTooLong);
        }
        if depth >= starts.len() {
            return Err(VfsError::NameTooLong);
        }

        // Every component but the first is preceded by a separator; the root
        // slash already sits at offset 0.
        let sep = if out.len > 1 { 1 } else { 0 };
        if out.len + sep + component.len() > MAX_PATH_LEN {
            return Err(VfsError::NameTooLong);
        }
        starts[depth] = out.len as u16;
        depth += 1;
        if sep == 1 {
            out.buf[out.len] = b'/';
            out.len += 1;
        }
        out.buf[out.len..out.len + component.len()].copy_from_slice(component);
        out.len += component.len();
    }

    Ok(out)
}
