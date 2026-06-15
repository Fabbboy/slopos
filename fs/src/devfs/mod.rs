use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};

const ROOT_INODE: InodeId = 1;
const NULL_INODE: InodeId = 2;
const ZERO_INODE: InodeId = 3;
const RANDOM_INODE: InodeId = 4;
const CONSOLE_INODE: InodeId = 5;
const KMSG_INODE: InodeId = 6;

use crate::MAX_NAME_LEN;

struct DeviceEntry {
    name: [u8; MAX_NAME_LEN],
    name_len: usize,
    inode: InodeId,
    major: u32,
    minor: u32,
}

impl DeviceEntry {
    const fn new(name: &[u8], inode: InodeId, major: u32, minor: u32) -> Self {
        let mut entry = Self {
            name: [0; MAX_NAME_LEN],
            name_len: 0,
            inode,
            major,
            minor,
        };
        let len = if name.len() < MAX_NAME_LEN {
            name.len()
        } else {
            MAX_NAME_LEN
        };
        let mut i = 0;
        while i < len {
            entry.name[i] = name[i];
            i += 1;
        }
        entry.name_len = len;
        entry
    }
}

static DEVICES: [DeviceEntry; 5] = [
    DeviceEntry::new(b"null", NULL_INODE, 1, 3),
    DeviceEntry::new(b"zero", ZERO_INODE, 1, 5),
    DeviceEntry::new(b"random", RANDOM_INODE, 1, 8),
    DeviceEntry::new(b"console", CONSOLE_INODE, 5, 1),
    // Kernel log ring buffer, readable from userland (major 1, minor 11).
    DeviceEntry::new(b"kmsg", KMSG_INODE, 1, 11),
];

pub struct DevFs;

impl DevFs {
    pub const fn new() -> Self {
        Self
    }
}

impl Default for DevFs {
    fn default() -> Self {
        Self::new()
    }
}

impl FileSystem for DevFs {
    fn name(&self) -> &'static str {
        "devfs"
    }

    fn root_inode(&self) -> InodeId {
        ROOT_INODE
    }

    fn lookup(&self, parent: InodeId, name: &[u8]) -> VfsResult<InodeId> {
        if parent != ROOT_INODE {
            return Err(VfsError::NotDirectory);
        }

        if name == b"." || name == b".." {
            return Ok(ROOT_INODE);
        }

        for dev in &DEVICES {
            if dev.name_len == name.len() && &dev.name[..dev.name_len] == name {
                return Ok(dev.inode);
            }
        }

        Err(VfsError::NotFound)
    }

    fn stat(&self, inode: InodeId) -> VfsResult<FileStat> {
        if inode == ROOT_INODE {
            return Ok(FileStat::new_directory(ROOT_INODE));
        }

        for dev in &DEVICES {
            if dev.inode == inode {
                return Ok(FileStat::new_char_device(inode, dev.major, dev.minor));
            }
        }

        Err(VfsError::NotFound)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        match inode {
            NULL_INODE => Ok(0),

            // Kernel log: serve the in-memory ring buffer by offset so a
            // plain `cat /dev/kmsg` streams the whole log to EOF.
            KMSG_INODE => Ok(slopos_ostd::klog::klog_read(offset as usize, buf)),

            ZERO_INODE => {
                buf.fill(0);
                Ok(buf.len())
            }

            RANDOM_INODE => {
                // Reads from the kernel CSPRNG via the platform service.
                let mut pos = 0;
                while pos < buf.len() {
                    let val = slopos_kernel_services::platform::rng_next();
                    let bytes = val.to_le_bytes();
                    let chunk = (buf.len() - pos).min(8);
                    buf[pos..pos + chunk].copy_from_slice(&bytes[..chunk]);
                    pos += chunk;
                }
                Ok(pos)
            }

            CONSOLE_INODE => Ok(0),

            ROOT_INODE => Err(VfsError::IsDirectory),

            _ => Err(VfsError::NotFound),
        }
    }

    fn write(&self, inode: InodeId, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        match inode {
            // kmsg is read-only from userland; accept+discard writes so a
            // stray redirect doesn't error.
            NULL_INODE | ZERO_INODE | KMSG_INODE => Ok(buf.len()),

            // Accept and discard writes to /dev/random (entropy injection is
            // not meaningful with a properly-seeded CSPRNG).
            RANDOM_INODE => Ok(buf.len()),

            CONSOLE_INODE => Ok(buf.len()),

            ROOT_INODE => Err(VfsError::IsDirectory),

            _ => Err(VfsError::NotFound),
        }
    }

    fn create(&self, _parent: InodeId, _name: &[u8], _file_type: FileType) -> VfsResult<InodeId> {
        Err(VfsError::ReadOnly)
    }

    fn unlink(&self, _parent: InodeId, _name: &[u8]) -> VfsResult<()> {
        Err(VfsError::ReadOnly)
    }

    fn readdir(
        &self,
        inode: InodeId,
        offset: usize,
        callback: &mut dyn FnMut(&[u8], InodeId, FileType) -> bool,
    ) -> VfsResult<usize> {
        if inode != ROOT_INODE {
            return Err(VfsError::NotDirectory);
        }

        let mut count = 0;
        let mut current = 0;

        if current >= offset {
            if !callback(b".", ROOT_INODE, FileType::Directory) {
                return Ok(count);
            }
            count += 1;
        }
        current += 1;

        if current >= offset {
            if !callback(b"..", ROOT_INODE, FileType::Directory) {
                return Ok(count);
            }
            count += 1;
        }
        current += 1;

        for dev in &DEVICES {
            if current >= offset {
                if !callback(&dev.name[..dev.name_len], dev.inode, FileType::CharDevice) {
                    return Ok(count);
                }
                count += 1;
            }
            current += 1;
        }

        Ok(count)
    }

    fn truncate(&self, _inode: InodeId, _size: u64) -> VfsResult<()> {
        Err(VfsError::NotSupported)
    }

    fn sync(&self) -> VfsResult<()> {
        Ok(())
    }
}
