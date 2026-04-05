use super::Ext2Error;
use super::blockmap;
use super::cache::BlockCache;
use super::ondisk::{DIR_ENTRY_HEADER_SIZE, DirEntry, Inode, dir_entry_size, write_dir_entry};
use super::types::{BlockNum, FileBlock, InodeNum};
use crate::blockdev::BlockDevice;
use core::cmp;

/// Iterate directory entries, calling `f` for each valid entry.
/// Returns early if `f` returns `false`.
pub fn for_each_entry(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    f: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<(), Ext2Error> {
    if !inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let mut offset = 0u32;
    while offset < inode.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(inode, file_block, ptrs_per_block, cache, device)?;
        if !phys.is_valid() {
            break;
        }
        let block = cache.get(phys, device)?;
        let data = block.data();
        let mut cursor = 0usize;
        while cursor + DIR_ENTRY_HEADER_SIZE <= block_size as usize {
            let entry_inode = u32::from_le_bytes([
                data[cursor],
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
            ]);
            let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
            let name_len = data[cursor + 6] as usize;
            let file_type = data[cursor + 7];
            if rec_len < DIR_ENTRY_HEADER_SIZE || cursor + rec_len > block_size as usize {
                return Err(Ext2Error::DirectoryFormat);
            }
            if entry_inode != 0 {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + name_len;
                if name_end > cursor + rec_len {
                    return Err(Ext2Error::DirectoryFormat);
                }
                let entry = DirEntry {
                    inode: InodeNum(entry_inode),
                    file_type,
                    name: &data[name_start..name_end],
                };
                if !f(entry) {
                    return Ok(());
                }
            }
            cursor += rec_len;
        }
        offset += block_size;
    }
    Ok(())
}

/// Look up a child inode by name in a directory.
pub fn lookup_child(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
) -> Result<InodeNum, Ext2Error> {
    let mut found = None;
    for_each_entry(
        parent,
        cache,
        device,
        ptrs_per_block,
        block_size,
        &mut |entry| {
            if entry.name == name {
                found = Some(entry.inode);
                false
            } else {
                true
            }
        },
    )?;
    found.ok_or(Ext2Error::PathNotFound)
}

/// Remove a directory entry by name with correct rec_len merging.
///
/// Per ext2 spec: extend the predecessor's rec_len to absorb the deleted entry.
/// For the first entry in a block (no predecessor), zero the inode field.
pub fn remove_dir_entry(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
) -> Result<(), Ext2Error> {
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let bs = block_size as usize;
    let mut offset = 0u32;
    while offset < parent.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(parent, file_block, ptrs_per_block, cache, device)?;
        if !phys.is_valid() {
            break;
        }
        let mut block = cache.get(phys, device)?;
        let data = block.data_mut();

        let mut cursor = 0usize;
        let mut prev_cursor: Option<usize> = None;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let entry_inode = u32::from_le_bytes([
                data[cursor],
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
            ]);
            let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
            let name_len = data[cursor + 6] as usize;

            if rec_len < DIR_ENTRY_HEADER_SIZE || cursor + rec_len > bs {
                return Err(Ext2Error::DirectoryFormat);
            }

            if entry_inode != 0 {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + cmp::min(name_len, rec_len - DIR_ENTRY_HEADER_SIZE);
                if &data[name_start..name_end] == name {
                    match prev_cursor {
                        Some(prev) => {
                            // Extend predecessor's rec_len to absorb this entry
                            let prev_rec =
                                u16::from_le_bytes([data[prev + 4], data[prev + 5]]) as usize;
                            let new_rec = (prev_rec + rec_len) as u16;
                            data[prev + 4..prev + 6].copy_from_slice(&new_rec.to_le_bytes());
                        }
                        None => {
                            // First entry in block: zero the inode field
                            data[cursor..cursor + 4].copy_from_slice(&0u32.to_le_bytes());
                        }
                    }
                    return Ok(());
                }
                prev_cursor = Some(cursor);
            }
            cursor += rec_len;
        }
        offset += block_size;
    }
    Err(Ext2Error::PathNotFound)
}

/// Check if a directory is empty (only contains . and ..).
pub fn is_dir_empty(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
) -> Result<bool, Ext2Error> {
    let mut count = 0u32;
    for_each_entry(
        inode,
        cache,
        device,
        ptrs_per_block,
        block_size,
        &mut |entry| {
            if entry.name == b"." || entry.name == b".." {
                count += 1;
                true
            } else {
                count += 1;
                false // found non-dot entry, stop
            }
        },
    )?;
    Ok(count <= 2)
}

