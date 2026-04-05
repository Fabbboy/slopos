use super::Ext2Error;
use super::cache::BlockCache;
use super::dir;
use super::file;
use super::ondisk::{
    DIR_FT_DIR, DIR_FT_REG_FILE, Inode, MODE_DIRECTORY, MODE_FILE, write_dir_entry,
};
use super::types::{BlockNum, InodeNum};
use crate::blockdev::BlockDevice;

/// Create a new regular file in a directory.
pub fn create_file(
    _parent_num: InodeNum,
    name: &[u8],
    parent: &mut Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    alloc_inode: &mut dyn FnMut() -> Result<InodeNum, Ext2Error>,
    alloc_block: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
) -> Result<InodeNum, Ext2Error> {
    if name.is_empty() || name.len() > 255 {
        return Err(Ext2Error::NameTooLong);
    }
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }

    let new_ino = alloc_inode()?;
    let new_inode = Inode {
        mode: MODE_FILE | 0o644,
        uid: 0,
        size: 0,
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

    write_inode_to_disk(
        new_ino,
        &new_inode,
        cache,
        device,
        block_size,
        ptrs_per_block,
    )?;

    dir::append_dir_entry(
        parent,
        new_ino,
        name,
        DIR_FT_REG_FILE,
        cache,
        device,
        ptrs_per_block,
        block_size,
        alloc_block,
    )?;

    Ok(new_ino)
}

/// Create a new directory.
pub fn create_directory(
    parent_num: InodeNum,
    name: &[u8],
    parent: &mut Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    alloc_inode: &mut dyn FnMut() -> Result<InodeNum, Ext2Error>,
    alloc_block: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
) -> Result<InodeNum, Ext2Error> {
    if name.is_empty() || name.len() > 255 {
        return Err(Ext2Error::NameTooLong);
    }
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }

    let new_ino = alloc_inode()?;
    let first_block = alloc_block()?;

    // Write . and .. entries into the first data block
    {
        let mut blk = cache.get_zero(first_block, device)?;
        let data = blk.data_mut();
        let bs = block_size as usize;
        // "." entry
        let dot_rec_len = 12; // header(8) + name(1) + padding
        write_dir_entry(
            &mut data[..dot_rec_len],
            new_ino,
            b".",
            DIR_FT_DIR,
            dot_rec_len,
        );
        // ".." entry (takes rest of block)
        let dotdot_rec_len = bs - dot_rec_len;
        write_dir_entry(
            &mut data[dot_rec_len..dot_rec_len + dotdot_rec_len],
            parent_num,
            b"..",
            DIR_FT_DIR,
            dotdot_rec_len,
        );
    }

    let mut new_inode = Inode {
        mode: MODE_DIRECTORY | 0o755,
        uid: 0,
        size: block_size,
        atime: 0,
        ctime: 0,
        mtime: 0,
        dtime: 0,
        gid: 0,
        links_count: 2, // . and parent's entry
        blocks: block_size / 512,
        flags: 0,
        block: [BlockNum::ZERO; 15],
    };
    new_inode.block[0] = first_block;

    write_inode_to_disk(
        new_ino,
        &new_inode,
        cache,
        device,
        block_size,
        ptrs_per_block,
    )?;

    dir::append_dir_entry(
        parent,
        new_ino,
        name,
        DIR_FT_DIR,
        cache,
        device,
        ptrs_per_block,
        block_size,
        alloc_block,
    )?;

    parent.links_count += 1; // for the .. reference

    Ok(new_ino)
}

