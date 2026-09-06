use slopos_ostd::sync::{IrqRwLock, LOCK_LEVEL_REGISTRY};
use slopos_ostd::{KArc, KVec, klog_info, lock_class};

use crate::MAX_NAME_LEN;
use crate::blockdev::BlockDevice;
use crate::vfs::{FileStat, FileSystem, FileType, InodeId, VfsError, VfsResult};
use slopos_kernel_services::driver_runtime::current_task_is_privileged;

const ROOT_INODE: InodeId = 1;
const NULL_INODE: InodeId = 2;
const ZERO_INODE: InodeId = 3;
const RANDOM_INODE: InodeId = 4;
const CONSOLE_INODE: InodeId = 5;
const KMSG_INODE: InodeId = 6;

/// Runtime-registered block nodes number from a well clear of the const
/// character-device ids above.
const BLOCK_INODE_BASE: InodeId = 64;
const MAX_BLOCK_NODES: usize = 16;

/// Linux's `virtblk` major; the minor is the registration ordinal.
const BLOCK_MAJOR: u32 = 254;

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
    DeviceEntry::new(b"kmsg", KMSG_INODE, 1, 11),
];

/// `capacity` is cached: `BlockDevice::capacity` takes the device's own lock
/// and `stat` answers it on every call.
struct BlockNode {
    name: [u8; MAX_NAME_LEN],
    name_len: usize,
    inode: InodeId,
    device: KArc<dyn BlockDevice + Send + Sync>,
    capacity: u64,
}

/// Append-only for the lifetime of the kernel, which is what lets `readdir`
/// walk these after [`DEVICES`] under the trait-default `readdir_cookie`.
static BLOCK_NODES: IrqRwLock<KVec<BlockNode>> = IrqRwLock::new(
    KVec::new(),
    lock_class!("DEVFS_BLOCK_NODES", LOCK_LEVEL_REGISTRY),
);

/// Publish `device` as `/dev/<name>`; `name` must be unique. The caller keeps
/// its own `KArc` clone, so one claim can back both a mount and this node.
pub fn devfs_register_block_device(
    name: &[u8],
    device: KArc<dyn BlockDevice + Send + Sync>,
) -> VfsResult<InodeId> {
    if name.is_empty() || name.len() > MAX_NAME_LEN {
        return Err(VfsError::InvalidArgument);
    }
    // Outside the registry lock: BLOCK_NODES must never nest over the device
    // state lock `capacity()` takes.
    let capacity = device.capacity();

    let mut table = BLOCK_NODES.write();
    if table.len() >= MAX_BLOCK_NODES {
        return Err(VfsError::NoSpace);
    }
    if table
        .iter()
        .any(|n| n.name_len == name.len() && &n.name[..n.name_len] == name)
    {
        return Err(VfsError::AlreadyExists);
    }
    let inode = BLOCK_INODE_BASE + table.len() as InodeId;
    let mut stored = [0u8; MAX_NAME_LEN];
    stored[..name.len()].copy_from_slice(name);
    table
        .push(BlockNode {
            name: stored,
            name_len: name.len(),
            inode,
            device,
            capacity,
        })
        .map_err(|_| VfsError::NoSpace)?;
    drop(table);

    klog_info!(
        "DEVFS: registered block node inode {} ({} bytes)",
        inode,
        capacity
    );
    Ok(inode)
}

fn block_inode_for(name: &[u8]) -> Option<InodeId> {
    let table = BLOCK_NODES.read();
    table
        .iter()
        .find(|n| n.name_len == name.len() && &n.name[..n.name_len] == name)
        .map(|n| n.inode)
}

/// Copied out so the caller can run the `readdir` callback with the registry
/// lock released.
fn block_node_at(index: usize) -> Option<([u8; MAX_NAME_LEN], usize, InodeId)> {
    let table = BLOCK_NODES.read();
    let node = table.get(index)?;
    Some((node.name, node.name_len, node.inode))
}

fn block_capacity_of(inode: InodeId) -> Option<u64> {
    let table = BLOCK_NODES.read();
    table.iter().find(|n| n.inode == inode).map(|n| n.capacity)
}

