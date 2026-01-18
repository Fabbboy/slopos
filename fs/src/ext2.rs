use core::cmp;
use core::mem;

use crate::blockdev::BlockDevice;
use slopos_lib::wl_currency;

const EXT2_MIN_BLOCK_SIZE: u32 = 1024;
const EXT2_MAX_BLOCK_SIZE: u32 = 4096;
const EXT2_MAX_BLOCK_SIZE_USIZE: usize = EXT2_MAX_BLOCK_SIZE as usize;

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Ext2Error {
    InvalidSuperblock,
    UnsupportedBlockSize,
    InvalidInode,
    InvalidBlock,
    UnsupportedIndirection,
    DeviceError,
    DirectoryFormat,
    NotDirectory,
    NotFile,
    PathNotFound,
}

#[derive(Debug, Copy, Clone)]
pub struct Ext2Superblock {
    pub inodes_count: u32,
    pub blocks_count: u32,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub blocks_per_group: u32,
    pub inodes_per_group: u32,
    pub magic: u16,
    pub rev_level: u32,
    pub first_ino: u32,
    pub inode_size: u16,
}

#[derive(Debug, Copy, Clone)]
pub struct Ext2GroupDesc {
    pub block_bitmap: u32,
    pub inode_bitmap: u32,
    pub inode_table: u32,
    pub free_blocks_count: u16,
    pub free_inodes_count: u16,
    pub used_dirs_count: u16,
}

#[derive(Debug, Copy, Clone)]
pub struct Ext2Inode {
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
    pub block: [u32; 15],
}

impl Ext2Inode {
    pub fn is_directory(&self) -> bool {
        (self.mode & 0x4000) != 0
    }

    pub fn is_regular_file(&self) -> bool {
        (self.mode & 0x8000) != 0
    }
}

#[derive(Debug, Copy, Clone)]
pub struct Ext2DirEntry<'a> {
    pub inode: u32,
    pub file_type: u8,
    pub name: &'a [u8],
}

pub struct Ext2Fs<'a> {
    device: &'a mut dyn BlockDevice,
    superblock: Ext2Superblock,
    block_size: u32,
    inode_size: u16,
    blocks_per_group: u32,
    inodes_per_group: u32,
}

impl<'a> Ext2Fs<'a> {
    pub fn init(device: &'a mut dyn BlockDevice) -> Result<Self, Ext2Error> {
        let result = Self::init_internal(device);
        match result {
            Ok(_) => wl_currency::award_win(),
            Err(_) => wl_currency::award_loss(),
        }
        result
    }

    pub fn superblock(&self) -> Ext2Superblock {
        self.superblock
    }

    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    pub fn read_inode(&mut self, inode: u32) -> Result<Ext2Inode, Ext2Error> {
        let result = self.read_inode_internal(inode);
        Self::record_result(&result);
        result
    }

    pub fn read_file(
        &mut self,
        inode: u32,
        offset: u32,
        buffer: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        let result = self.read_file_internal(inode, offset, buffer);
        Self::record_result(&result);
        result
    }

    pub fn write_file(
        &mut self,
        inode: u32,
        offset: u32,
        buffer: &[u8],
    ) -> Result<usize, Ext2Error> {
        let result = self.write_file_internal(inode, offset, buffer);
        Self::record_result(&result);
        result
    }

