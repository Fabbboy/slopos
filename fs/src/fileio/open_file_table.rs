use super::*;

use slopos_ostd::handle::{Handle, HandleTable};

/// Bits reserved for the slot index when packing an [`OpenFile`] handle
/// into the `u64` poll token. The open-file table is capped at
/// [`FILEIO_MAX_OPEN_FILE_ENTRIES`] (≤ 65536), so 16 bits cover every
/// slot; the remaining 48 bits hold the generation.
const OF_SLOT_BITS: u32 = 16;
const OF_SLOT_MASK: u64 = (1 << OF_SLOT_BITS) - 1;

/// Pack an [`OpenFile`] handle into a `u64` poll token for the
/// kernel-internal poll ABI (which cannot carry the 16-byte handle type).
pub(super) fn pack_open_file_token(h: Handle<OpenFile>) -> u64 {
    let generation = h.generation() & (u64::MAX >> OF_SLOT_BITS);
    (generation << OF_SLOT_BITS) | (h.slot() as u64 & OF_SLOT_MASK)
}

/// Rebuild an [`OpenFile`] handle from a packed poll token.
pub(super) fn unpack_open_file_token(token: u64) -> Handle<OpenFile> {
    Handle::from_parts((token & OF_SLOT_MASK) as u32, token >> OF_SLOT_BITS)
}

pub(super) fn alloc_open_file_entry(
    open_files: &mut HandleTable<OpenFile>,
    ops: &'static dyn FileOps,
    handle: usize,
    status_flags: OpenMode,
    position: u64,
) -> Option<Handle<OpenFile>> {
    if open_files.len() >= FILEIO_MAX_OPEN_FILE_ENTRIES {
        return None;
    }
    open_files
        .insert(OpenFile {
            ops: Some(ops),
            handle,
            position,
            status_flags,
            refcount: 1,
        })
        .ok()
}

pub(super) fn get_open_file_mut(
    open_files: &mut HandleTable<OpenFile>,
    open_file: Handle<OpenFile>,
) -> Option<&mut OpenFile> {
    open_files.get_mut(open_file).ok()
}

pub(super) fn incref_open_file(
    open_files: &mut HandleTable<OpenFile>,
    open_file: Handle<OpenFile>,
) -> bool {
    let Ok(slot) = open_files.get_mut(open_file) else {
        return false;
    };
    slot.refcount = slot.refcount.saturating_add(1);
    true
}

pub(super) fn release_open_file(
    open_files: &mut HandleTable<OpenFile>,
    open_file: Handle<OpenFile>,
) {
    let Ok(slot) = open_files.get_mut(open_file) else {
        return;
    };
    if slot.refcount > 1 {
        slot.refcount -= 1;
        return;
    }
    // Last reference: remove the entry from the table (bumping the slot
    // generation so any surviving handle goes stale) and then run the
    // backend release with the owned value.
    if let Ok(entry) = open_files.remove(open_file)
        && let Some(ops) = entry.ops
    {
        ops.release(entry.handle);
    }
}

#[allow(dead_code)]
pub(super) fn open_file_kind(
    open_files: &mut HandleTable<OpenFile>,
    open_file: Handle<OpenFile>,
) -> Option<FileKind> {
    let slot = open_files.get_mut(open_file).ok()?;
    Some(slot.ops?.kind())
}
