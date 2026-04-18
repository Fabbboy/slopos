/// Physical block number on the block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct BlockNum(pub u32);

// SAFETY: BlockNum wraps a single u32; all-zero bits encode the
// `BlockNum::ZERO` sentinel. No invalid bit patterns.
unsafe impl slopos_alloc::Zeroable for BlockNum {}

/// Logical file block number (offset within a file's data).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct FileBlock(pub u32);

/// Inode number (1-indexed, 0 is invalid).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct InodeNum(pub u32);

/// Block group index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct GroupIdx(pub u32);

/// Byte offset on the block device.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct DiskOffset(pub u64);

impl BlockNum {
    pub const ZERO: Self = Self(0);

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn raw(self) -> u32 {
        self.0
    }

    pub fn to_disk_offset(self, block_size: u32) -> DiskOffset {
        DiskOffset(self.0 as u64 * block_size as u64)
    }
}

impl FileBlock {
    pub fn raw(self) -> u32 {
        self.0
    }
}

impl InodeNum {
    pub const ROOT: Self = Self(2);

    pub fn raw(self) -> u32 {
        self.0
    }

    pub fn is_valid(self) -> bool {
        self.0 != 0
    }

    pub fn block_group(self, inodes_per_group: u32) -> GroupIdx {
        GroupIdx((self.0 - 1) / inodes_per_group)
    }

    pub fn local_index(self, inodes_per_group: u32) -> u32 {
        (self.0 - 1) % inodes_per_group
    }
}

impl GroupIdx {
    pub fn raw(self) -> u32 {
        self.0
    }
}

impl DiskOffset {
    pub fn raw(self) -> u64 {
        self.0
    }

    pub fn from_block(block: BlockNum, block_size: u32) -> Self {
        block.to_disk_offset(block_size)
    }
}
