use super::Ext2Error;
use super::types::{BlockNum, InodeNum};

pub const EXT2_MAGIC: u16 = 0xEF53;
pub const EXT2_MIN_BLOCK_SIZE: u32 = 1024;
pub const EXT2_MAX_BLOCK_SIZE: u32 = 4096;
pub const EXT2_ROOT_INODE: InodeNum = InodeNum::ROOT;

pub const MODE_FIFO: u16 = 0x1000;
pub const MODE_CHARDEV: u16 = 0x2000;
pub const MODE_DIRECTORY: u16 = 0x4000;
pub const MODE_BLOCKDEV: u16 = 0x6000;
pub const MODE_FILE: u16 = 0x8000;
pub const MODE_SYMLINK: u16 = 0xA000;
pub const MODE_SOCKET: u16 = 0xC000;
pub const MODE_TYPE_MASK: u16 = 0xF000;

pub const DIR_FT_UNKNOWN: u8 = 0;
pub const DIR_FT_REG_FILE: u8 = 1;
pub const DIR_FT_DIR: u8 = 2;
pub const DIR_FT_CHRDEV: u8 = 3;
pub const DIR_FT_BLKDEV: u8 = 4;
pub const DIR_FT_FIFO: u8 = 5;
pub const DIR_FT_SOCK: u8 = 6;
pub const DIR_FT_SYMLINK: u8 = 7;

/// Maximum bytes storable inline in a fast symlink (i_block[0..14] = 60 bytes).
pub const FAST_SYMLINK_MAX: usize = 60;

// ---- Superblock ----

#[derive(Debug, Copy, Clone)]
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: BlockNum,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub magic: u16,
    pub rev_level: u32,
    pub first_ino: u32,
    pub inode_size: u16,
}

impl Superblock {
    pub fn parse(data: &[u8]) -> Result<Self, Ext2Error> {
        if data.len() < 1024 {
            return Err(Ext2Error::InvalidSuperblock);
        }
        let sb = Self {
            inodes_count: le32(data, 0),
            blocks_count: le32(data, 4),
            free_blocks_count: le32(data, 12),
            free_inodes_count: le32(data, 16),
            first_data_block: BlockNum(le32(data, 20)),
            log_block_size: le32(data, 24),
            blocks_per_group: le32(data, 32),
            inodes_per_group: le32(data, 40),
            magic: le16(data, 56),
            rev_level: le32(data, 76),
            first_ino: le32(data, 84),
            inode_size: le16(data, 88),
        };
        if sb.magic != EXT2_MAGIC {
            return Err(Ext2Error::InvalidSuperblock);
        }
        Ok(sb)
    }

    pub fn encode_free_counts(&self, data: &mut [u8]) {
        put_le32(data, 12, self.free_blocks_count);
        put_le32(data, 16, self.free_inodes_count);
    }

    pub fn block_size(&self) -> Result<u32, Ext2Error> {
        let size = EXT2_MIN_BLOCK_SIZE
            .checked_shl(self.log_block_size)
            .ok_or(Ext2Error::UnsupportedBlockSize)?;
        if size < EXT2_MIN_BLOCK_SIZE || size > EXT2_MAX_BLOCK_SIZE {
            return Err(Ext2Error::UnsupportedBlockSize);
        }
        Ok(size)
    }

    pub fn effective_inode_size(&self) -> u16 {
        if self.inode_size == 0 {
            128
        } else {
            self.inode_size
        }
    }

    pub fn groups_count(&self) -> u32 {
        if self.blocks_per_group == 0 {
            return 0;
        }
        (self.blocks_count + self.blocks_per_group - 1) / self.blocks_per_group
    }
}

// ---- Block Group Descriptor ----

#[derive(Debug, Copy, Clone)]
pub struct GroupDesc {
    pub block_bitmap: BlockNum,
    pub inode_bitmap: BlockNum,
    pub inode_table: BlockNum,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

impl GroupDesc {
    pub fn parse(data: &[u8]) -> Self {
        Self {
            block_bitmap: BlockNum(le32(data, 0)),
            inode_bitmap: BlockNum(le32(data, 4)),
            inode_table: BlockNum(le32(data, 8)),
            free_blocks_count: le16(data, 12),
            free_inodes_count: le16(data, 14),
            used_dirs_count: le16(data, 16),
        }
    }

