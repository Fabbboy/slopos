use super::Ext2Error;
use super::cache::BlockCache;
use super::ext2_alloc;
use super::ondisk::{Inode, Superblock};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;

const DIRECT_BLOCKS: u32 = 12;
const INDIRECT_IDX: u32 = 12;
const DINDIRECT_IDX: u32 = 13;
const TINDIRECT_IDX: u32 = 14;

/// A chain of offsets to traverse from the inode's block[] array.
#[derive(Debug)]
pub struct BlockPath {
    pub depth: u8,
    pub offsets: [u32; 4],
}

pub fn block_to_path(file_block: FileBlock, ptrs_per_block: u32) -> Result<BlockPath, Ext2Error> {
    let fb = file_block.raw();
    let n = ptrs_per_block;

    if fb < DIRECT_BLOCKS {
        return Ok(BlockPath {
            depth: 1,
            offsets: [fb, 0, 0, 0],
        });
    }

    let fb = fb - DIRECT_BLOCKS;
    if fb < n {
        return Ok(BlockPath {
            depth: 2,
            offsets: [INDIRECT_IDX, fb, 0, 0],
        });
    }

    let fb = fb - n;
    let n2 = n.checked_mul(n).ok_or(Ext2Error::InvalidBlock)?;
    if fb < n2 {
        return Ok(BlockPath {
            depth: 3,
            offsets: [DINDIRECT_IDX, fb / n, fb % n, 0],
        });
    }

    let fb = fb - n2;
    let n3 = n2.checked_mul(n).ok_or(Ext2Error::InvalidBlock)?;
    if fb < n3 {
        return Ok(BlockPath {
            depth: 4,
            offsets: [TINDIRECT_IDX, fb / n2, (fb / n) % n, fb % n],
        });
    }

    Err(Ext2Error::InvalidBlock)
}

fn read_ptr(data: &[u8], idx: u32) -> BlockNum {
    let off = idx as usize * 4;
    BlockNum(u32::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn write_ptr(data: &mut [u8], idx: u32, block: BlockNum) {
    let off = idx as usize * 4;
    data[off..off + 4].copy_from_slice(&block.raw().to_le_bytes());
}

/// Returns `BlockNum::ZERO` for holes (sparse file).
pub fn map_block(
    inode: &Inode,
    file_block: FileBlock,
    ptrs_per_block: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<BlockNum, Ext2Error> {
    let path = block_to_path(file_block, ptrs_per_block)?;

    let mut current = inode.block[path.offsets[0] as usize];
    if !current.is_valid() {
        return Ok(BlockNum::ZERO);
    }

    for level in 1..path.depth as usize {
        let block = cache.get(current, device)?;
        current = read_ptr(block.data(), path.offsets[level]);
        if !current.is_valid() {
            return Ok(BlockNum::ZERO);
        }
    }

    Ok(current)
}

/// Ensure a data block exists at the given file block offset.
/// Allocates blocks internally via the alloc module — no closure needed.
pub fn ensure_data_block(
    inode: &mut Inode,
    file_block: FileBlock,
    ptrs_per_block: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    superblock: &mut Superblock,
    block_size: u32,
) -> Result<(BlockNum, bool), Ext2Error> {
    let path = block_to_path(file_block, ptrs_per_block)?;

    if path.depth == 1 {
        let idx = path.offsets[0] as usize;
        if inode.block[idx].is_valid() {
            return Ok((inode.block[idx], false));
        }
        let new_block = ext2_alloc::allocate_block(superblock, cache, device, block_size)?;
        drop(cache.get_zero_data(new_block, device)?);
        inode.block[idx] = new_block;
        return Ok((new_block, true));
    }

    let top_idx = path.offsets[0] as usize;
    if !inode.block[top_idx].is_valid() {
        let new_block = ext2_alloc::allocate_block(superblock, cache, device, block_size)?;
        drop(cache.get_zero(new_block, device)?);
        inode.block[top_idx] = new_block;
    }

    let mut current_indirect = inode.block[top_idx];
    for level in 1..path.depth as usize - 1 {
        let child = {
            let block = cache.get(current_indirect, device)?;
            read_ptr(block.data(), path.offsets[level])
        };
        if child.is_valid() {
            current_indirect = child;
        } else {
            let new_block = ext2_alloc::allocate_block(superblock, cache, device, block_size)?;
            drop(cache.get_zero(new_block, device)?);
            let mut parent = cache.get(current_indirect, device)?;
            write_ptr(parent.data_mut(), path.offsets[level], new_block);
            current_indirect = new_block;
        }
    }

    let data_idx = path.offsets[path.depth as usize - 1];
    let existing = {
        let block = cache.get(current_indirect, device)?;
        read_ptr(block.data(), data_idx)
    };

    if existing.is_valid() {
        return Ok((existing, false));
    }

    let new_data = ext2_alloc::allocate_block(superblock, cache, device, block_size)?;
    drop(cache.get_zero_data(new_data, device)?);
    let mut parent = cache.get(current_indirect, device)?;
    write_ptr(parent.data_mut(), data_idx, new_data);

    Ok((new_data, true))
}
