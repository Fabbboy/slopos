//! VMA Interval Tree - O(log n) virtual memory area management
//!
//! This module implements an augmented red-black tree for efficient VMA operations.
//! Each node stores an interval [start, end) and maintains `max_end` for efficient
//! overlap queries.
//!
//! Key operations:
//! - Insert: O(log n)
//! - Delete: O(log n)
//! - Find overlapping: O(log n + k) where k is number of overlaps
//! - Find covering: O(log n)

use core::cmp::{self, Ordering};
use core::ptr;

use crate::kernel_heap::{kfree, kmalloc};

pub const VMA_FLAG_COW: u32 = 1 << 16;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Color {
    Red = 0,
    Black = 1,
}

/// A node in the VMA interval tree
#[repr(C)]
pub struct VmaNode {
    /// Start address of this VMA (inclusive)
    pub start: u64,
    /// End address of this VMA (exclusive)
    pub end: u64,
    /// VMA flags (permissions, etc.)
    pub flags: u32,
    /// Reference count for shared mappings
    pub ref_count: u32,
    /// Maximum end address in this subtree (for interval queries)
    max_end: u64,
    /// Node color for RB-tree balancing
    color: Color,
    /// Parent node
    parent: *mut VmaNode,
    /// Left child
    left: *mut VmaNode,
    /// Right child
    right: *mut VmaNode,
}

unsafe impl Send for VmaNode {}
unsafe impl Sync for VmaNode {}

impl VmaNode {
    /// Allocate a new VMA node
    fn new(start: u64, end: u64, flags: u32) -> *mut Self {
        let ptr = kmalloc(core::mem::size_of::<VmaNode>()) as *mut VmaNode;
        if ptr.is_null() {
            return ptr::null_mut();
        }
        unsafe {
            (*ptr).start = start;
            (*ptr).end = end;
            (*ptr).flags = flags;
            (*ptr).ref_count = 1;
            (*ptr).max_end = end;
            (*ptr).color = Color::Red;
            (*ptr).parent = ptr::null_mut();
            (*ptr).left = ptr::null_mut();
            (*ptr).right = ptr::null_mut();
        }
        ptr
    }

    /// Check if this interval overlaps with [start, end)
    #[inline]
    fn overlaps(&self, start: u64, end: u64) -> bool {
        self.start < end && start < self.end
    }

    /// Check if this interval fully covers [start, end)
    #[inline]
    fn covers(&self, start: u64, end: u64) -> bool {
        self.start <= start && self.end >= end
    }
}

#[derive(Clone, Copy)]
pub struct VmaTree {
    root: *mut VmaNode,
    count: usize,
}

unsafe impl Send for VmaTree {}
unsafe impl Sync for VmaTree {}

impl VmaTree {
    /// Create a new empty VMA tree
    pub const fn new() -> Self {
        Self {
            root: ptr::null_mut(),
            count: 0,
        }
    }

    /// Returns the number of VMAs in the tree
    #[inline]
    pub fn len(&self) -> usize {
        self.count
    }