/// Unlink a file or remove an empty directory.
pub fn unlink(
    parent: &mut Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    read_inode_fn: &mut dyn FnMut(InodeNum) -> Result<Inode, Ext2Error>,
    _write_inode_fn: &mut dyn FnMut(InodeNum, &Inode) -> Result<(), Ext2Error>,
    free_block_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
    free_inode_fn: &mut dyn FnMut(InodeNum) -> Result<(), Ext2Error>,
) -> Result<(), Ext2Error> {
    let target_num = dir::lookup_child(parent, name, cache, device, ptrs_per_block, block_size)?;
    let target = read_inode_fn(target_num)?;

    if target.is_directory() {
        if !dir::is_dir_empty(&target, cache, device, ptrs_per_block, block_size)? {
            return Err(Ext2Error::NotEmpty);
        }
    }

    dir::remove_dir_entry(parent, name, cache, device, ptrs_per_block, block_size)?;

    // Free all data blocks
    let mut target_mut = target;
    file::truncate(
        &mut target_mut,
        0,
        cache,
        device,
        ptrs_per_block,
        block_size,
        free_block_fn,
    )?;

    // Free the inode
    free_inode_fn(target_num)?;

    if target.is_directory() {
        parent.links_count = parent.links_count.saturating_sub(1);
    }

    Ok(())
}

/// Rename a file or directory from old_parent/old_name to new_parent/new_name.
pub fn rename(
    old_parent: &mut Inode,
    old_name: &[u8],
    new_parent: &mut Inode,
    new_name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    new_parent_num: InodeNum,
    read_inode_fn: &mut dyn FnMut(InodeNum) -> Result<Inode, Ext2Error>,
    _write_inode_fn: &mut dyn FnMut(InodeNum, &Inode) -> Result<(), Ext2Error>,
    free_block_fn: &mut dyn FnMut(BlockNum) -> Result<(), Ext2Error>,
    free_inode_fn: &mut dyn FnMut(InodeNum) -> Result<(), Ext2Error>,
    alloc_block: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
) -> Result<(), Ext2Error> {
    let src_num = dir::lookup_child(
        old_parent,
        old_name,
        cache,
        device,
        ptrs_per_block,
        block_size,
    )?;
    let src_inode = read_inode_fn(src_num)?;
    let file_type = if src_inode.is_directory() {
        DIR_FT_DIR
    } else {
        DIR_FT_REG_FILE
    };

    // Check if target already exists
    if let Ok(existing_num) = dir::lookup_child(
        new_parent,
        new_name,
        cache,
        device,
        ptrs_per_block,
        block_size,
    ) {
        let existing = read_inode_fn(existing_num)?;
        if existing.is_directory() {
            if !dir::is_dir_empty(&existing, cache, device, ptrs_per_block, block_size)? {
                return Err(Ext2Error::NotEmpty);
            }
        }
        // Remove existing target
        dir::remove_dir_entry(
            new_parent,
            new_name,
            cache,
            device,
            ptrs_per_block,
            block_size,
        )?;
        let mut existing_mut = existing;
        file::truncate(
            &mut existing_mut,
            0,
            cache,
            device,
            ptrs_per_block,
            block_size,
            free_block_fn,
        )?;
        free_inode_fn(existing_num)?;
        if existing.is_directory() {
            new_parent.links_count = new_parent.links_count.saturating_sub(1);
        }
    }

    // Add new entry first (crash-safe: entry exists before old one is removed)
    dir::append_dir_entry(
        new_parent,
        src_num,
        new_name,
        file_type,
        cache,
        device,
        ptrs_per_block,
        block_size,
        alloc_block,
    )?;

    // Remove old entry
    dir::remove_dir_entry(
        old_parent,
        old_name,
        cache,
        device,
        ptrs_per_block,
        block_size,
    )?;

    // If directory: update .. to point to new parent, fix link counts
    if src_inode.is_directory() {
        dir::update_dotdot(
            &src_inode,
            new_parent_num,
            cache,
            device,
            ptrs_per_block,
            block_size,
        )?;
        old_parent.links_count = old_parent.links_count.saturating_sub(1);
        new_parent.links_count += 1;
    }

    Ok(())
}

// ---- Internal helpers ----

fn write_inode_to_disk(
    ino: InodeNum,
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    block_size: u32,
    ptrs_per_block: u32,
) -> Result<(), Ext2Error> {
    // This is a simplified version: read the inode table block, encode the inode,
    // write it back via cache. In practice this would go through the Ext2Fs handle.
    // For now this is a placeholder that will be wired through the VFS adapter.
    let _ = (ino, inode, cache, device, block_size, ptrs_per_block);
    Ok(())
}