    pub fn encode(&self, data: &mut [u8]) {
        put_le32(data, 0, self.block_bitmap.raw());
        put_le32(data, 4, self.inode_bitmap.raw());
        put_le32(data, 8, self.inode_table.raw());
        put_le16(data, 12, self.free_blocks_count);
        put_le16(data, 14, self.free_inodes_count);
        put_le16(data, 16, self.used_dirs_count);
    }
}

// ---- Inode ----

#[derive(Debug, Copy, Clone)]
pub struct Inode {
    pub mode: u16,
    pub uid: u16,
    pub size: u32,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub gid: u16,
    pub links_count: u16,
    pub blocks: u32,
    pub flags: u32,
    pub block: [BlockNum; 15],
}

impl Inode {
    pub fn parse(data: &[u8]) -> Self {
        let mut block = [BlockNum::ZERO; 15];
        let mut offset = 40usize;
        for slot in &mut block {
            *slot = BlockNum(le32(data, offset));
            offset += 4;
        }
        Self {
            mode: le16(data, 0),
            uid: le16(data, 2),
            size: le32(data, 4),
            atime: le32(data, 8),
            ctime: le32(data, 12),
            mtime: le32(data, 16),
            dtime: le32(data, 20),
            gid: le16(data, 24),
            links_count: le16(data, 26),
            blocks: le32(data, 28),
            flags: le32(data, 32),
            block,
        }
    }

    pub fn encode(&self, data: &mut [u8]) {
        for byte in data.iter_mut() {
            *byte = 0;
        }
        put_le16(data, 0, self.mode);
        put_le16(data, 2, self.uid);
        put_le32(data, 4, self.size);
        put_le32(data, 8, self.atime);
        put_le32(data, 12, self.ctime);
        put_le32(data, 16, self.mtime);
        put_le32(data, 20, self.dtime);
        put_le16(data, 24, self.gid);
        put_le16(data, 26, self.links_count);
        put_le32(data, 28, self.blocks);
        put_le32(data, 32, self.flags);
        let mut offset = 40usize;
        for blk in &self.block {
            put_le32(data, offset, blk.raw());
            offset += 4;
        }
    }

    pub fn file_type_mode(&self) -> u16 {
        self.mode & MODE_TYPE_MASK
    }

    pub fn is_directory(&self) -> bool {
        self.file_type_mode() == MODE_DIRECTORY
    }

    pub fn is_regular_file(&self) -> bool {
        self.file_type_mode() == MODE_FILE
    }

    pub fn is_symlink(&self) -> bool {
        self.file_type_mode() == MODE_SYMLINK
    }

    pub fn is_fast_symlink(&self) -> bool {
        self.is_symlink() && self.blocks == 0 && (self.size as usize) <= FAST_SYMLINK_MAX
    }
}

// ---- Directory Entry (borrowed) ----

#[derive(Debug, Copy, Clone)]
pub struct DirEntry<'a> {
    pub inode: InodeNum,
    pub file_type: u8,
    pub name: &'a [u8],
}

/// Minimum size of a directory entry record (header only, no name).
pub const DIR_ENTRY_HEADER_SIZE: usize = 8;

/// Compute the on-disk record size for a directory entry with the given name length.
pub fn dir_entry_size(name_len: usize) -> usize {
    (DIR_ENTRY_HEADER_SIZE + name_len + 3) & !3
}

/// Write a directory entry into `data`. Caller must ensure `data.len() >= rec_len`.
pub fn write_dir_entry(
    data: &mut [u8],
    inode: InodeNum,
    name: &[u8],
    file_type: u8,
    rec_len: usize,
) {
    put_le32(data, 0, inode.raw());
    put_le16(data, 4, rec_len as u16);
    data[6] = name.len() as u8;
    data[7] = file_type;
    for byte in data[DIR_ENTRY_HEADER_SIZE..rec_len].iter_mut() {
        *byte = 0;
    }
    let name_end = DIR_ENTRY_HEADER_SIZE + name.len();
    data[DIR_ENTRY_HEADER_SIZE..name_end].copy_from_slice(name);
}

// ---- Helpers for path splitting ----

pub fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
    if path.is_empty() || path[0] != b'/' {
        return None;
    }
    let mut end = path.len();
    while end > 1 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 1 {
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

// ---- Little-endian byte helpers ----

fn le16(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

fn le32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

fn put_le16(data: &mut [u8], offset: usize, val: u16) {
    data[offset..offset + 2].copy_from_slice(&val.to_le_bytes());
}

fn put_le32(data: &mut [u8], offset: usize, val: u32) {
    data[offset..offset + 4].copy_from_slice(&val.to_le_bytes());
}