    pub fn for_each_dir_entry<F>(&mut self, inode: u32, mut f: F) -> Result<(), Ext2Error>
    where
        F: FnMut(Ext2DirEntry<'_>) -> bool,
    {
        let result = self.for_each_dir_entry_internal(inode, &mut f);
        Self::record_result(&result);
        result
    }

    pub fn resolve_path(&mut self, path: &[u8]) -> Result<u32, Ext2Error> {
        let result = self.resolve_path_internal(path);
        Self::record_result(&result);
        result
    }

    pub fn create_file(&mut self, parent_inode: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        let result = self.create_inode_entry(parent_inode, name, false);
        Self::record_result(&result);
        result
    }

    pub fn create_directory(&mut self, parent_inode: u32, name: &[u8]) -> Result<u32, Ext2Error> {
        let result = self.create_inode_entry(parent_inode, name, true);
        Self::record_result(&result);
        result
    }

    pub fn remove_path(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        let result = self.remove_path_internal(path);
        Self::record_result(&result);
        result
    }

    fn record_result<T>(result: &Result<T, Ext2Error>) {
        // W/L ledger: success earns a win, recoverable errors take a loss.
        match result {
            Ok(_) => wl_currency::award_win(),
            Err(_) => wl_currency::award_loss(),
        }
    }

    pub(crate) fn init_internal(device: &'a mut dyn BlockDevice) -> Result<Self, Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        let superblock = parse_superblock(&sb_buf)?;
        if superblock.magic != 0xEF53 {
            return Err(Ext2Error::InvalidSuperblock);
        }

        let block_size = EXT2_MIN_BLOCK_SIZE
            .checked_shl(superblock.log_block_size)
            .ok_or(Ext2Error::UnsupportedBlockSize)?;
        if block_size < EXT2_MIN_BLOCK_SIZE || block_size > EXT2_MAX_BLOCK_SIZE {
            return Err(Ext2Error::UnsupportedBlockSize);
        }
        let inode_size = if superblock.inode_size == 0 {
            128
        } else {
            superblock.inode_size
        };

        Ok(Self {
            device,
            superblock,
            block_size,
            inode_size,
            blocks_per_group: superblock.blocks_per_group,
            inodes_per_group: superblock.inodes_per_group,
        })
    }

    fn read_inode_internal(&mut self, inode: u32) -> Result<Ext2Inode, Ext2Error> {
        if inode == 0 || inode > self.superblock.inodes_count {
            return Err(Ext2Error::InvalidInode);
        }
        let inode_index = inode - 1;
        let group = inode_index / self.inodes_per_group;
        let index_in_group = inode_index % self.inodes_per_group;
        let group_desc = self.read_group_desc(group)?;
        let inode_table_block = group_desc.inode_table;
        if inode_table_block == 0 {
            return Err(Ext2Error::InvalidInode);
        }
        let inode_offset = (inode_table_block as u64 * self.block_size as u64)
            + (index_in_group as u64 * self.inode_size as u64);
        let block_offset = inode_offset / self.block_size as u64;
        let within = (inode_offset % self.block_size as u64) as usize;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        self.read_block(block_offset as u32, block_slice)?;
        let inode_bytes = &block_slice[within..within + self.inode_size as usize];
        Ok(parse_inode(inode_bytes))
    }

    fn read_file_internal(
        &mut self,
        inode: u32,
        offset: u32,
        buffer: &mut [u8],
    ) -> Result<usize, Ext2Error> {
        let inode = self.read_inode_internal(inode)?;
        if !inode.is_regular_file() {
            return Err(Ext2Error::NotFile);
        }
        let file_size = inode.size;
        if offset >= file_size || buffer.is_empty() {
            return Ok(0);
        }
        let max_len = cmp::min(buffer.len() as u32, file_size - offset) as usize;
        let mut read_total = 0usize;
        let mut remaining = max_len;
        let mut file_offset = offset as usize;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        while remaining > 0 {
            let file_block = file_offset / self.block_size as usize;
            let block_offset = file_offset % self.block_size as usize;
            let block_num = self.map_block(&inode, file_block as u32)?;
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(block_num, block_slice)?;
            let to_copy = cmp::min(remaining, self.block_size as usize - block_offset);
            buffer[read_total..read_total + to_copy]
                .copy_from_slice(&block_slice[block_offset..block_offset + to_copy]);
            read_total += to_copy;
            remaining -= to_copy;
            file_offset += to_copy;
        }
        Ok(read_total)
    }

    fn for_each_dir_entry_internal<F>(&mut self, inode: u32, f: &mut F) -> Result<(), Ext2Error>
    where
        F: FnMut(Ext2DirEntry<'_>) -> bool,
    {
        let inode = self.read_inode_internal(inode)?;
        if !inode.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let mut remaining = inode.size as usize;
        let mut offset = 0usize;
        while remaining > 0 {
            let file_block = offset / self.block_size as usize;
            let block_offset = offset % self.block_size as usize;
            let block_num = self.map_block(&inode, file_block as u32)?;
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(block_num, block_slice)?;
            let mut cursor = block_offset;
            while cursor + 8 <= self.block_size as usize && remaining > 0 {
                let entry_inode = read_le_u32(&block_slice[cursor..]);
                let rec_len = read_le_u16(&block_slice[cursor + 4..]) as usize;
                let name_len = block_slice[cursor + 6] as usize;
                let file_type = block_slice[cursor + 7];
                if rec_len < 8 || cursor + rec_len > self.block_size as usize {
                    return Err(Ext2Error::DirectoryFormat);
                }
                if entry_inode != 0 {
                    let name_start = cursor + 8;
                    let name_end = name_start + name_len;
                    if name_end > cursor + rec_len {
                        return Err(Ext2Error::DirectoryFormat);
                    }
                    let entry = Ext2DirEntry {
                        inode: entry_inode,
                        file_type,
                        name: &block_slice[name_start..name_end],
                    };
                    if !f(entry) {
                        return Ok(());
                    }
                }
                cursor += rec_len;
                if remaining >= rec_len {
                    remaining -= rec_len;
                } else {
                    remaining = 0;
                }
            }
            offset += self.block_size as usize - block_offset;
        }
        Ok(())
    }

    fn resolve_path_internal(&mut self, path: &[u8]) -> Result<u32, Ext2Error> {
        if path.is_empty() || path[0] != b'/' {
            return Err(Ext2Error::PathNotFound);
        }
        let mut inode = 2u32;
        let mut idx = 0usize;
        while idx < path.len() {
            while idx < path.len() && path[idx] == b'/' {
                idx += 1;
            }
            if idx >= path.len() {
                break;
            }
            let start = idx;
            while idx < path.len() && path[idx] != b'/' {
                idx += 1;
            }
            let component = &path[start..idx];
            if component == b"." {
                continue;
            }
            if component == b".." {
                inode = self.parent_inode(inode)?;
                continue;
            }
            let mut found = None;
            self.for_each_dir_entry_internal(inode, &mut |entry| {
                if entry.name == component {
                    found = Some(entry.inode);
                    return false;
                }
                true
            })?;
            if let Some(next) = found {
                inode = next;
            } else {
                return Err(Ext2Error::PathNotFound);
            }
        }
        Ok(inode)
    }

    fn remove_path_internal(&mut self, path: &[u8]) -> Result<(), Ext2Error> {
        if path.is_empty() || path == b"/" {
            return Err(Ext2Error::PathNotFound);
        }
        let target_inode = self.resolve_path_internal(path)?;
        let inode_data = self.read_inode_internal(target_inode)?;
        if inode_data.is_directory() {
            return Err(Ext2Error::NotFile);
        }
        let (parent, name) = split_parent(path).ok_or(Ext2Error::PathNotFound)?;
        if name == b"." || name == b".." {
            return Err(Ext2Error::PathNotFound);
        }
        let parent_inode = self.resolve_path_internal(parent)?;
        self.remove_dir_entry(parent_inode, name)?;
        self.release_file_blocks(&inode_data)?;
        self.free_inode(target_inode)?;
        Ok(())
    }

    fn write_file_internal(
        &mut self,
        inode_num: u32,
        offset: u32,
        buffer: &[u8],
    ) -> Result<usize, Ext2Error> {
        if buffer.is_empty() {
            return Ok(0);
        }
        let mut inode = self.read_inode_internal(inode_num)?;
        if !inode.is_regular_file() {
            return Err(Ext2Error::NotFile);
        }
        let mut written = 0usize;
        let mut remaining = buffer.len();
        let mut file_offset = offset as usize;
        let mut allocated_blocks = 0u32;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];

        while remaining > 0 {
            let file_block = file_offset / self.block_size as usize;
            let block_offset = file_offset % self.block_size as usize;
            let (block_num, allocated) = self.ensure_data_block(&mut inode, file_block as u32)?;
            if allocated {
                allocated_blocks += 1;
            }
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(block_num, block_slice)?;
            let to_copy = cmp::min(remaining, self.block_size as usize - block_offset);
            block_slice[block_offset..block_offset + to_copy]
                .copy_from_slice(&buffer[written..written + to_copy]);
            self.write_block(block_num, block_slice)?;
            written += to_copy;
            remaining -= to_copy;
            file_offset += to_copy;
        }

        let end_pos = offset as u64 + written as u64;
        if end_pos > inode.size as u64 {
            inode.size = end_pos as u32;
        }
        if allocated_blocks > 0 {
            let sectors_per_block = (self.block_size / 512) as u32;
            inode.blocks = inode
                .blocks
                .saturating_add(allocated_blocks * sectors_per_block);
        }
        self.write_inode(inode_num, inode)?;
        Ok(written)
    }

    fn parent_inode(&mut self, inode: u32) -> Result<u32, Ext2Error> {
        let mut parent = None;
        self.for_each_dir_entry_internal(inode, &mut |entry| {
            if entry.name == b".." {
                parent = Some(entry.inode);
                return false;
            }
            true
        })?;
        parent.ok_or(Ext2Error::PathNotFound)
    }

    fn remove_dir_entry(&mut self, parent_inode: u32, name: &[u8]) -> Result<(), Ext2Error> {
        let parent = self.read_inode_internal(parent_inode)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let mut block_index = 0u32;
        let mut offset = 0usize;
        while offset < parent.size as usize {
            let block_num = self.map_block(&parent, block_index)?;
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(block_num, block_slice)?;
            let mut cursor = 0usize;
            while cursor + 8 <= self.block_size as usize {
                let entry_inode = read_le_u32(&block_slice[cursor..]);
                let rec_len = read_le_u16(&block_slice[cursor + 4..]) as usize;
                let name_len = block_slice[cursor + 6] as usize;
                if rec_len < 8 || cursor + rec_len > self.block_size as usize {
                    return Err(Ext2Error::DirectoryFormat);
                }
                if entry_inode != 0 {
                    let name_start = cursor + 8;
                    let name_end = name_start + name_len;
                    if name_end <= cursor + rec_len && &block_slice[name_start..name_end] == name {
                        write_le_u32(&mut block_slice[cursor..cursor + 4], 0);
                        self.write_block(block_num, block_slice)?;
                        return Ok(());
                    }
                }
                cursor += rec_len;
            }
            offset += self.block_size as usize;
            block_index += 1;
        }
        Err(Ext2Error::PathNotFound)
    }

    fn release_file_blocks(&mut self, inode: &Ext2Inode) -> Result<(), Ext2Error> {
        for block in inode.block.iter().take(12) {
            if *block != 0 {
                self.free_block(*block)?;
            }
        }
        if inode.block[12] != 0 {
            let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(inode.block[12], block_slice)?;
            for idx in 0..(self.block_size as usize / 4) {
                let offset = idx * 4;
                let block = read_le_u32(&block_slice[offset..]);
                if block != 0 {
                    self.free_block(block)?;
                }
            }
            self.free_block(inode.block[12])?;
        }
        Ok(())
    }

    fn create_inode_entry(
        &mut self,
        parent_inode: u32,
        name: &[u8],
        is_dir: bool,
    ) -> Result<u32, Ext2Error> {
        if name.is_empty() || name.len() > 255 {
            return Err(Ext2Error::PathNotFound);
        }
        let parent = self.read_inode_internal(parent_inode)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        let inode_num = self.allocate_inode()?;
        let mut inode = Ext2Inode {
            mode: if is_dir { MODE_DIRECTORY } else { MODE_FILE },
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: if is_dir { 2 } else { 1 },
            blocks: 0,
            flags: 0,
            block: [0u32; 15],
        };

        if is_dir {
            let block = self.allocate_block()?;
            inode.block[0] = block;
            inode.blocks = (self.block_size / 512) as u32;
            inode.size = self.block_size;
            self.write_dir_block(block, inode_num, parent_inode)?;
        }

        self.write_inode(inode_num, inode)?;
        self.append_dir_entry(parent_inode, inode_num, name, is_dir)?;
        if is_dir {
            let mut parent = self.read_inode_internal(parent_inode)?;
            parent.links_count = parent.links_count.saturating_add(1);
            self.write_inode(parent_inode, parent)?;
            self.bump_used_dirs(parent_inode)?;
        }
        Ok(inode_num)
    }

    fn read_group_desc(&mut self, group: u32) -> Result<Ext2GroupDesc, Ext2Error> {
        let desc_size = mem::size_of::<Ext2GroupDesc>() as u64;
        let table_start = if self.block_size == 1024 {
            2u64 * self.block_size as u64
        } else {
            self.block_size as u64
        };
        let desc_offset = table_start + (group as u64 * desc_size);
        let block = desc_offset / self.block_size as u64;
        let within = (desc_offset % self.block_size as u64) as usize;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        self.read_block(block as u32, block_slice)?;
        let desc_bytes = &block_slice[within..within + desc_size as usize];
        Ok(parse_group_desc(desc_bytes))
    }

    fn write_group_desc(&mut self, group: u32, desc: Ext2GroupDesc) -> Result<(), Ext2Error> {
        let desc_size = mem::size_of::<Ext2GroupDesc>() as u64;
        let table_start = if self.block_size == 1024 {
            2u64 * self.block_size as u64
        } else {
            self.block_size as u64
        };
        let desc_offset = table_start + (group as u64 * desc_size);
        let block = desc_offset / self.block_size as u64;
        let within = (desc_offset % self.block_size as u64) as usize;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        self.read_block(block as u32, block_slice)?;
        let desc_bytes = &mut block_slice[within..within + desc_size as usize];
        encode_group_desc(desc_bytes, desc);
        self.write_block(block as u32, block_slice)?;
        Ok(())
    }

    fn read_block(&mut self, block: u32, buffer: &mut [u8]) -> Result<(), Ext2Error> {
        if buffer.len() != self.block_size as usize {
            return Err(Ext2Error::InvalidBlock);
        }
        let offset = block as u64 * self.block_size as u64;
        if offset + self.block_size as u64 > self.device.capacity() {
            return Err(Ext2Error::InvalidBlock);
        }
        self.device
            .read_at(offset, buffer)
            .map_err(|_| Ext2Error::DeviceError)
    }

    fn write_block(&mut self, block: u32, buffer: &[u8]) -> Result<(), Ext2Error> {
        if buffer.len() != self.block_size as usize {
            return Err(Ext2Error::InvalidBlock);
        }
        let offset = block as u64 * self.block_size as u64;
        if offset + self.block_size as u64 > self.device.capacity() {
            return Err(Ext2Error::InvalidBlock);
        }
        self.device
            .write_at(offset, buffer)
            .map_err(|_| Ext2Error::DeviceError)
    }

    fn write_superblock(&mut self) -> Result<(), Ext2Error> {
        let mut sb_buf = [0u8; 1024];
        self.device
            .read_at(1024, &mut sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        encode_superblock(&mut sb_buf, self.superblock);
        self.device
            .write_at(1024, &sb_buf)
            .map_err(|_| Ext2Error::DeviceError)?;
        Ok(())
    }

    fn map_block(&mut self, inode: &Ext2Inode, file_block: u32) -> Result<u32, Ext2Error> {
        if file_block < 12 {
            let block = inode.block[file_block as usize];
            if block == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            return Ok(block);
        }
        if file_block < 12 + (self.block_size / 4) {
            let ind_block = inode.block[12];
            if ind_block == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(ind_block, block_slice)?;
            let idx = file_block - 12;
            let offset = (idx as usize) * 4;
            let block = read_le_u32(&block_slice[offset..]);
            if block == 0 {
                return Err(Ext2Error::InvalidBlock);
            }
            return Ok(block);
        }
        Err(Ext2Error::UnsupportedIndirection)
    }

    fn ensure_data_block(
        &mut self,
        inode: &mut Ext2Inode,
        file_block: u32,
    ) -> Result<(u32, bool), Ext2Error> {
        if file_block < 12 {
            let entry = &mut inode.block[file_block as usize];
            if *entry == 0 {
                *entry = self.allocate_block()?;
                return Ok((*entry, true));
            }
            return Ok((*entry, false));
        }
        let per_block = (self.block_size / 4) as u32;
        if file_block < 12 + per_block {
            if inode.block[12] == 0 {
                let ind_block = self.allocate_block()?;
                inode.block[12] = ind_block;
                let zero = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
                let slice = &zero[..self.block_size as usize];
                self.write_block(ind_block, slice)?;
            }
            let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
            let block_slice = &mut block_buf[..self.block_size as usize];
            let ind_block = inode.block[12];
            self.read_block(ind_block, block_slice)?;
            let idx = file_block - 12;
            let offset = (idx as usize) * 4;
            let mut entry = read_le_u32(&block_slice[offset..]);
            if entry == 0 {
                entry = self.allocate_block()?;
                write_le_u32(&mut block_slice[offset..offset + 4], entry);
                self.write_block(ind_block, block_slice)?;
                return Ok((entry, true));
            }
            return Ok((entry, false));
        }
        Err(Ext2Error::UnsupportedIndirection)
    }

    fn allocate_block(&mut self) -> Result<u32, Ext2Error> {
        self.bitmap_allocate(BitmapKind::Block)
    }

    fn free_block(&mut self, block: u32) -> Result<(), Ext2Error> {
        self.bitmap_free(BitmapKind::Block, block)
    }

    fn allocate_inode(&mut self) -> Result<u32, Ext2Error> {
        self.bitmap_allocate(BitmapKind::Inode)
    }

    fn free_inode(&mut self, inode_num: u32) -> Result<(), Ext2Error> {
        self.bitmap_free(BitmapKind::Inode, inode_num)?;
        let empty = Ext2Inode {
            mode: 0,
            uid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            gid: 0,
            links_count: 0,
            blocks: 0,
            flags: 0,
            block: [0u32; 15],
        };
        self.write_inode(inode_num, empty)
    }

    fn write_inode(&mut self, inode_num: u32, inode: Ext2Inode) -> Result<(), Ext2Error> {
        if inode_num == 0 || inode_num > self.superblock.inodes_count {
            return Err(Ext2Error::InvalidInode);
        }
        let inode_index = inode_num - 1;
        let group = inode_index / self.inodes_per_group;
        let index_in_group = inode_index % self.inodes_per_group;
        let group_desc = self.read_group_desc(group)?;
        if group_desc.inode_table == 0 {
            return Err(Ext2Error::InvalidInode);
        }
        let inode_offset = (group_desc.inode_table as u64 * self.block_size as u64)
            + (index_in_group as u64 * self.inode_size as u64);
        let block = inode_offset / self.block_size as u64;
        let within = (inode_offset % self.block_size as u64) as usize;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        self.read_block(block as u32, block_slice)?;
        let inode_bytes = &mut block_slice[within..within + self.inode_size as usize];
        encode_inode(inode_bytes, inode);
        self.write_block(block as u32, block_slice)?;
        Ok(())
    }

    fn append_dir_entry(
        &mut self,
        parent_inode: u32,
        child_inode: u32,
        name: &[u8],
        is_dir: bool,
    ) -> Result<(), Ext2Error> {
        let mut parent = self.read_inode_internal(parent_inode)?;
        if !parent.is_directory() {
            return Err(Ext2Error::NotDirectory);
        }
        let entry_size = dir_entry_size(name.len());
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let mut offset = 0usize;
        let mut block_index = 0u32;

        while offset < parent.size as usize {
            let block_num = self.map_block(&parent, block_index)?;
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(block_num, block_slice)?;
            let mut cursor = 0usize;
            while cursor + 8 <= self.block_size as usize {
                let rec_len = read_le_u16(&block_slice[cursor + 4..]) as usize;
                if rec_len < 8 || cursor + rec_len > self.block_size as usize {
                    return Err(Ext2Error::DirectoryFormat);
                }
                if cursor + rec_len == self.block_size as usize {
                    let name_len = block_slice[cursor + 6] as usize;
                    let used = dir_entry_size(name_len);
                    if rec_len >= used + entry_size {
                        write_le_u16(&mut block_slice[cursor + 4..cursor + 6], used as u16);
                        let new_off = cursor + used;
                        write_dir_entry(
                            &mut block_slice[new_off..],
                            child_inode,
                            name,
                            is_dir,
                            rec_len - used,
                        );
                        self.write_block(block_num, block_slice)?;
                        return Ok(());
                    }
                    break;
                }
                cursor += rec_len;
            }
            offset += self.block_size as usize;
            block_index += 1;
        }

        let (block_num, allocated) = self.ensure_data_block(&mut parent, block_index)?;
        if allocated {
            parent.size = parent.size.saturating_add(self.block_size);
            let sectors_per_block = (self.block_size / 512) as u32;
            parent.blocks = parent.blocks.saturating_add(sectors_per_block);
        }
        let block_slice = &mut block_buf[..self.block_size as usize];
        for byte in block_slice.iter_mut() {
            *byte = 0;
        }
        write_dir_entry(
            block_slice,
            child_inode,
            name,
            is_dir,
            self.block_size as usize,
        );
        self.write_block(block_num, block_slice)?;
        self.write_inode(parent_inode, parent)?;
        Ok(())
    }

    fn write_dir_block(
        &mut self,
        block: u32,
        inode_num: u32,
        parent_inode: u32,
    ) -> Result<(), Ext2Error> {
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        for byte in block_slice.iter_mut() {
            *byte = 0;
        }
        let dot_size = dir_entry_size(1);
        write_dir_entry(
            &mut block_slice[..dot_size],
            inode_num,
            b".",
            true,
            dot_size,
        );
        write_dir_entry(
            &mut block_slice[dot_size..],
            parent_inode,
            b"..",
            true,
            self.block_size as usize - dot_size,
        );
        self.write_block(block, block_slice)?;
        Ok(())
    }

    fn bump_used_dirs(&mut self, inode_num: u32) -> Result<(), Ext2Error> {
        let inode_index = inode_num - 1;
        let group = inode_index / self.inodes_per_group;
        let mut desc = self.read_group_desc(group)?;
        desc.used_dirs_count = desc.used_dirs_count.saturating_add(1);
        self.write_group_desc(group, desc)
    }
}

#[derive(Clone, Copy)]
enum BitmapKind {
    Block,
    Inode,
}

impl<'a> Ext2Fs<'a> {
    fn bitmap_counts(&self, kind: BitmapKind) -> (u32, u32) {
        match kind {
            BitmapKind::Block => (self.superblock.blocks_count, self.blocks_per_group),
            BitmapKind::Inode => (self.superblock.inodes_count, self.inodes_per_group),
        }
    }

    fn bitmap_block(desc: &Ext2GroupDesc, kind: BitmapKind) -> u32 {
        match kind {
            BitmapKind::Block => desc.block_bitmap,
            BitmapKind::Inode => desc.inode_bitmap,
        }
    }

    fn free_count(desc: &Ext2GroupDesc, kind: BitmapKind) -> u16 {
        match kind {
            BitmapKind::Block => desc.free_blocks_count,
            BitmapKind::Inode => desc.free_inodes_count,
        }
    }

    fn decrement_free_count(desc: &mut Ext2GroupDesc, kind: BitmapKind) {
        match kind {
            BitmapKind::Block => desc.free_blocks_count = desc.free_blocks_count.saturating_sub(1),
            BitmapKind::Inode => desc.free_inodes_count = desc.free_inodes_count.saturating_sub(1),
        }
    }

    fn increment_free_count(desc: &mut Ext2GroupDesc, kind: BitmapKind) {
        match kind {
            BitmapKind::Block => desc.free_blocks_count = desc.free_blocks_count.saturating_add(1),
            BitmapKind::Inode => desc.free_inodes_count = desc.free_inodes_count.saturating_add(1),
        }
    }

    fn decrement_superblock_free(&mut self, kind: BitmapKind) {
        match kind {
            BitmapKind::Block => {
                self.superblock.free_blocks_count =
                    self.superblock.free_blocks_count.saturating_sub(1)
            }
            BitmapKind::Inode => {
                self.superblock.free_inodes_count =
                    self.superblock.free_inodes_count.saturating_sub(1)
            }
        }
    }

    fn increment_superblock_free(&mut self, kind: BitmapKind) {
        match kind {
            BitmapKind::Block => {
                self.superblock.free_blocks_count =
                    self.superblock.free_blocks_count.saturating_add(1)
            }
            BitmapKind::Inode => {
                self.superblock.free_inodes_count =
                    self.superblock.free_inodes_count.saturating_add(1)
            }
        }
    }

    fn bitmap_index_to_id(&self, kind: BitmapKind, group: u32, bit: usize) -> u32 {
        match kind {
            BitmapKind::Block => {
                group * self.blocks_per_group + bit as u32 + self.superblock.first_data_block
            }
            BitmapKind::Inode => group * self.inodes_per_group + bit as u32 + 1,
        }
    }

    fn id_to_bitmap_index(&self, kind: BitmapKind, id: u32) -> Option<(u32, usize)> {
        match kind {
            BitmapKind::Block => {
                if id < self.superblock.first_data_block {
                    return None;
                }
                let base = id - self.superblock.first_data_block;
                let group = base / self.blocks_per_group;
                let bit = (base % self.blocks_per_group) as usize;
                Some((group, bit))
            }
            BitmapKind::Inode => {
                if id == 0 || id > self.superblock.inodes_count {
                    return None;
                }
                let index = id - 1;
                let group = index / self.inodes_per_group;
                let bit = (index % self.inodes_per_group) as usize;
                Some((group, bit))
            }
        }
    }

    fn bitmap_error(kind: BitmapKind) -> Ext2Error {
        match kind {
            BitmapKind::Block => Ext2Error::InvalidBlock,
            BitmapKind::Inode => Ext2Error::InvalidInode,
        }
    }

    fn alloc_start_bit(&self, kind: BitmapKind, group: u32) -> usize {
        match kind {
            BitmapKind::Block => 0,
            BitmapKind::Inode => {
                if group == 0 && self.superblock.first_ino > 0 {
                    (self.superblock.first_ino - 1) as usize
                } else {
                    0
                }
            }
        }
    }

    fn bitmap_allocate(&mut self, kind: BitmapKind) -> Result<u32, Ext2Error> {
        let (total_count, per_group) = self.bitmap_counts(kind);
        let num_groups = (total_count + per_group - 1) / per_group;
        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];

        for group in 0..num_groups {
            let mut desc = self.read_group_desc(group)?;
            if Self::free_count(&desc, kind) == 0 {
                continue;
            }

            let bitmap_blk = Self::bitmap_block(&desc, kind);
            let block_slice = &mut block_buf[..self.block_size as usize];
            self.read_block(bitmap_blk, block_slice)?;

            let start_bit = self.alloc_start_bit(kind, group);
            if let Some(bit) = find_free_bit_from(block_slice, start_bit) {
                set_bit(block_slice, bit);
                self.write_block(bitmap_blk, block_slice)?;

                Self::decrement_free_count(&mut desc, kind);
                self.decrement_superblock_free(kind);
                self.write_group_desc(group, desc)?;
                self.write_superblock()?;

                return Ok(self.bitmap_index_to_id(kind, group, bit));
            }
        }

        Err(Self::bitmap_error(kind))
    }

    fn bitmap_free(&mut self, kind: BitmapKind, id: u32) -> Result<(), Ext2Error> {
        let (group, bit) = self
            .id_to_bitmap_index(kind, id)
            .ok_or_else(|| Self::bitmap_error(kind))?;

        let mut desc = self.read_group_desc(group)?;
        let bitmap_blk = Self::bitmap_block(&desc, kind);

        let mut block_buf = [0u8; EXT2_MAX_BLOCK_SIZE_USIZE];
        let block_slice = &mut block_buf[..self.block_size as usize];
        self.read_block(bitmap_blk, block_slice)?;

        clear_bit(block_slice, bit);
        self.write_block(bitmap_blk, block_slice)?;

        Self::increment_free_count(&mut desc, kind);
        self.increment_superblock_free(kind);
        self.write_group_desc(group, desc)?;
        self.write_superblock()?;

        Ok(())
    }
}

fn parse_superblock(data: &[u8]) -> Result<Ext2Superblock, Ext2Error> {
    if data.len() < 1024 {
        return Err(Ext2Error::InvalidSuperblock);
    }
    Ok(Ext2Superblock {
        inodes_count: read_le_u32(&data[0..]),
        blocks_count: read_le_u32(&data[4..]),
        free_blocks_count: read_le_u32(&data[12..]),
        free_inodes_count: read_le_u32(&data[16..]),
        first_data_block: read_le_u32(&data[20..]),
        log_block_size: read_le_u32(&data[24..]),
        blocks_per_group: read_le_u32(&data[32..]),
        inodes_per_group: read_le_u32(&data[40..]),
        magic: read_le_u16(&data[56..]),
        rev_level: read_le_u32(&data[76..]),
        first_ino: read_le_u32(&data[84..]),
        inode_size: read_le_u16(&data[88..]),
    })
}

fn parse_group_desc(data: &[u8]) -> Ext2GroupDesc {
    Ext2GroupDesc {
        block_bitmap: read_le_u32(&data[0..]),
        inode_bitmap: read_le_u32(&data[4..]),
        inode_table: read_le_u32(&data[8..]),
        free_blocks_count: read_le_u16(&data[12..]),
        free_inodes_count: read_le_u16(&data[14..]),
        used_dirs_count: read_le_u16(&data[16..]),
    }
}

fn parse_inode(data: &[u8]) -> Ext2Inode {
    let mut block = [0u32; 15];
    let mut offset = 40usize;
    for idx in 0..15 {
        block[idx] = read_le_u32(&data[offset..]);
        offset += 4;
    }
    Ext2Inode {
        mode: read_le_u16(&data[0..]),
        uid: read_le_u16(&data[2..]),
        size: read_le_u32(&data[4..]),
        atime: read_le_u32(&data[8..]),
        ctime: read_le_u32(&data[12..]),
        mtime: read_le_u32(&data[16..]),
        dtime: read_le_u32(&data[20..]),
        gid: read_le_u16(&data[24..]),
        links_count: read_le_u16(&data[26..]),
        blocks: read_le_u32(&data[28..]),
        flags: read_le_u32(&data[32..]),
        block,
    }
}

fn encode_superblock(data: &mut [u8], sb: Ext2Superblock) {
    write_le_u32(&mut data[12..16], sb.free_blocks_count);
    write_le_u32(&mut data[16..20], sb.free_inodes_count);
}

fn encode_group_desc(data: &mut [u8], desc: Ext2GroupDesc) {
    write_le_u32(&mut data[0..4], desc.block_bitmap);
    write_le_u32(&mut data[4..8], desc.inode_bitmap);
    write_le_u32(&mut data[8..12], desc.inode_table);
    write_le_u16(&mut data[12..14], desc.free_blocks_count);
    write_le_u16(&mut data[14..16], desc.free_inodes_count);
    write_le_u16(&mut data[16..18], desc.used_dirs_count);
}

fn encode_inode(data: &mut [u8], inode: Ext2Inode) {
    for byte in data.iter_mut() {
        *byte = 0;
    }
    write_le_u16(&mut data[0..2], inode.mode);
    write_le_u16(&mut data[2..4], inode.uid);
    write_le_u32(&mut data[4..8], inode.size);
    write_le_u32(&mut data[8..12], inode.atime);
    write_le_u32(&mut data[12..16], inode.ctime);
    write_le_u32(&mut data[16..20], inode.mtime);
    write_le_u32(&mut data[20..24], inode.dtime);
    write_le_u16(&mut data[24..26], inode.gid);
    write_le_u16(&mut data[26..28], inode.links_count);
    write_le_u32(&mut data[28..32], inode.blocks);
    write_le_u32(&mut data[32..36], inode.flags);
    let mut offset = 40usize;
    for idx in 0..15 {
        write_le_u32(&mut data[offset..offset + 4], inode.block[idx]);
        offset += 4;
    }
}

fn dir_entry_size(name_len: usize) -> usize {
    let base = 8 + name_len;
    (base + 3) & !3
}

fn write_dir_entry(data: &mut [u8], inode: u32, name: &[u8], is_dir: bool, rec_len: usize) {
    write_le_u32(&mut data[0..4], inode);
    write_le_u16(&mut data[4..6], rec_len as u16);
    data[6] = name.len() as u8;
    data[7] = if is_dir { 2 } else { 1 };
    for byte in data[8..rec_len].iter_mut() {
        *byte = 0;
    }
    let name_end = 8 + name.len();
    data[8..name_end].copy_from_slice(name);
}

fn split_parent(path: &[u8]) -> Option<(&[u8], &[u8])> {
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

fn find_free_bit_from(bitmap: &[u8], start_bit: usize) -> Option<usize> {
    let start_byte = start_bit / 8;
    let start_offset = start_bit % 8;
    for (byte_idx, byte) in bitmap.iter().enumerate().skip(start_byte) {
        if *byte == 0xFF {
            continue;
        }
        let bit_start = if byte_idx == start_byte {
            start_offset
        } else {
            0
        };
        for bit in bit_start..8 {
            if (*byte & (1 << bit)) == 0 {
                return Some(byte_idx * 8 + bit);
            }
        }
    }
    None
}

fn set_bit(bitmap: &mut [u8], bit: usize) {
    let byte_idx = bit / 8;
    let bit_idx = bit % 8;
    if let Some(byte) = bitmap.get_mut(byte_idx) {
        *byte |= 1 << bit_idx;
    }
}

fn clear_bit(bitmap: &mut [u8], bit: usize) {
    let byte_idx = bit / 8;
    let bit_idx = bit % 8;
    if let Some(byte) = bitmap.get_mut(byte_idx) {
        *byte &= !(1 << bit_idx);
    }
}

fn read_le_u16(data: &[u8]) -> u16 {
    u16::from_le_bytes([data[0], data[1]])
}

fn read_le_u32(data: &[u8]) -> u32 {
    u32::from_le_bytes([data[0], data[1], data[2], data[3]])
}

fn write_le_u16(data: &mut [u8], value: u16) {
    data[0..2].copy_from_slice(&value.to_le_bytes());
}

fn write_le_u32(data: &mut [u8], value: u32) {
    data[0..4].copy_from_slice(&value.to_le_bytes());
}

const MODE_FILE: u16 = 0x8000;
const MODE_DIRECTORY: u16 = 0x4000;
