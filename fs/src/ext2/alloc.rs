use super::Ext2Error;
use super::cache::{BlockCache, BlockOwner};
use super::geometry::Ext2Geometry;
use super::ondisk::{GROUP_DESC_SIZE, GroupDesc, Superblock};
use super::types::{BlockNum, GroupIdx, InodeNum};
use crate::blockdev::BlockDevice;
use slopos_ostd::bitmap_slice;

/// ext2's own denial-of-service answer: `s_r_blocks_count` blocks are spendable
/// only by a writer entitled to them, so a process that fills the disk still
/// leaves the reserve for `/sbin/init` and the kernel's own writes. A handle
/// whose caller is entitled carries a reserve of zero.
fn reserve_permits_allocation(geom: &Ext2Geometry, superblock: &Superblock) -> bool {
    superblock.free_blocks_count > geom.reserved_blocks()
}

pub fn allocate_block_near(
    goal: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    if !reserve_permits_allocation(geom, superblock) {
        return Err(Ext2Error::NoSpace);
    }
    // Charged before the search, so a caller over its ceiling costs no bitmap
    // work, and given back below if the search finds nothing. The reserve
    // above is a system floor; this is the per-principal ceiling.
    cache.charge_blocks(geom.account(), owner.charged_inode(), 1)?;
    let allocated = allocate_searching(goal, geom, superblock, cache, device);
    if allocated.is_err() {
        cache.cancel_block_charge(geom.account(), owner.charged_inode(), 1);
    }
    allocated
}

