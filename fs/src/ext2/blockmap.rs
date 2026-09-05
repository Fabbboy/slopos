use super::Ext2Error;
use super::cache::{BlockCache, BlockOwner};
use super::ext2_alloc;
use super::geometry::Ext2Geometry;
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

/// Bytes addressable by twelve direct blocks plus three levels of indirection.
///
/// Saturating rather than checked: `ptrs_per_block` is `block_size / 4`, so
/// the product cannot overflow `u64` for any block size this implementation
/// accepts, and a saturating answer is a cap rather than a panic if one ever
/// could.
pub fn max_file_size(ptrs_per_block: u32, block_size: u32) -> u64 {
    let n = ptrs_per_block as u64;
    let blocks = (DIRECT_BLOCKS as u64)
        .saturating_add(n)
        .saturating_add(n.saturating_mul(n))
        .saturating_add(n.saturating_mul(n).saturating_mul(n));
    // An ext2 file block number is 32 bits, which binds first on a large block
    // size: 1024³ already exceeds `u32::MAX`.
    blocks
        .min(u32::MAX as u64)
        .saturating_mul(block_size as u64)
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

    // Past what three levels of indirection can address. This is a *caller's*
    // offset, not a number the image supplied: `file_block_index` only refuses
    // above 16 TiB, so every offset between the triple-indirect reach and that
    // ceiling arrives here. `InvalidRange` rather than `InvalidBlock` is
    // therefore load-bearing — the latter is classified as image damage, and a
    // one-byte `pwrite` past the reach would latch the whole mount read-only
    // for every process on it.
    Err(Ext2Error::InvalidRange)
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
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    let path = block_to_path(file_block, ptrs_per_block)?;

    let mut current = inode.block[path.offsets[0] as usize];
    if !current.is_valid() {
        return Ok(BlockNum::ZERO);
    }

    for level in 1..path.depth as usize {
        let block = cache.get_owned(current, device, owner)?;
        current = read_ptr(block.data(), path.offsets[level]);
        if !current.is_valid() {
            return Ok(BlockNum::ZERO);
        }
    }

    Ok(current)
}

/// Answers the block plus how many blocks the call allocated, **including the
/// indirect blocks it had to create**.
///
/// `i_blocks` counts every 512-byte sector the inode owns, indirect blocks
/// among them: `e2fsck` recomputes the field from the whole tree and reports
/// one that counts only the data. Returning a count rather than a bool is what
/// lets the caller keep it right.
pub fn ensure_data_block(
    inode: &mut Inode,
    file_block: FileBlock,
    ptrs_per_block: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    superblock: &mut Superblock,
    geom: &Ext2Geometry,
    owner: BlockOwner,
) -> Result<(BlockNum, u32), Ext2Error> {
    let path = block_to_path(file_block, ptrs_per_block)?;
    let mut allocated = 0u32;

    if path.depth == 1 {
        let idx = path.offsets[0] as usize;
        if inode.block[idx].is_valid() {
            return Ok((inode.block[idx], 0));
        }
        let new_block = ext2_alloc::allocate_block(geom, superblock, cache, device)?;
        drop(cache.get_zero_data(new_block, device, owner)?);
        inode.block[idx] = new_block;
        return Ok((new_block, 1));
    }

    let top_idx = path.offsets[0] as usize;
    if !inode.block[top_idx].is_valid() {
        let new_block = ext2_alloc::allocate_block(geom, superblock, cache, device)?;
        drop(cache.get_zero_owned(new_block, device, owner)?);
        inode.block[top_idx] = new_block;
        allocated += 1;
    }

    let mut current_indirect = inode.block[top_idx];
    for level in 1..path.depth as usize - 1 {
        let child = {
            let block = cache.get_owned(current_indirect, device, owner)?;
            read_ptr(block.data(), path.offsets[level])
        };
        if child.is_valid() {
            current_indirect = child;
        } else {
            let new_block = ext2_alloc::allocate_block(geom, superblock, cache, device)?;
            drop(cache.get_zero_owned(new_block, device, owner)?);
            let mut parent = cache.get_owned(current_indirect, device, owner)?;
            write_ptr(parent.data_mut(), path.offsets[level], new_block);
            current_indirect = new_block;
            allocated += 1;
        }
    }

    let data_idx = path.offsets[path.depth as usize - 1];
    let existing = {
        let block = cache.get_owned(current_indirect, device, owner)?;
        read_ptr(block.data(), data_idx)
    };

    if existing.is_valid() {
        return Ok((existing, allocated));
    }

    let new_data = ext2_alloc::allocate_block(geom, superblock, cache, device)?;
    drop(cache.get_zero_data(new_data, device, owner)?);
    let mut parent = cache.get_owned(current_indirect, device, owner)?;
    write_ptr(parent.data_mut(), data_idx, new_data);

    Ok((new_data, allocated + 1))
}
