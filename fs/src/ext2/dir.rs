use super::Ext2Error;
use super::blockmap;
use super::cache::{BlockCache, BlockOwner};
use super::geometry::Ext2Geometry;
use super::ondisk::{
    DIR_ENTRY_HEADER_SIZE, DirEntry, Inode, Superblock, dir_entry_size, write_dir_entry,
};
use super::types::{FileBlock, InodeNum};
use crate::blockdev::BlockDevice;
use core::cmp;

/// One record's header fields, accepted only if every consumer of the record
/// can act on it.
///
/// The walker and the inserter disagreed about what "valid" meant: the walker
/// admitted a record whose `rec_len` was merely `>= 8` and long enough for the
/// name, while the inserter needed room for the *padded* entry size and
/// subtracted the two. A record with `name_len=1, rec_len=9` satisfied the
/// first and underflowed the second. One predicate, used by both.
struct DirRecord {
    inode: u32,
    rec_len: usize,
    name_len: usize,
    file_type: u8,
    /// Bytes this record actually needs; never greater than `rec_len`.
    actual_size: usize,
}

fn parse_record(data: &[u8], cursor: usize, block_size: usize) -> Result<DirRecord, Ext2Error> {
    if cursor + DIR_ENTRY_HEADER_SIZE > data.len() || cursor + DIR_ENTRY_HEADER_SIZE > block_size {
        return Err(Ext2Error::DirectoryFormat);
    }
    let inode = u32::from_le_bytes([
        data[cursor],
        data[cursor + 1],
        data[cursor + 2],
        data[cursor + 3],
    ]);
    let rec_len = u16::from_le_bytes([data[cursor + 4], data[cursor + 5]]) as usize;
    let name_len = data[cursor + 6] as usize;
    let file_type = data[cursor + 7];

    if rec_len < DIR_ENTRY_HEADER_SIZE || rec_len % 4 != 0 {
        return Err(Ext2Error::DirectoryFormat);
    }
    let end = cursor
        .checked_add(rec_len)
        .ok_or(Ext2Error::DirectoryFormat)?;
    if end > block_size || end > data.len() {
        return Err(Ext2Error::DirectoryFormat);
    }

    // A free record's name_len byte is stale and unconstrained, so only a live
    // record's name is held to fitting.
    let actual_size = if inode != 0 {
        let size = dir_entry_size(name_len);
        if size > rec_len {
            return Err(Ext2Error::DirectoryFormat);
        }
        size
    } else {
        DIR_ENTRY_HEADER_SIZE
    };

    Ok(DirRecord {
        inode,
        rec_len,
        name_len,
        file_type,
        actual_size,
    })
}

/// Iterate directory entries, calling `f` for each valid entry.
/// Returns early if `f` returns `false`.
pub fn for_each_entry(
    inode: &Inode,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
    f: &mut dyn FnMut(DirEntry<'_>) -> bool,
) -> Result<(), Ext2Error> {
    if !inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let mut offset = 0u32;
    while offset < inode.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(inode, file_block, ptrs_per_block, cache, device, owner)?;
        if !phys.is_valid() {
            break;
        }
        let block = cache.get_owned(phys, device, owner)?;
        let data = block.data();
        let mut cursor = 0usize;
        while cursor + DIR_ENTRY_HEADER_SIZE <= block_size as usize {
            let record = parse_record(data, cursor, block_size as usize)?;
            if record.inode != 0 {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end = name_start + record.name_len;
                let entry = DirEntry {
                    inode: InodeNum(record.inode),
                    file_type: record.file_type,
                    name: &data[name_start..name_end],
                };
                if !f(entry) {
                    return Ok(());
                }
            }
            cursor += record.rec_len;
        }
        offset += block_size;
    }
    Ok(())
}

