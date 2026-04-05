use super::ondisk::Inode;
use super::types::InodeNum;

/// Handle to a cached inode with dirty tracking.
///
/// Accessing fields via `data()` is read-only. Accessing via `data_mut()` sets
/// the dirty flag. The caller must call `flush()` (or equivalent) before
/// dropping a dirty handle — debug builds panic on forgotten flushes.
pub struct InodeHandle {
    num: InodeNum,
    data: Inode,
    dirty: bool,
}

impl InodeHandle {
    pub fn new(num: InodeNum, data: Inode) -> Self {
        Self {
            num,
            data,
            dirty: false,
        }
    }

    pub fn num(&self) -> InodeNum {
        self.num
    }

    pub fn data(&self) -> &Inode {
        &self.data
    }

    pub fn data_mut(&mut self) -> &mut Inode {
        self.dirty = true;
        &mut self.data
    }

    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    pub fn mark_clean(&mut self) {
        self.dirty = false;
    }

    pub fn into_inner(mut self) -> (InodeNum, Inode, bool) {
        let dirty = self.dirty;
        self.dirty = false; // prevent drop panic
        (self.num, self.data, dirty)
    }
}

impl Drop for InodeHandle {
    fn drop(&mut self) {
        debug_assert!(
            !self.dirty,
            "InodeHandle for inode {} dropped while dirty — call flush first",
            self.num.raw()
        );
    }
}