    /// Returns true if the tree is empty
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.root.is_null()
    }

    /// Insert a new VMA into the tree
    /// Returns the inserted node, or null if allocation failed
    pub fn insert(&mut self, start: u64, end: u64, flags: u32) -> *mut VmaNode {
        let node = VmaNode::new(start, end, flags);
        if node.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            self.insert_node(node);
        }
        self.count += 1;
        node
    }

    /// Insert a pre-allocated node into the tree
    unsafe fn insert_node(&mut self, node: *mut VmaNode) {
        let mut parent: *mut VmaNode = ptr::null_mut();
        let mut current = self.root;

        while !current.is_null() {
            parent = current;
            if (*node).end > (*current).max_end {
                (*current).max_end = (*node).end;
            }
            if (*node).start < (*current).start {
                current = (*current).left;
            } else {
                current = (*current).right;
            }
        }

        (*node).parent = parent;

        if parent.is_null() {
            self.root = node;
        } else if (*node).start < (*parent).start {
            (*parent).left = node;
        } else {
            (*parent).right = node;
        }

        self.insert_fixup(node);
    }

    /// Fix red-black tree properties after insertion
    unsafe fn insert_fixup(&mut self, mut node: *mut VmaNode) {
        while !(*node).parent.is_null() && (*(*node).parent).color == Color::Red {
            let parent = (*node).parent;
            let grandparent = (*parent).parent;

            if grandparent.is_null() {
                break;
            }

            if parent == (*grandparent).left {
                let uncle = (*grandparent).right;

                if !uncle.is_null() && (*uncle).color == Color::Red {
                    // Case 1: Uncle is red
                    (*parent).color = Color::Black;
                    (*uncle).color = Color::Black;
                    (*grandparent).color = Color::Red;
                    node = grandparent;
                } else {
                    if node == (*parent).right {
                        // Case 2: Node is right child
                        node = parent;
                        self.rotate_left(node);
                    }
                    // Case 3: Node is left child
                    let parent = (*node).parent;
                    let grandparent = (*parent).parent;
                    (*parent).color = Color::Black;
                    (*grandparent).color = Color::Red;
                    self.rotate_right(grandparent);
                }
            } else {
                // Mirror cases for right subtree
                let uncle = (*grandparent).left;

                if !uncle.is_null() && (*uncle).color == Color::Red {
                    (*parent).color = Color::Black;
                    (*uncle).color = Color::Black;
                    (*grandparent).color = Color::Red;
                    node = grandparent;
                } else {
                    if node == (*parent).left {
                        node = parent;
                        self.rotate_right(node);
                    }
                    let parent = (*node).parent;
                    let grandparent = (*parent).parent;
                    (*parent).color = Color::Black;
                    (*grandparent).color = Color::Red;
                    self.rotate_left(grandparent);
                }
            }
        }

        (*self.root).color = Color::Black;
    }

    /// Left rotation
    unsafe fn rotate_left(&mut self, x: *mut VmaNode) {
        let y = (*x).right;
        if y.is_null() {
            return;
        }

        (*x).right = (*y).left;
        if !(*y).left.is_null() {
            (*(*y).left).parent = x;
        }

        (*y).parent = (*x).parent;
        if (*x).parent.is_null() {
            self.root = y;
        } else if x == (*(*x).parent).left {
            (*(*x).parent).left = y;
        } else {
            (*(*x).parent).right = y;
        }

        (*y).left = x;
        (*x).parent = y;

        self.update_max_end(x);
        self.update_max_end(y);
    }

    /// Right rotation
    unsafe fn rotate_right(&mut self, y: *mut VmaNode) {
        let x = (*y).left;
        if x.is_null() {
            return;
        }

        (*y).left = (*x).right;
        if !(*x).right.is_null() {
            (*(*x).right).parent = y;
        }

        (*x).parent = (*y).parent;
        if (*y).parent.is_null() {
            self.root = x;
        } else if y == (*(*y).parent).left {
            (*(*y).parent).left = x;
        } else {
            (*(*y).parent).right = x;
        }

        (*x).right = y;
        (*y).parent = x;

        self.update_max_end(y);
        self.update_max_end(x);
    }

    unsafe fn update_max_end(&self, node: *mut VmaNode) {
        if node.is_null() {
            return;
        }
        let mut max = (*node).end;
        if !(*node).left.is_null() {
            max = cmp::max(max, (*(*node).left).max_end);
        }
        if !(*node).right.is_null() {
            max = cmp::max(max, (*(*node).right).max_end);
        }
        (*node).max_end = max;
    }

    pub fn remove(&mut self, start: u64, end: u64) -> bool {
        let node = self.find_exact(start, end);
        if node.is_null() {
            return false;
        }
        unsafe {
            self.remove_node(node);
        }
        self.count -= 1;
        true
    }

    pub unsafe fn remove_node(&mut self, node: *mut VmaNode) {
        self.delete_node(node);
        kfree(node as *mut _);
    }

    unsafe fn delete_node(&mut self, z: *mut VmaNode) {
        let mut y = z;
        let mut y_original_color = (*y).color;
        let x: *mut VmaNode;
        let x_parent: *mut VmaNode;

        if (*z).left.is_null() {
            x = (*z).right;
            x_parent = (*z).parent;
            self.transplant(z, (*z).right);
        } else if (*z).right.is_null() {
            x = (*z).left;
            x_parent = (*z).parent;
            self.transplant(z, (*z).left);
        } else {
            y = (*z).right;
            while !(*y).left.is_null() {
                y = (*y).left;
            }
            y_original_color = (*y).color;
            x = (*y).right;

            if (*y).parent == z {
                x_parent = y;
                if !x.is_null() {
                    (*x).parent = y;
                }
            } else {
                x_parent = (*y).parent;
                self.transplant(y, (*y).right);
                (*y).right = (*z).right;
                (*(*y).right).parent = y;
            }

            self.transplant(z, y);
            (*y).left = (*z).left;
            (*(*y).left).parent = y;
            (*y).color = (*z).color;
        }

        self.update_max_end_to_root(x_parent);

        if y_original_color == Color::Black {
            self.delete_fixup(x, x_parent);
        }
    }

    unsafe fn transplant(&mut self, u: *mut VmaNode, v: *mut VmaNode) {
        if (*u).parent.is_null() {
            self.root = v;
        } else if u == (*(*u).parent).left {
            (*(*u).parent).left = v;
        } else {
            (*(*u).parent).right = v;
        }
        if !v.is_null() {
            (*v).parent = (*u).parent;
        }
    }

    unsafe fn delete_fixup(&mut self, mut x: *mut VmaNode, mut x_parent: *mut VmaNode) {
        while x != self.root && (x.is_null() || (*x).color == Color::Black) {
            if x_parent.is_null() {
                break;
            }

            if x == (*x_parent).left {
                let mut w = (*x_parent).right;

                if !w.is_null() && (*w).color == Color::Red {
                    (*w).color = Color::Black;
                    (*x_parent).color = Color::Red;
                    self.rotate_left(x_parent);
                    w = (*x_parent).right;
                }

                if w.is_null() {
                    x = x_parent;
                    x_parent = (*x).parent;
                    continue;
                }

                let left_black = (*w).left.is_null() || (*(*w).left).color == Color::Black;
                let right_black = (*w).right.is_null() || (*(*w).right).color == Color::Black;

                if left_black && right_black {
                    (*w).color = Color::Red;
                    x = x_parent;
                    x_parent = (*x).parent;
                } else {
                    if right_black {
                        if !(*w).left.is_null() {
                            (*(*w).left).color = Color::Black;
                        }
                        (*w).color = Color::Red;
                        self.rotate_right(w);
                        w = (*x_parent).right;
                    }

                    if !w.is_null() {
                        (*w).color = (*x_parent).color;
                        (*x_parent).color = Color::Black;
                        if !(*w).right.is_null() {
                            (*(*w).right).color = Color::Black;
                        }
                        self.rotate_left(x_parent);
                    }
                    x = self.root;
                    break;
                }
            } else {
                let mut w = (*x_parent).left;

                if !w.is_null() && (*w).color == Color::Red {
                    (*w).color = Color::Black;
                    (*x_parent).color = Color::Red;
                    self.rotate_right(x_parent);
                    w = (*x_parent).left;
                }

                if w.is_null() {
                    x = x_parent;
                    x_parent = (*x).parent;
                    continue;
                }

                let left_black = (*w).left.is_null() || (*(*w).left).color == Color::Black;
                let right_black = (*w).right.is_null() || (*(*w).right).color == Color::Black;

                if left_black && right_black {
                    (*w).color = Color::Red;
                    x = x_parent;
                    x_parent = (*x).parent;
                } else {
                    if left_black {
                        if !(*w).right.is_null() {
                            (*(*w).right).color = Color::Black;
                        }
                        (*w).color = Color::Red;
                        self.rotate_left(w);
                        w = (*x_parent).left;
                    }

                    if !w.is_null() {
                        (*w).color = (*x_parent).color;
                        (*x_parent).color = Color::Black;
                        if !(*w).left.is_null() {
                            (*(*w).left).color = Color::Black;
                        }
                        self.rotate_right(x_parent);
                    }
                    x = self.root;
                    break;
                }
            }
        }

        if !x.is_null() {
            (*x).color = Color::Black;
        }
    }

    unsafe fn update_max_end_to_root(&self, mut node: *mut VmaNode) {
        while !node.is_null() {
            self.update_max_end(node);
            node = (*node).parent;
        }
    }

    pub fn find_exact(&self, start: u64, end: u64) -> *mut VmaNode {
        let mut current = self.root;
        unsafe {
            while !current.is_null() {
                match (*current).start.cmp(&start) {
                    Ordering::Equal => {
                        if (*current).end == end {
                            return current;
                        }
                        let left_result = self.find_exact_in_subtree((*current).left, start, end);
                        if !left_result.is_null() {
                            return left_result;
                        }
                        current = (*current).right;
                    }
                    Ordering::Greater => current = (*current).left,
                    Ordering::Less => current = (*current).right,
                }
            }
        }
        ptr::null_mut()
    }

    unsafe fn find_exact_in_subtree(
        &self,
        node: *mut VmaNode,
        start: u64,
        end: u64,
    ) -> *mut VmaNode {
        if node.is_null() {
            return ptr::null_mut();
        }
        if (*node).start == start && (*node).end == end {
            return node;
        }
        if start < (*node).start {
            self.find_exact_in_subtree((*node).left, start, end)
        } else {
            self.find_exact_in_subtree((*node).right, start, end)
        }
    }

    pub fn find_overlapping(&self, start: u64, end: u64) -> *mut VmaNode {
        unsafe { self.find_overlapping_in_subtree(self.root, start, end) }
    }

    unsafe fn find_overlapping_in_subtree(
        &self,
        node: *mut VmaNode,
        start: u64,
        end: u64,
    ) -> *mut VmaNode {
        if node.is_null() {
            return ptr::null_mut();
        }

        if (*node).max_end <= start {
            return ptr::null_mut();
        }

        if !(*node).left.is_null() && (*(*node).left).max_end > start {
            let result = self.find_overlapping_in_subtree((*node).left, start, end);
            if !result.is_null() {
                return result;
            }
        }

        if (*node).overlaps(start, end) {
            return node;
        }

        if (*node).start < end {
            return self.find_overlapping_in_subtree((*node).right, start, end);
        }

        ptr::null_mut()
    }

    pub fn find_covering(&self, start: u64, end: u64) -> *mut VmaNode {
        unsafe { self.find_covering_in_subtree(self.root, start, end) }
    }

    unsafe fn find_covering_in_subtree(
        &self,
        node: *mut VmaNode,
        start: u64,
        end: u64,
    ) -> *mut VmaNode {
        if node.is_null() {
            return ptr::null_mut();
        }

        if (*node).max_end < end {
            return ptr::null_mut();
        }

        if !(*node).left.is_null() {
            let result = self.find_covering_in_subtree((*node).left, start, end);
            if !result.is_null() {
                return result;
            }
        }

        if (*node).covers(start, end) {
            return node;
        }

        self.find_covering_in_subtree((*node).right, start, end)
    }

    pub fn find_containing(&self, addr: u64) -> *mut VmaNode {
        self.find_covering(addr, addr + 1)
    }

    pub fn find_first_at_or_after(&self, addr: u64) -> *mut VmaNode {
        let mut result: *mut VmaNode = ptr::null_mut();
        let mut current = self.root;

        unsafe {
            while !current.is_null() {
                if (*current).start >= addr {
                    result = current;
                    current = (*current).left;
                } else {
                    current = (*current).right;
                }
            }
        }
        result
    }

    pub fn first(&self) -> *mut VmaNode {
        if self.root.is_null() {
            return ptr::null_mut();
        }
        let mut current = self.root;
        unsafe {
            while !(*current).left.is_null() {
                current = (*current).left;
            }
        }
        current
    }

    pub fn next(&self, node: *mut VmaNode) -> *mut VmaNode {
        if node.is_null() {
            return ptr::null_mut();
        }

        unsafe {
            if !(*node).right.is_null() {
                let mut current = (*node).right;
                while !(*current).left.is_null() {
                    current = (*current).left;
                }
                return current;
            }

            let mut current = node;
            let mut parent = (*current).parent;
            while !parent.is_null() && current == (*parent).right {
                current = parent;
                parent = (*current).parent;
            }
            parent
        }
    }

    pub fn clear(&mut self) {
        unsafe {
            self.clear_subtree(self.root);
        }
        self.root = ptr::null_mut();
        self.count = 0;
    }

    unsafe fn clear_subtree(&self, node: *mut VmaNode) {
        if node.is_null() {
            return;
        }
        self.clear_subtree((*node).left);
        self.clear_subtree((*node).right);
        kfree(node as *mut _);
    }

    pub unsafe fn set_end(&mut self, node: *mut VmaNode, new_end: u64) {
        if node.is_null() {
            return;
        }
        (*node).end = new_end;
        self.update_max_end_to_root(node);
    }

    pub unsafe fn set_start(&mut self, node: *mut VmaNode, new_start: u64) {
        if node.is_null() {
            return;
        }
        (*node).start = new_start;
    }
}
