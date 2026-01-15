use crate::vfs::mount::with_mount_table;
use crate::vfs::traits::{FileSystem, FileType, InodeId, VfsError, VfsResult};

pub struct ResolvedPath {
    pub fs: &'static dyn FileSystem,
    pub inode: InodeId,
}

pub fn resolve_path(path: &[u8]) -> VfsResult<ResolvedPath> {
    if path.is_empty() || path[0] != b'/' {
        return Err(VfsError::InvalidPath);
    }

    with_mount_table(|mount_table| {
        let (fs, relative) = mount_table.resolve(path)?;

        let mut current_inode = fs.root_inode();

        for component in PathComponents::new(relative) {
            if component == b"." {
                continue;
            }

            if component == b".." {
                match fs.lookup(current_inode, b"..") {
                    Ok(parent) => current_inode = parent,
                    Err(_) => {}
                }
                continue;
            }

            current_inode = fs.lookup(current_inode, component)?;
        }

        let fs_static: &'static dyn FileSystem = unsafe { core::mem::transmute(fs) };

        Ok(ResolvedPath {
            fs: fs_static,
            inode: current_inode,
        })
    })
}

pub fn resolve_parent(path: &[u8]) -> VfsResult<(ResolvedPath, &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return Err(VfsError::InvalidPath);
    }

    let (parent_path, name) = split_path(path).ok_or(VfsError::InvalidPath)?;

    let resolved = resolve_path(parent_path)?;

    let stat = resolved.fs.stat(resolved.inode)?;
    if stat.file_type != FileType::Directory {
        return Err(VfsError::NotDirectory);
    }

    Ok((resolved, name))
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