/// Append a directory entry to a parent directory.
/// Scans existing blocks for free space, allocates a new block if needed.
pub fn append_dir_entry(
    parent_inode: &mut Inode,
    child: InodeNum,
    name: &[u8],
    file_type: u8,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    alloc_fn: &mut dyn FnMut() -> Result<BlockNum, Ext2Error>,
) -> Result<(), Ext2Error> {
    let needed = dir_entry_size(name.len());
    let bs = block_size as usize;

    // Scan existing directory blocks for space
    let mut offset = 0u32;
    while offset < parent_inode.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(parent_inode, file_block, ptrs_per_block, cache, device)?;
        if !phys.is_valid() {
            break;
        }
        let mut block = cache.get(phys, device)?;
        let data = block.data_mut();
        let mut cursor = 0usize;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
            if rec_len < DIR_ENTRY_HEADER_SIZE || cursor + rec_len > bs {
                return Err(Ext2Error::DirectoryFormat);
            }
            let entry_inode = u32::from_le_bytes([
                data[cursor],
                data[cursor + 1],
                data[cursor + 2],
                data[cursor + 3],
            ]);
            let name_len = data[cursor + 6] as usize;

            let actual_size = if entry_inode != 0 {
                dir_entry_size(name_len)
            } else {
                DIR_ENTRY_HEADER_SIZE
            };
            let slack = rec_len - actual_size;

            if slack >= needed {
                if entry_inode != 0 {
                    // Shrink existing entry, place new entry after it
                    data[cursor + 4..cursor + 6]
                        .copy_from_slice(&(actual_size as u16).to_le_bytes());
                    let new_cursor = cursor + actual_size;
                    let new_rec_len = rec_len - actual_size;
                    write_dir_entry(
                        &mut data[new_cursor..new_cursor + new_rec_len],
                        child,
                        name,
                        file_type,
                        new_rec_len,
                    );
                } else {
                    // Reuse deleted slot
                    write_dir_entry(
                        &mut data[cursor..cursor + rec_len],
                        child,
                        name,
                        file_type,
                        rec_len,
                    );
                }
                return Ok(());
            }
            cursor += rec_len;
        }
        offset += block_size;
    }

    // No space found: allocate a new block
    let new_block = alloc_fn()?;
    let file_block = FileBlock(parent_inode.size / block_size);

    // Store the new block pointer in the inode's block map
    blockmap::ensure_data_block(
        parent_inode,
        file_block,
        ptrs_per_block,
        cache,
        device,
        &mut || Ok(new_block),
    )?;

    // Write the new entry into the fresh block
    let mut block = cache.get_zero(new_block, device)?;
    let data = block.data_mut();
    write_dir_entry(&mut data[..bs], child, name, file_type, bs);

    parent_inode.size += block_size;
    parent_inode.blocks += block_size / 512;

    Ok(())
}

/// Update the ".." entry in a directory to point to a new parent.
pub fn update_dotdot(
    dir_inode: &Inode,
    new_parent: InodeNum,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
) -> Result<(), Ext2Error> {
    if !dir_inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let phys = blockmap::map_block(dir_inode, FileBlock(0), ptrs_per_block, cache, device)?;
    if !phys.is_valid() {
        return Err(Ext2Error::DirectoryFormat);
    }
    let mut block = cache.get(phys, device)?;
    let data = block.data_mut();
    let bs = block_size as usize;

    // Walk entries in the first block to find ".."
    let mut cursor = 0usize;
    while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
        let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
        if rec_len < DIR_ENTRY_HEADER_SIZE || cursor + rec_len > bs {
            return Err(Ext2Error::DirectoryFormat);
        }
        let entry_inode = u32::from_le_bytes([
            data[cursor],
            data[cursor + 1],
            data[cursor + 2],
            data[cursor + 3],
        ]);
        let name_len = data[cursor + 6] as usize;
        if entry_inode != 0 && name_len == 2 {
            let n = &data[cursor + DIR_ENTRY_HEADER_SIZE..cursor + DIR_ENTRY_HEADER_SIZE + 2];
            if n == b".." {
                data[cursor..cursor + 4].copy_from_slice(&new_parent.raw().to_le_bytes());
                return Ok(());
            }
        }
        cursor += rec_len;
    }
    Err(Ext2Error::DirectoryFormat)
}
