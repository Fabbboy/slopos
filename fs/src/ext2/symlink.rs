use super::Ext2Error;
use super::blockmap;
use super::cache::BlockCache;
use super::ondisk::{FAST_SYMLINK_MAX, Inode, MODE_SYMLINK};
use super::types::{BlockNum, FileBlock};
use crate::blockdev::BlockDevice;

/// Create a symlink inode pointing to `target`.
/// Returns the new inode data (caller must write it to disk and add the dir entry).
pub fn create_symlink_inode(
    target: &[u8],
    block_size: u32,
    alloc_block: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
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
        // Fast symlink: store target directly in inode.block[] (60 bytes
        // = 15 × u32). `BlockNum: Pod` lets us reinterpret the array as
        // bytes via the OSTD byte_view helper.
        let block_bytes =
            slopos_ostd::util::byte_view::pod_slice_as_bytes_mut(&mut inode.block[..]);
        block_bytes[..target.len()].copy_from_slice(target);
        // blocks stays 0 for fast symlinks
    } else {
        // Slow symlink: allocate a data block and store target there
        let data_block = alloc_block()?;
        let mut blk = cache.get_zero(data_block, device)?;
        let data = blk.data_mut();
        data[..target.len()].copy_from_slice(target);
        inode.block[0] = data_block;
        inode.blocks = block_size / 512;
    }

    Ok(inode)
}

/// Read the target of a symlink.
pub fn read_symlink(
    inode: &Inode,
    buf: &mut [u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    _block_size: u32,
) -> Result<usize, Ext2Error> {
    if !inode.is_symlink() {
        return Err(Ext2Error::NotFile);
    }

    let target_len = inode.size as usize;
    let copy_len = core::cmp::min(target_len, buf.len());

    if inode.is_fast_symlink() {
        // Read from inode.block[] reinterpreted as bytes via the OSTD
        // Pod byte view (15 × u32 → 60 bytes).
        let block_bytes = slopos_ostd::util::byte_view::pod_slice_as_bytes(&inode.block[..]);
        buf[..copy_len].copy_from_slice(&block_bytes[..copy_len]);
    } else {
        // Read from data block
        let phys = blockmap::map_block(inode, FileBlock(0), ptrs_per_block, cache, device)?;
        if !phys.is_valid() {
            return Err(Ext2Error::DeviceError);
        }
        let blk = cache.get(phys, device)?;
        buf[..copy_len].copy_from_slice(&blk.data()[..copy_len]);
    }

    Ok(copy_len)
}