/// Clones the device out from under the registry lock: the read that follows
/// goes to the driver and must not hold an IRQ-off spinlock.
fn block_device_of(inode: InodeId) -> Option<(KArc<dyn BlockDevice + Send + Sync>, u64)> {
    let table = BLOCK_NODES.read();
    table
        .iter()
        .find(|n| n.inode == inode)
        .map(|n| (KArc::clone(&n.device), n.capacity))
}

/// Serve bytes from a registered block device.
///
/// Requires the entitlement the ext2 block reserve asks for (a kernel thread
/// or `TASK_FLAG_SYSTEM`), else [`VfsError::PermissionDenied`]: a raw read
/// bypasses every filesystem permission check above it, and ext2 does not zero
/// a block it frees, so an unprivileged reader could recover the contents of
/// any unlinked file.
///
/// A short read is EOF to the VFS, so this shortens only at the end of the
/// device.
fn block_read(inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
    block_read_entitled(inode, offset, buf, current_task_is_privileged())
}

/// `entitled` is [`current_task_is_privileged`] on every production path; it
/// is a parameter only so a test can reach the refusal, which a kernel thread
/// cannot otherwise do.
pub(crate) fn block_read_entitled(
    inode: InodeId,
    offset: u64,
    buf: &mut [u8],
    entitled: bool,
) -> VfsResult<usize> {
    let Some((device, capacity)) = block_device_of(inode) else {
        return Err(VfsError::NotFound);
    };
    if !entitled {
        return Err(VfsError::PermissionDenied);
    }
    if offset >= capacity || buf.is_empty() {
        return Ok(0);
    }
    let want = (capacity - offset).min(buf.len() as u64) as usize;
    device
        .read_at(offset, &mut buf[..want])
        .map_err(|_| VfsError::IoError)?;
    Ok(want)
}

/// Not a ZST deliberately: filesystem identity is the address of the `static`
/// (`vfs::traits::same_filesystem`), and Rust does not promise two distinct
/// zero-sized statics distinct addresses.
pub struct DevFs(#[expect(dead_code, reason = "gives the static an address of its own")] u8);

impl DevFs {
    pub const fn new() -> Self {
        Self(0)
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

        block_inode_for(name).ok_or(VfsError::NotFound)
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

        if let Some(capacity) = block_capacity_of(inode) {
            return Ok(FileStat::new_block_device(
                inode,
                capacity,
                BLOCK_MAJOR,
                (inode - BLOCK_INODE_BASE) as u32,
            ));
        }

        Err(VfsError::NotFound)
    }

    fn read(&self, inode: InodeId, offset: u64, buf: &mut [u8]) -> VfsResult<usize> {
        match inode {
            NULL_INODE => Ok(0),

            // Served by offset so a plain `cat /dev/kmsg` streams to EOF.
            KMSG_INODE => Ok(slopos_ostd::klog::klog_read(offset as usize, buf)),

            ZERO_INODE => {
                buf.fill(0);
                Ok(buf.len())
            }

            RANDOM_INODE => {
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

            _ => block_read(inode, offset, buf),
        }
    }

    fn write(&self, inode: InodeId, _offset: u64, buf: &[u8]) -> VfsResult<usize> {
        match inode {
            // kmsg is read-only; writes are discarded so a stray redirect does
            // not error.
            NULL_INODE | ZERO_INODE | KMSG_INODE => Ok(buf.len()),

            // Entropy injection is not meaningful with a seeded CSPRNG.
            RANDOM_INODE => Ok(buf.len()),

            CONSOLE_INODE => Ok(buf.len()),

            ROOT_INODE => Err(VfsError::IsDirectory),

            // The mount holds the device's exclusive write capability for the
            // kernel's lifetime, and a device-level write behind the ext2
            // block cache would race its own writeback.
            _ if block_capacity_of(inode).is_some() => Err(VfsError::ReadOnly),

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

        // One row at a time, so the callback never runs with BLOCK_NODES held.
        let mut index = 0;
        while let Some((name, name_len, node_inode)) = block_node_at(index) {
            if current >= offset {
                if !callback(&name[..name_len], node_inode, FileType::BlockDevice) {
                    return Ok(count);
                }
                count += 1;
            }
            current += 1;
            index += 1;
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
