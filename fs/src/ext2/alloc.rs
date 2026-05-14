use super::Ext2Error;
use super::cache::BlockCache;
use super::ondisk::{GroupDesc, Superblock};
use super::types::{BlockNum, GroupIdx, InodeNum};
use crate::blockdev::BlockDevice;
use slopos_ostd::bitmap_slice;

/// Goal-directed block allocation: try the goal's group first, then scan.
pub fn allocate_block_near(
    goal: BlockNum,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<BlockNum, Ext2Error> {
    let groups_count = superblock.groups_count();
    if groups_count == 0 {
        return Err(Ext2Error::NoSpace);
    }
    let goal_group = if goal.is_valid() {
        GroupIdx(
            (goal.raw().saturating_sub(superblock.first_data_block.raw()))
                / superblock.blocks_per_group,
        )
    } else {
        GroupIdx(0)
    };

    // Try goal group first
    if let Some(block) =
        try_alloc_block_in_group(goal_group, superblock, cache, device, block_size)?
    {
        return Ok(block);
    }

    // Scan other groups
    for g in 0..groups_count {
        let group = GroupIdx(g);
        if group == goal_group {
            continue;
        }
        if let Some(block) = try_alloc_block_in_group(group, superblock, cache, device, block_size)?
        {
            return Ok(block);
        }
    }

    Err(Ext2Error::NoSpace)
}

/// Simple block allocation (no goal preference).
pub fn allocate_block(
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<BlockNum, Ext2Error> {
    allocate_block_near(BlockNum::ZERO, superblock, cache, device, block_size)
}

/// Allocate an inode, preferring the same group as the parent.
pub fn allocate_inode(
    parent_group: GroupIdx,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<InodeNum, Ext2Error> {
    let groups_count = superblock.groups_count();
    if groups_count == 0 {
        return Err(Ext2Error::NoSpace);
    }

    // Try parent's group first (locality for files in same directory)
    if let Some(ino) =
        try_alloc_inode_in_group(parent_group, superblock, cache, device, block_size)?
    {
        return Ok(ino);
    }

    // Scan other groups
    for g in 0..groups_count {
        let group = GroupIdx(g);
        if group == parent_group {
            continue;
        }
        if let Some(ino) = try_alloc_inode_in_group(group, superblock, cache, device, block_size)? {
            return Ok(ino);
        }
    }

    Err(Ext2Error::NoSpace)
}

/// Free a block: clear its bitmap bit and update counts.
pub fn free_block(
    block: BlockNum,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<(), Ext2Error> {
    let group = GroupIdx(
        (block
            .raw()
            .saturating_sub(superblock.first_data_block.raw()))
            / superblock.blocks_per_group,
    );
    let bit = (block
        .raw()
        .saturating_sub(superblock.first_data_block.raw()))
        % superblock.blocks_per_group;

    let mut desc = read_group_desc(group, superblock, cache, device, block_size)?;
    let bitmap_block = desc.block_bitmap;

    {
        let mut bmap = cache.get(bitmap_block, device)?;
        let data = bmap.data_mut();
        bitmap_slice::clear_bit(data, bit as usize);
    }

    desc.free_blocks_count = desc.free_blocks_count.saturating_add(1);
    write_group_desc(group, &desc, superblock, cache, device, block_size)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_add(1);

    Ok(())
}

/// Free an inode: clear its bitmap bit and update counts.
pub fn free_inode(
    ino: InodeNum,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<(), Ext2Error> {
    let group = ino.block_group(superblock.inodes_per_group);
    let bit = ino.local_index(superblock.inodes_per_group);

    let mut desc = read_group_desc(group, superblock, cache, device, block_size)?;
    let bitmap_block = desc.inode_bitmap;

    {
        let mut bmap = cache.get(bitmap_block, device)?;
        let data = bmap.data_mut();
        bitmap_slice::clear_bit(data, bit as usize);
    }

    desc.free_inodes_count = desc.free_inodes_count.saturating_add(1);
    write_group_desc(group, &desc, superblock, cache, device, block_size)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_add(1);

    Ok(())
}

// ---- Internal helpers ----

fn try_alloc_block_in_group(
    group: GroupIdx,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<Option<BlockNum>, Ext2Error> {
    let mut desc = read_group_desc(group, superblock, cache, device, block_size)?;
    if desc.free_blocks_count == 0 {
        return Ok(None);
    }

    let bitmap_block = desc.block_bitmap;
    let bits_in_group = superblock.blocks_per_group;

    let bit = {
        let bmap = cache.get(bitmap_block, device)?;
        bitmap_slice::find_first_zero(bmap.data(), bits_in_group as usize, 0)
    };

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get(bitmap_block, device)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }

    desc.free_blocks_count = desc.free_blocks_count.saturating_sub(1);
    write_group_desc(group, &desc, superblock, cache, device, block_size)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_sub(1);

    let block_num =
        group.raw() * superblock.blocks_per_group + bit as u32 + superblock.first_data_block.raw();
    Ok(Some(BlockNum(block_num)))
}

fn try_alloc_inode_in_group(
    group: GroupIdx,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<Option<InodeNum>, Ext2Error> {
    let mut desc = read_group_desc(group, superblock, cache, device, block_size)?;
    if desc.free_inodes_count == 0 {
        return Ok(None);
    }

    let bitmap_block = desc.inode_bitmap;
    let bits_in_group = superblock.inodes_per_group;
    let start_bit = if group.raw() == 0 {
        superblock.first_ino.saturating_sub(1) as usize
    } else {
        0
    };

    let bit = {
        let bmap = cache.get(bitmap_block, device)?;
        bitmap_slice::find_first_zero(bmap.data(), bits_in_group as usize, start_bit)
    };

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get(bitmap_block, device)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }

    desc.free_inodes_count = desc.free_inodes_count.saturating_sub(1);
    write_group_desc(group, &desc, superblock, cache, device, block_size)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_sub(1);

    let inode_num = group.raw() * superblock.inodes_per_group + bit as u32 + 1;
    Ok(Some(InodeNum(inode_num)))
}

fn group_desc_offset(
    group: GroupIdx,
    _superblock: &Superblock,
    block_size: u32,
) -> (BlockNum, usize) {
    let table_start = if block_size == 1024 { 2 } else { 1 };
    let desc_per_block = block_size as usize / 32; // group desc is 32 bytes
    let block_idx = group.raw() as usize / desc_per_block;
    let within = (group.raw() as usize % desc_per_block) * 32;
    (BlockNum(table_start + block_idx as u32), within)
}

fn read_group_desc(
    group: GroupIdx,
    superblock: &Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<GroupDesc, Ext2Error> {
    let (block, offset) = group_desc_offset(group, superblock, block_size);
    let blk = cache.get(block, device)?;
    Ok(GroupDesc::parse(&blk.data()[offset..offset + 32]))
}

fn write_group_desc(
    group: GroupIdx,
    desc: &GroupDesc,
    superblock: &Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
) -> Result<(), Ext2Error> {
    let (block, offset) = group_desc_offset(group, superblock, block_size);
    let mut blk = cache.get(block, device)?;
    desc.encode(&mut blk.data_mut()[offset..offset + 32]);
    Ok(())
}
