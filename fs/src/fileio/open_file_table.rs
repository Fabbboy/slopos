use super::*;

pub(super) fn alloc_open_file_entry(
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    ops: &'static dyn FileOps,
    handle: usize,
    status_flags: OpenMode,
    position: u64,
) -> Option<u16> {
    for (idx, slot) in open_files.iter_mut().enumerate() {
        if !slot.valid {
            let generation = slot.generation.wrapping_add(1);
            *slot = OpenFileEntry {
                ops: Some(ops),
                handle,
                position,
                status_flags,
                refcount: 1,
                generation,
                valid: true,
            };
            return Some(idx as u16);
        }
    }
    None
}

pub(super) fn get_open_file_mut<'a>(
    open_files: &'a mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    open_file_idx: u16,
) -> Option<&'a mut OpenFileEntry> {
    let idx = open_file_idx as usize;
    let slot = open_files.get_mut(idx)?;
    if !slot.valid {
        return None;
    }
    Some(slot)
}

pub(super) fn incref_open_file(
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    open_file_idx: u16,
) -> bool {
    let Some(slot) = get_open_file_mut(open_files, open_file_idx) else {
        return false;
    };
    slot.refcount = slot.refcount.saturating_add(1);
    true
}

pub(super) fn release_open_file(
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    open_file_idx: u16,
) {
    let idx = open_file_idx as usize;
    let Some(slot) = open_files.get_mut(idx) else {
        return;
    };
    if !slot.valid {
        return;
    }

    if slot.refcount > 1 {
        slot.refcount -= 1;
        return;
    }

    if let Some(ops) = slot.ops {
        ops.release(slot.handle);
    }

    slot.ops = None;
    slot.handle = 0;
    slot.position = 0;
    slot.status_flags = OpenMode::EMPTY;
    slot.refcount = 0;
    slot.valid = false;
}

#[allow(dead_code)]
pub(super) fn open_file_kind(
    open_files: &mut [OpenFileEntry; FILEIO_MAX_OPEN_FILE_ENTRIES],
    open_file_idx: u16,
) -> Option<FileKind> {
    let slot = get_open_file_mut(open_files, open_file_idx)?;
    Some(slot.ops?.kind())
}
