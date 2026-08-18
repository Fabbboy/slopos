use super::ondisk::Inode;
use super::types::InodeNum;

/// Handle to a cached inode with dirty tracking.
///
/// A dirty handle must be flushed before it is dropped; debug builds panic otherwise.
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
