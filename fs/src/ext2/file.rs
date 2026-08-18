use super::Ext2Error;
use super::blockmap;
use super::cache::BlockCache;
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
            let blk = cache.get_data(phys, device)?;
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

        let (phys, newly_allocated) = blockmap::ensure_data_block(
            inode,
            fb,
            ptrs_per_block,
            cache,
            device,
            superblock,
            block_size,
        )?;

        let mut blk = cache.get_data(phys, device)?;
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
        inode.size = new_size as u32;
        return Ok(());
    }

    let first_keep = ((new_size + block_size as u64 - 1) / block_size as u64) as u32;

    for i in first_keep
        ..cmp::min(
            ((old_size + block_size as u64 - 1) / block_size as u64) as u32,
            12,
        )
    {
        let blk = inode.block[i as usize];
        if blk.is_valid() {
            free_fn(blk)?;
            cache.invalidate(blk);
            inode.block[i as usize] = BlockNum::ZERO;
        }
    }

    if first_keep <= 12 && inode.block[12].is_valid() {
        free_indirect(inode.block[12], 1, cache, device, ptrs_per_block, free_fn)?;
        free_fn(inode.block[12])?;
        cache.invalidate(inode.block[12]);
        inode.block[12] = BlockNum::ZERO;
    }

    if first_keep <= 12 + ptrs_per_block && inode.block[13].is_valid() {
        free_indirect(inode.block[13], 2, cache, device, ptrs_per_block, free_fn)?;
        free_fn(inode.block[13])?;
        cache.invalidate(inode.block[13]);
        inode.block[13] = BlockNum::ZERO;
    }

    let ti_start = 12 + ptrs_per_block + ptrs_per_block * ptrs_per_block;
    if first_keep <= ti_start && inode.block[14].is_valid() {
        free_indirect(inode.block[14], 3, cache, device, ptrs_per_block, free_fn)?;
        free_fn(inode.block[14])?;
        cache.invalidate(inode.block[14]);
        inode.block[14] = BlockNum::ZERO;
    }

    inode.size = new_size as u32;
    let sectors_per_block = block_size / 512;
    let mut count = 0u32;
    for blk in &inode.block[..12] {
        if blk.is_valid() {
            count += 1;
        }
    }
    inode.blocks = count * sectors_per_block;

    Ok(())
}

fn free_indirect(
    block: BlockNum,
    depth: u32,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    free_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    if depth == 0 || !block.is_valid() {
        return Ok(());
    }

    let mut ptrs = [BlockNum::ZERO; 1024];
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
                free_indirect(ptrs[i], depth - 1, cache, device, ptrs_per_block, free_fn)?;
            }
            free_fn(ptrs[i])?;
            cache.invalidate(ptrs[i]);
        }
    }

    Ok(())
}