fn allocate_searching(
    goal: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<BlockNum, Ext2Error> {
    let first_group = geom.group(0).ok_or(Ext2Error::NoSpace)?;
    let goal_group = geom
        .locate_block(goal)
        .map(|(g, _)| g)
        .unwrap_or(first_group);

    if let Some(block) = try_alloc_block_in_group(goal_group, geom, superblock, cache, device)? {
        return Ok(block);
    }

    for g in 0..geom.groups_count() {
        let Some(group) = geom.group(g) else {
            continue;
        };
        if group == goal_group {
            continue;
        }
        if let Some(block) = try_alloc_block_in_group(group, geom, superblock, cache, device)? {
            return Ok(block);
        }
    }

    Err(Ext2Error::NoSpace)
}

pub fn allocate_block(
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<BlockNum, Ext2Error> {
    allocate_block_near(BlockNum::ZERO, geom, superblock, cache, device, owner)
}

pub fn allocate_inode(
    parent_group: GroupIdx,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<InodeNum, Ext2Error> {
    if superblock.free_inodes_count <= geom.reserved_inodes() {
        return Err(Ext2Error::NoSpace);
    }
    // Locality: files in one directory belong in one group.
    if let Some(ino) = try_alloc_inode_in_group(parent_group, geom, superblock, cache, device)? {
        return Ok(ino);
    }

    for g in 0..geom.groups_count() {
        let Some(group) = geom.group(g) else {
            continue;
        };
        if group == parent_group {
            continue;
        }
        if let Some(ino) = try_alloc_inode_in_group(group, geom, superblock, cache, device)? {
            return Ok(ino);
        }
    }

    Err(Ext2Error::NoSpace)
}

pub fn free_block(
    block: BlockNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    let (group, bit) = geom.locate_block(block).ok_or(Ext2Error::InvalidBlock)?;
    // Before the bitmap moves: no earlier log record may be replayed into
    // this block, because the next allocation may hand it out as file data.
    cache.note_revoke(block, device)?;
    // Deferred to the commit: a rollback restores the bitmap, so an operation
    // that frees and then fails still owes the block. Credited to the
    // principal charged for it, not to this caller.
    cache.note_blocks_freed(owner.charged_inode(), 1);

    let mut desc = read_group_desc(group, geom, cache, device)?;
    let bitmap_block = desc.block_bitmap;

    {
        let mut bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        let data = bmap.data_mut();
        bitmap_slice::clear_bit(data, bit as usize);
    }

    desc.free_blocks_count = desc.free_blocks_count.saturating_add(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_add(1);

    Ok(())
}

pub fn free_inode(
    ino: InodeNum,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    let (group, bit) = geom.locate_inode(ino).ok_or(Ext2Error::InvalidInode)?;

    let mut desc = read_group_desc(group, geom, cache, device)?;
    let bitmap_block = desc.inode_bitmap;

    {
        let mut bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        let data = bmap.data_mut();
        bitmap_slice::clear_bit(data, bit as usize);
    }

    desc.free_inodes_count = desc.free_inodes_count.saturating_add(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_add(1);

    Ok(())
}

fn try_alloc_block_in_group(
    group: GroupIdx,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<Option<BlockNum>, Ext2Error> {
    let mut desc = read_group_desc(group, geom, cache, device)?;
    if desc.free_blocks_count == 0 {
        return Ok(None);
    }

    let bitmap_block = desc.block_bitmap;
    let bits_in_group = geom.blocks_per_group();

    let bit = {
        let bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        bitmap_slice::find_first_zero(bmap.data(), bits_in_group as usize, 0)
    };

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }

    let Some(block_num) = geom.block_of(group, bit as u32) else {
        return Err(Ext2Error::InvalidBlock);
    };

    desc.free_blocks_count = desc.free_blocks_count.saturating_sub(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_blocks_count = superblock.free_blocks_count.saturating_sub(1);

    Ok(Some(block_num))
}

fn try_alloc_inode_in_group(
    group: GroupIdx,
    geom: &Ext2Geometry,
    superblock: &mut Superblock,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<Option<InodeNum>, Ext2Error> {
    let mut desc = read_group_desc(group, geom, cache, device)?;
    if desc.free_inodes_count == 0 {
        return Ok(None);
    }

    let bitmap_block = desc.inode_bitmap;
    let bits_in_group = geom.inodes_per_group();
    let start_bit = if group.raw() == 0 {
        superblock.first_ino.saturating_sub(1) as usize
    } else {
        0
    };

    let bit = {
        let bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        bitmap_slice::find_first_zero(bmap.data(), bits_in_group as usize, start_bit)
    };

    let Some(bit) = bit else {
        return Ok(None);
    };

    {
        let mut bmap = cache.get_owned(bitmap_block, device, BlockOwner::Alloc)?;
        bitmap_slice::set_bit(bmap.data_mut(), bit);
    }

    let Some(inode_num) = geom.inode_of(group, bit as u32) else {
        return Err(Ext2Error::InvalidInode);
    };

    desc.free_inodes_count = desc.free_inodes_count.saturating_sub(1);
    write_group_desc(group, &desc, geom, cache, device)?;
    superblock.free_inodes_count = superblock.free_inodes_count.saturating_sub(1);

    Ok(Some(inode_num))
}

pub(crate) fn read_group_desc(
    group: GroupIdx,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<GroupDesc, Ext2Error> {
    let loc = geom.group_desc_loc(group);
    let blk = cache.get_owned(loc.block(), device, BlockOwner::Alloc)?;
    let window = blk
        .window::<GROUP_DESC_SIZE>(loc.within())
        .ok_or(Ext2Error::InvalidBlock)?;
    let desc = GroupDesc::parse(window);
    geom.validate_desc(group, &desc)?;
    Ok(desc)
}

pub(crate) fn write_group_desc(
    group: GroupIdx,
    desc: &GroupDesc,
    geom: &Ext2Geometry,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
) -> Result<(), Ext2Error> {
    geom.validate_desc(group, desc)?;
    let loc = geom.group_desc_loc(group);
    let mut blk = cache.get_owned(loc.block(), device, BlockOwner::Alloc)?;
    let window = blk
        .window_mut::<GROUP_DESC_SIZE>(loc.within())
        .ok_or(Ext2Error::InvalidBlock)?;
    desc.encode(window);
    Ok(())
}
