use crate::vfs::canon::canonicalise;
use crate::vfs::mount::mount_at;
use crate::vfs::traits::{FileSystem, FileType, InodeId, VfsError, VfsResult};

pub struct ResolvedPath {
    pub fs: &'static dyn FileSystem,
    pub inode: InodeId,
}

pub fn resolve_path(path: &[u8]) -> VfsResult<ResolvedPath> {
    let canon = canonicalise(path)?;
    resolve_canonical(canon.as_bytes())
}

/// Walk an already-canonical path, re-resolving the mount table at each
/// directory component.
///
/// Selecting a filesystem once up front means a mount point crossed mid-walk
/// is never honoured: the walk continues inside the covered directory of the
/// filesystem underneath.
fn resolve_canonical(path: &[u8]) -> VfsResult<ResolvedPath> {
    let mut fs = mount_at(b"/").ok_or(VfsError::NotFound)?;
    let mut current_inode = fs.root_inode();

    // Byte offset in `path` of the end of the components consumed so far,
    // which is what the mount table is keyed on.
    let mut prefix_end = 0usize;

    for component in PathComponents::new(path) {
        prefix_end += 1 + component.len();

        // `canonicalise` has already resolved `.` and `..` lexically, so a
        // component here is always a real name.
        if let Some(mounted) = mount_at(&path[..prefix_end]) {
            fs = mounted;
            current_inode = fs.root_inode();
            continue;
        }

        current_inode = fs.lookup(current_inode, component)?;
    }

    Ok(ResolvedPath {
        fs,
        inode: current_inode,
    })
}

pub fn resolve_parent(path: &[u8]) -> VfsResult<(ResolvedPath, &[u8])> {
    let canon = canonicalise(path)?;
    let canon_bytes = canon.as_bytes();

    let (parent_path, name) = split_path(canon_bytes).ok_or(VfsError::InvalidPath)?;
    let resolved = resolve_canonical(parent_path)?;

    let stat = resolved.fs.stat(resolved.inode)?;
    if stat.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }

    // The name is returned as a slice of the caller's path, so the canonical
    // buffer need not outlive this call. Canonicalisation never rewrites the
    // final component of a path that has one.
    let name_in_input = tail_component(path, name)?;
    Ok((resolved, name_in_input))
}

/// Locate `name` as a suffix of the caller's own `path` buffer.
fn tail_component<'a>(path: &'a [u8], name: &[u8]) -> VfsResult<&'a [u8]> {
    if name.len() > path.len() {
        return Err(VfsError::InvalidPath);
    }
    let start = path.len() - name.len();
    if &path[start..] == name {
        return Ok(&path[start..]);
    }
    // A trailing slash was trimmed during canonicalisation.
    let trimmed = {
        let mut end = path.len();
        while end > 1 && path[end - 1] == b'/' {
            end -= 1;
        }
        &path[..end]
    };
    if name.len() <= trimmed.len() {
        let start = trimmed.len() - name.len();
        if &trimmed[start..] == name {
            return Ok(&trimmed[start..]);
        }
    }
    Err(VfsError::InvalidPath)
}

fn split_path(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return None;
    }

    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }

    if end <= 1 {
        return None;
    }

    let trimmed = &path[..end];

    let mut idx = trimmed.len();
    while idx > 0 && trimmed[idx - 1] != b'/' {
        idx -= 1;
    }

    if idx == 0 {
        return None;
    }

    let parent = if idx == 1 {
        &trimmed[..1]
    } else {
        &trimmed[..idx - 1]
    };
    let name = &trimmed[idx..];

    if name.is_empty() {
        return None;
    }

    Some((parent, name))
}

struct PathComponents<'a> {
    remaining: &'a [u8],
}

impl<'a> PathComponents<'a> {
    fn new(path: &'a [u8]) -> Self {
        let start = if !path.is_empty() && path[0] == b'/' {
            1
        } else {
            0
        };
        Self {
            remaining: &path[start..],
        }
    }
}

impl<'a> Iterator for PathComponents<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<Self::Item> {
        while !self.remaining.is_empty() && self.remaining[0] == b'/' {
            self.remaining = &self.remaining[1..];
        }

        if self.remaining.is_empty() {
            return None;
        }

        let end = self
            .remaining
            .iter()
            .position(|&c| c == b'/')
            .unwrap_or(self.remaining.len());

        let component = &self.remaining[..end];
        self.remaining = &self.remaining[end..];

        Some(component)
    }
}
