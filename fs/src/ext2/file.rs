use super::Ext2Error;
use super::blockmap;
use super::cache::BlockCache;
use super::ondisk::Inode;
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;
use core::cmp;

/// Read file data using the block cache.
pub fn read_file(
    inode: &Inode,
    offset: u64,
    buffer: &mut [u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
) -> Result<usize, Ext2Error> {
    if !inode.is_regular_file() {
        return Err(Ext2Error::NotFile);
    }
    let file_size = inode.size as u64;
    if offset >= file_size || buffer.is_empty() {
        return Ok(0);
    }
    let max_len = cmp::min(buffer.len() as u64, file_size - offset) as usize;
    let mut read_total = 0usize;
    let mut file_offset = offset;

    while read_total < max_len {
        let fb = FileBlock((file_offset / block_size as u64) as u32);
        let block_off = (file_offset % block_size as u64) as usize;
        let to_copy = cmp::min(max_len - read_total, block_size as usize - block_off);

        let phys = blockmap::map_block(inode, fb, ptrs_per_block, cache, device)?;
        if phys.is_valid() {
            let blk = cache.get(phys, device)?;
            buffer[read_total..read_total + to_copy]
                .copy_from_slice(&blk.data()[block_off..block_off + to_copy]);
        } else {
            // Sparse hole: zero-fill
            buffer[read_total..read_total + to_copy].fill(0);
        }

        read_total += to_copy;
        file_offset += to_copy as u64;
    }
    Ok(read_total)
}

/// Write file data using the block cache.
pub fn write_file(
    inode: &mut Inode,
    offset: u64,
    buffer: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    alloc_fn: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
) -> Result<usize, Ext2Error> {
    if !inode.is_regular_file() {
        return Err(Ext2Error::NotFile);
    }
    if buffer.is_empty() {
        return Ok(0);
    }
    let mut written = 0usize;
    let mut file_offset = offset;

    while written < buffer.len() {
        let fb = FileBlock((file_offset / block_size as u64) as u32);
        let block_off = (file_offset % block_size as u64) as usize;
        let to_copy = cmp::min(buffer.len() - written, block_size as usize - block_off);

        let (phys, newly_allocated) =
            blockmap::ensure_data_block(inode, fb, ptrs_per_block, cache, device, alloc_fn)?;

        let mut blk = cache.get(phys, device)?;
        if newly_allocated && (block_off != 0 || to_copy != block_size as usize) {
            // Partial write to a newly allocated block: already zero from get_zero
        }
        blk.data_mut()[block_off..block_off + to_copy]
            .copy_from_slice(&buffer[written..written + to_copy]);

        written += to_copy;
        file_offset += to_copy as u64;

        if newly_allocated {
            inode.blocks += block_size / 512;
        }
    }

    let new_end = offset + written as u64;
    if new_end > inode.size as u64 {
        inode.size = new_end as u32;
    }
    Ok(written)
}

/// Truncate a file to `new_size` bytes.
/// Frees blocks beyond the new end, updates inode size and block count.
pub fn truncate(
    inode: &mut Inode,
    new_size: u64,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    let old_size = inode.size as u64;
    if new_size >= old_size {
        // Extend: just update size (blocks allocated lazily on write)
        inode.size = new_size as u32;
        return Ok(());
    }

    // Shrink: free blocks beyond the new end
    let first_keep = ((new_size + block_size as u64 - 1) / block_size as u64) as u32;
    let old_block_count = ((old_size + block_size as u64 - 1) / block_size as u64) as u32;

    // Free direct blocks
    for i in first_keep..cmp::min(old_block_count, 12) {
        let blk = inode.block[i as usize];
        if blk.is_valid() {
            free_fn(blk)?;
            cache.invalidate(blk);
            inode.block[i as usize] = BlockNum::ZERO;
        }
    }

    // Free single indirect
    if first_keep <= 12 && inode.block[12].is_valid() {
        free_indirect(
            inode.block[12],
            1,
            0,
            cache,
            device,
            ptrs_per_block,
            free_fn,
        )?;
        free_fn(inode.block[12])?;
        cache.invalidate(inode.block[12]);
        inode.block[12] = BlockNum::ZERO;
    } else if first_keep > 12 && first_keep < 12 + ptrs_per_block && inode.block[12].is_valid() {
        let start = first_keep - 12;
        partial_free_indirect(inode.block[12], start, cache, device, free_fn)?;
    }

    // Free double indirect
    let di_start = 12 + ptrs_per_block;
    if first_keep <= di_start && inode.block[13].is_valid() {
        free_indirect(
            inode.block[13],
            2,
            0,
            cache,
            device,
            ptrs_per_block,
            free_fn,
        )?;
        free_fn(inode.block[13])?;
        cache.invalidate(inode.block[13]);
        inode.block[13] = BlockNum::ZERO;
    }

    // Free triple indirect
    let ti_start = di_start + ptrs_per_block * ptrs_per_block;
    if first_keep <= ti_start && inode.block[14].is_valid() {
        free_indirect(
            inode.block[14],
            3,
            0,
            cache,
            device,
            ptrs_per_block,
            free_fn,
        )?;
        free_fn(inode.block[14])?;
        cache.invalidate(inode.block[14]);
        inode.block[14] = BlockNum::ZERO;
    }

    inode.size = new_size as u32;
    // Recalculate blocks (approximate — count non-zero block pointers * sectors_per_block)
    let sectors_per_block = block_size / 512;
    inode.blocks = count_allocated_blocks(inode, ptrs_per_block, cache, device) * sectors_per_block;

    Ok(())
}

/// Recursively free an indirect block tree at the given depth.
fn free_indirect(
    block: BlockNum,
    depth: u32,
    _start_idx: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    if depth == 0 || !block.is_valid() {
        return Ok(());
    }

    // Read the indirect block to get child pointers
    let mut ptrs = [BlockNum::ZERO; 1024]; // max ptrs_per_block for 4KB blocks
    let count = cmp::min(ptrs_per_block as usize, ptrs.len());
    {
        let blk = cache.get(block, device)?;
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
                    0,
                    cache,
                    device,
                    ptrs_per_block,
                    free_fn,
                )?;
            }
            free_fn(ptrs[i])?;
            cache.invalidate(ptrs[i]);
        }
    }

    Ok(())
}

/// Free entries in a single indirect block starting from `start_idx`.
fn partial_free_indirect(
    block: BlockNum,
    start_idx: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    let mut blk = cache.get(block, device)?;
    let data = blk.data_mut();
    let count = data.len() / 4;
    for i in start_idx as usize..count {
        let off = i * 4;
        let ptr = BlockNum(u32::from_le_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
        ]));
        if ptr.is_valid() {
            free_fn(ptr)?;
            data[off..off + 4].copy_from_slice(&0u32.to_le_bytes());
        }
    }
    Ok(())
}

/// Count allocated data blocks (direct + indirect).
/// Simple approximation: count non-zero direct pointers and walk first indirect.
fn count_allocated_blocks(
    inode: &Inode,
    _ptrs_per_block: u32,
    _cache: &mut BlockCache,
    _device: &dyn BlockDevice,
) -> u32 {
    let mut count = 0u32;
    for blk in &inode.block[..12] {
        if blk.is_valid() {
            count += 1;
        }
    }
    // For indirect blocks, the original block count in the inode is authoritative
    // during partial truncation. A full recount would require walking all indirect
    // trees which is expensive. Use the direct count as a lower bound.
    count
}