pub fn lookup_child(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
) -> Result<InodeNum, Ext2Error> {
    let mut found = None;
    for_each_entry(
        parent,
        cache,
        device,
        ptrs_per_block,
        block_size,
        owner,
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

/// Remove a directory entry by name. Per ext2: extend the predecessor's
/// `rec_len` to absorb the deleted entry; for the first entry in a block, zero
/// the inode field instead.
pub fn remove_dir_entry(
    parent: &Inode,
    name: &[u8],
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    if !parent.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let bs = block_size as usize;
    let mut offset = 0u32;
    while offset < parent.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(parent, file_block, ptrs_per_block, cache, device, owner)?;
        if !phys.is_valid() {
            break;
        }
        let mut block = cache.get_owned(phys, device, owner)?;
        let data = block.data_mut();

        let mut cursor = 0usize;
        let mut prev_cursor: Option<usize> = None;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let record = parse_record(data, cursor, bs)?;
            let rec_len = record.rec_len;

            if record.inode != 0 {
                let name_start = cursor + DIR_ENTRY_HEADER_SIZE;
                let name_end =
                    name_start + cmp::min(record.name_len, rec_len - DIR_ENTRY_HEADER_SIZE);
                if &data[name_start..name_end] == name {
                    match prev_cursor {
                        Some(prev) => {
                            let prev_rec =
                                u16::from_le_bytes([data[prev + 4], data[prev + 5]]) as usize;
                            let merged = prev_rec + rec_len;
                            // A merged record must still fit the block; a
                            // u16 truncation here would corrupt the chain.
                            let new_rec =
                                u16::try_from(merged).map_err(|_| Ext2Error::DirectoryFormat)?;
                            data[prev + 4..prev + 6].copy_from_slice(&new_rec.to_le_bytes());
                        }
                        None => {
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
    owner: BlockOwner,
) -> Result<bool, Ext2Error> {
    let mut count = 0u32;
    for_each_entry(
        inode,
        cache,
        device,
        ptrs_per_block,
        block_size,
        owner,
        &mut |entry| {
            if entry.name == b"." || entry.name == b".." {
                count += 1;
                true
            } else {
                count += 1;
                false
            }
        },
    )?;
    Ok(count <= 2)
}

pub fn append_dir_entry(
    parent_inode: &mut Inode,
    child: InodeNum,
    name: &[u8],
    file_type: u8,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    superblock: &mut Superblock,
    geom: &Ext2Geometry,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    let needed = dir_entry_size(name.len());
    let bs = block_size as usize;

    let mut offset = 0u32;
    while offset < parent_inode.size {
        let file_block = FileBlock(offset / block_size);
        let phys = blockmap::map_block(
            parent_inode,
            file_block,
            ptrs_per_block,
            cache,
            device,
            owner,
        )?;
        if !phys.is_valid() {
            break;
        }
        let mut block = cache.get_owned(phys, device, owner)?;
        let data = block.data_mut();
        let mut cursor = 0usize;

        while cursor + DIR_ENTRY_HEADER_SIZE <= bs {
            let record = parse_record(data, cursor, bs)?;
            let rec_len = record.rec_len;
            let entry_inode = record.inode;
            let actual_size = record.actual_size;
            let slack = rec_len - actual_size;

            if slack >= needed {
                if entry_inode != 0 {
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

    let file_block = FileBlock(parent_inode.size / block_size);
    blockmap::ensure_data_block(
        parent_inode,
        file_block,
        ptrs_per_block,
        cache,
        device,
        superblock,
        geom,
        owner,
    )?;
    let new_block = blockmap::map_block(
        parent_inode,
        file_block,
        ptrs_per_block,
        cache,
        device,
        owner,
    )?;

    let mut block = cache.get_zero_owned(new_block, device, owner)?;
    let data = block.data_mut();
    write_dir_entry(&mut data[..bs], child, name, file_type, bs);

    parent_inode.size += block_size;
    parent_inode.blocks += block_size / 512;

    Ok(())
}

pub fn update_dotdot(
    dir_inode: &Inode,
    new_parent: InodeNum,
    cache: &mut BlockCache,
    device: &dyn BlockDevice,
    ptrs_per_block: u32,
    block_size: u32,
    owner: BlockOwner,
) -> Result<(), Ext2Error> {
    if !dir_inode.is_directory() {
        return Err(Ext2Error::NotDirectory);
    }
    let phys = blockmap::map_block(
        dir_inode,
        FileBlock(0),
        ptrs_per_block,
        cache,
        device,
        owner,
    )?;
    if !phys.is_valid() {
        return Err(Ext2Error::DirectoryFormat);
    }
    let mut block = cache.get_owned(phys, device, owner)?;
    let data = block.data_mut();
    let bs = block_size as usize;

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
