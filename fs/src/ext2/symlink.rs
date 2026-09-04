use super::Ext2Error;
use super::blockmap;
use super::cache::{BlockCache, BlockOwner};
use super::ondisk::{FAST_SYMLINK_MAX, Inode, MODE_SYMLINK};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;

/// Create a symlink inode pointing to `target`. The caller must write it to
/// disk and add the directory entry.
pub fn create_symlink_inode(
    target: &[u8],
    block_size: u32,
    alloc_block: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    owner: BlockOwner,
) -> Result<Inode, Ext2Error> {
    if target.is_empty() {
        return Err(Ext2Error::PathNotFound);
    }

    let mut inode = Inode {
        mode: MODE_SYMLINK | 0o777,
        uid: 0,
        size: target.len() as u32,
        atime: 0,
        ctime: 0,
        mtime: 0,
        dtime: 0,
        gid: 0,
        links_count: 1,
        blocks: 0,
        flags: 0,
        block: [BlockNum::ZERO; 15],
    };

    if target.len() <= FAST_SYMLINK_MAX {
        // Fast symlink: the target lives in inode.block[] itself, 15 × u32.
        let block_bytes =
            slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(&mut inode.block[..]);
        block_bytes[..target.len()].copy_from_slice(target);
        // blocks stays 0 for fast symlinks
    } else {
        let data_block = alloc_block()?;
        let mut blk = cache.get_zero_data(data_block, device, owner)?;
        let data = blk.data_mut();
        data[..target.len()].copy_from_slice(target);
        inode.block[0] = data_block;
        inode.blocks = block_size / 512;
    }

    Ok(inode)
}

pub fn read_symlink(
    inode: &Inode,
    buf: &mut [u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    _block_size: u32,
    owner: BlockOwner,
) -> Result<usize, Ext2Error> {
    if !inode.is_symlink() {
        return Err(Ext2Error::NotFile);
    }

    let target_len = inode.size as usize;
    let copy_len = core::cmp::min(target_len, buf.len());

    if inode.is_fast_symlink() {
        let block_bytes = slopos_ostd::util::byte_view::pod_slice_as_bytes(&inode.block[..]);
        buf[..copy_len].copy_from_slice(&block_bytes[..copy_len]);
    } else {
        let phys = blockmap::map_block(inode, FileBlock(0), ptrs_per_block, cache, device, owner)?;
        if !phys.is_valid() {
            return Err(Ext2Error::DeviceError);
        }
        let blk = cache.get_data(phys, device, owner)?;
        buf[..copy_len].copy_from_slice(&blk.data()[..copy_len]);
    }

    Ok(copy_len)
}
