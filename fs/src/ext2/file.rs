use super::Ext2Error;
use super::blockmap;
use super::cache::{BlockCache, BlockOwner};
use super::geometry::Ext2Geometry;
use super::ondisk::{Inode, Superblock};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;
use core::cmp;

pub fn read_file(
    inode: &Inode,
    offset: u64,
    buffer: &mut [u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
) -> Result<usize, Ext2Error> {
    if !inode.is_regular_file() {
        return Err(Ext2Error::NotFile);
    }
    let file_size = inode.size;
    if offset >= file_size || buffer.is_empty() {
        return Ok(0);
    }
    let max_len = cmp::min(buffer.len() as u64, file_size - offset) as usize;
    let mut read_total = 0usize;
    let mut file_offset = offset;

    while read_total < max_len {
        let fb = FileBlock(file_block_index(file_offset, block_size)?);
        let block_off = (file_offset % block_size as u64) as usize;
        let to_copy = cmp::min(max_len - read_total, block_size as usize - block_off);

        let phys = blockmap::map_block(inode, fb, ptrs_per_block, cache, device, owner)?;
        if phys.is_valid() {
            let blk = cache.get_data(phys, device, owner)?;
            buffer[read_total..read_total + to_copy]
                .copy_from_slice(&blk.data()[block_off..block_off + to_copy]);
        } else {
            buffer[read_total..read_total + to_copy].fill(0);
        }

        read_total += to_copy;
        file_offset += to_copy as u64;
    }
    Ok(read_total)
}

pub fn write_file(
    inode: &mut Inode,
    offset: u64,
    buffer: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    superblock: &mut Superblock,
    geom: &Ext2Geometry,
    owner: BlockOwner,
) -> Result<usize, Ext2Error> {
    if !inode.is_regular_file() {
        return Err(Ext2Error::NotFile);
    }
    if buffer.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    let mut file_offset = offset;
    let mut failure = None;

    while written < buffer.len() {
        let fb = match file_block_index(file_offset, block_size) {
            Ok(fb) => FileBlock(fb),
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        let block_off = (file_offset % block_size as u64) as usize;
        let to_copy = cmp::min(buffer.len() - written, block_size as usize - block_off);

        let (phys, allocated) = match blockmap::ensure_data_block(
            inode,
            fb,
            ptrs_per_block,
            cache,
            device,
            superblock,
            geom,
            owner,
        ) {
            Ok(v) => v,
            Err(e) => {
                failure = Some(e);
                break;
            }
        };
        // Counted before the copy: the blocks are the inode's the moment
        // `ensure_data_block` linked them in, whether or not this iteration
        // goes on to fill them.
        inode.blocks += allocated * (block_size / 512);

        match cache.get_data(phys, device, owner) {
            Ok(mut blk) => {
                blk.data_mut()[block_off..block_off + to_copy]
                    .copy_from_slice(&buffer[written..written + to_copy]);
            }
            Err(e) => {
                failure = Some(e);
                break;
            }
        }

        written += to_copy;
        file_offset += to_copy as u64;
    }

    // The size covers what was written *before* the failure, so the blocks
    // allocated on earlier iterations are reachable rather than leaked, and
    // the caller sees a short count instead of a silent loss.
    let new_end = offset + written as u64;
    if new_end > inode.size {
        inode.size = new_end;
    }
    if written == 0
        && let Some(e) = failure
    {
        return Err(e);
    }
    Ok(written)
}

/// A file offset's block index, refused rather than truncated past the 32-bit
/// block number ext2 addresses with.
fn file_block_index(offset: u64, block_size: u32) -> Result<u32, Ext2Error> {
    u32::try_from(offset / block_size as u64).map_err(|_| Ext2Error::InvalidRange)
}

/// Shrink or extend a file to `new_size`.
///
/// An extension is sparse: no block is allocated, and `read_file` answers zero
/// for the hole. A shrink frees every block past the new end, including the
/// partial tails of the indirect trees, and zeroes the remainder of the last
/// surviving block — a later extension must read zeros there, not the bytes
/// the truncate logically removed.
#[allow(clippy::too_many_arguments)]
pub fn truncate(
    inode: &mut Inode,
    new_size: u64,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    let old_size = inode.size;
    if new_size >= old_size {
        inode.size = new_size;
        return Ok(());
    }

    let bs = block_size as u64;
    let first_free = new_size.div_ceil(bs);
    let mut freed = 0u64;
    let mut count_free = |blk: BlockNum| -> Result<(), Ext2Error> {
        freed += 1;
        free_fn(blk)
    };

    for i in 0..12u64 {
        if i < first_free {
            continue;
        }
        let blk = inode.block[i as usize];
        if blk.is_valid() {
            count_free(blk)?;
            cache.invalidate(blk);
            inode.block[i as usize] = BlockNum::ZERO;
        }
    }

    let ppb = ptrs_per_block as u64;
    let mut subtree_start = 12u64;
    for (slot, depth) in [(12usize, 1u32), (13, 2), (14, 3)] {
        let span = ppb.checked_pow(depth).ok_or(Ext2Error::InvalidBlock)?;
        let root = inode.block[slot];
        if root.is_valid() {
            let from = first_free.saturating_sub(subtree_start);
            if from < span {
                let emptied = truncate_indirect(
                    root,
                    depth,
                    from,
                    cache,
                    device,
                    ptrs_per_block,
                    owner,
                    &mut count_free,
                )?;
                if emptied {
                    count_free(root)?;
                    cache.invalidate(root);
                    inode.block[slot] = BlockNum::ZERO;
                }
            }
        }
        subtree_start += span;
    }

    // Bytes between the new end and the end of its block are still on disk;
    // extending the file again would surface them.
    let tail = (new_size % bs) as usize;
    if tail != 0 {
        let fb = FileBlock(file_block_index(new_size, block_size)?);
        let phys = blockmap::map_block(inode, fb, ptrs_per_block, cache, device, owner)?;
        if phys.is_valid() {
            let mut blk = cache.get_data(phys, device, owner)?;
            blk.data_mut()[tail..].fill(0);
        }
    }

    inode.size = new_size;
    let sectors_per_block = (block_size / 512) as u64;
    inode.blocks = inode
        .blocks
        .saturating_sub((freed.saturating_mul(sectors_per_block)).min(u32::MAX as u64) as u32);

    Ok(())
}

/// Free every block at or after relative file-block index `from` inside the
/// subtree rooted at `block`, clearing the pointers as it goes. Answers
/// whether the subtree is now empty, which is what tells the caller it may
/// free the root as well.
#[allow(clippy::too_many_arguments)]
fn truncate_indirect(
    block: BlockNum,
    depth: u32,
    from: u64,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    owner: BlockOwner,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<bool, Ext2Error> {
    if depth == 0 || !block.is_valid() {
        return Ok(false);
    }
    let count = cmp::min(ptrs_per_block as usize, 1024);
    let child_span = (ptrs_per_block as u64)
        .checked_pow(depth - 1)
        .ok_or(Ext2Error::InvalidBlock)?;

    // Heap-held, as in `free_indirect`: an inline pointer array per recursion
    // level puts 12 KiB on the stack at depth 3.
    let mut ptrs =
        slopos_ostd::KVec::<BlockNum>::zeroed(count).map_err(|_| Ext2Error::OutOfMemory)?;
    {
        let blk = cache.get_owned(block, device, owner)?;
        let data = blk.data();
        for (i, slot) in ptrs.as_mut_slice().iter_mut().enumerate() {
            let off = i * 4;
            *slot = BlockNum(u32::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
            ]));
        }
    }

    let mut cleared = false;
    for i in 0..count {
        let child = ptrs[i];
        if !child.is_valid() {
            continue;
        }
        let child_start = (i as u64) * child_span;
        if child_start + child_span <= from {
            continue;
        }
        let child_from = from.saturating_sub(child_start);
        let release = if depth > 1 {
            truncate_indirect(
                child,
                depth - 1,
                child_from,
                cache,
                device,
                ptrs_per_block,
                owner,
                free_fn,
            )?
        } else {
            true
        };
        if !release {
            continue;
        }
        free_fn(child)?;
        cache.invalidate(child);
        ptrs[i] = BlockNum::ZERO;
        cleared = true;
    }

    if cleared {
        let mut blk = cache.get_owned(block, device, owner)?;
        let data = blk.data_mut();
        for (i, ptr) in ptrs.as_slice().iter().enumerate() {
            data[i * 4..i * 4 + 4].copy_from_slice(&ptr.raw().to_le_bytes());
        }
    }

    Ok(ptrs.iter().all(|p| !p.is_valid()))
}

/// Free every block reachable through an indirect block at `depth`, leaving
/// the indirect block itself to the caller.
///
/// The pointer buffer is heap-allocated: at depth 3 an inline `[BlockNum;
/// 1024]` per frame would put 12 KiB across the recursion, well past the
/// kernel's bounded-stack budget.
pub(crate) fn free_indirect(
    block: BlockNum,
    depth: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    owner: BlockOwner,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    if depth == 0 || !block.is_valid() {
        return Ok(());
    }

    let count = cmp::min(ptrs_per_block as usize, 1024);
    let mut ptrs =
        slopos_ostd::KVec::<BlockNum>::zeroed(count).map_err(|_| Ext2Error::OutOfMemory)?;
    {
        let blk = cache.get_owned(block, device, owner)?;
        let data = blk.data();
        for i in 0..count {
            let off = i * 4;
            ptrs[i] = BlockNum(u32::from_le_bytes([
                data[off],
                data[off + 1],
                data[off + 2],
                data[off + 3],
            ]));
        }
    }

    for i in 0..count {
        if ptrs[i].is_valid() {
            if depth > 1 {
                free_indirect(
                    ptrs[i],
                    depth - 1,
                    cache,
                    device,
                    ptrs_per_block,
                    owner,
                    free_fn,
                )?;
            }
            free_fn(ptrs[i])?;
            cache.invalidate(ptrs[i]);
        }
    }

    Ok(())
}
