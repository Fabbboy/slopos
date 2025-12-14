use core::ffi::{c_char, c_int, c_void};
use core::{mem, ptr, slice};

use spin::Mutex;
use slopos_drivers::serial_println;
use slopos_drivers::wl_currency;

pub const RAMFS_TYPE_FILE: c_int = 1;
pub const RAMFS_TYPE_DIRECTORY: c_int = 2;

#[repr(C)]
pub struct ramfs_node_t {
    pub name: *mut c_char,
    pub type_: c_int,
    pub size: usize,
    pub data: *mut c_void,
    pub refcount: u32,
    pub pending_unlink: u8,
    pub parent: *mut ramfs_node_t,
    pub children: *mut ramfs_node_t,
    pub next_sibling: *mut ramfs_node_t,
    pub prev_sibling: *mut ramfs_node_t,
}

// ramfs_node_t implementation - methods are defined elsewhere as needed

#[derive(Copy, Clone, PartialEq, Eq)]
enum RamfsCreateMode {
    None,
    Directories,
}

struct RamfsState {
    root: *mut ramfs_node_t,
    initialized: bool,
}

impl RamfsState {
    const fn new() -> Self {
        Self {
            root: ptr::null_mut(),
            initialized: false,
        }
    }
}

static RAMFS_STATE: Mutex<RamfsState> = Mutex::new(RamfsState::new());

unsafe impl Send for RamfsState {}

use slopos_mm::kernel_heap::{kmalloc, kfree};

const MAX_PATH: usize = 512;

unsafe fn cstr_len(ptr_in: *const c_char) -> usize {
    if ptr_in.is_null() {
        return 0;
    }
    let mut len = 0usize;
    unsafe {
        while *ptr_in.add(len) != 0 {
            len += 1;
        }
    }
    len
}

unsafe fn path_bytes<'a>(path: *const c_char) -> Option<&'a [u8]> {
    unsafe {
        if path.is_null() {
            return None;
        }
        let len = cstr_len(path);
        Some(slice::from_raw_parts(path as *const u8, len.min(MAX_PATH)))
    }
}

unsafe fn dup_bytes(bytes: &[u8]) -> *mut c_char {
    unsafe {
        let len = bytes.len();
        let alloc = kmalloc(len + 1);
        if alloc.is_null() {
            return ptr::null_mut();
        }
        let dst = alloc as *mut u8;
        ptr::copy_nonoverlapping(bytes.as_ptr(), dst, len);
        *dst.add(len) = 0;
        dst as *mut c_char
    }
}

unsafe fn alloc_zeroed(size: usize) -> *mut c_void {
    unsafe {
        let ptr = kmalloc(size);
        if ptr.is_null() {
            return ptr::null_mut();
        }
        ptr::write_bytes(ptr, 0, size);
        ptr
    }
}

unsafe fn alloc_node(name: &[u8], type_: c_int, parent: *mut ramfs_node_t) -> *mut ramfs_node_t {
    unsafe {
        let node_ptr = alloc_zeroed(mem::size_of::<ramfs_node_t>()) as *mut ramfs_node_t;
        if node_ptr.is_null() {
            return ptr::null_mut();
        }

        let name_ptr = dup_bytes(name);
        if name_ptr.is_null() {
            kfree(node_ptr as *mut c_void);
            return ptr::null_mut();
        }

        (*node_ptr) = ramfs_node_t {
            name: name_ptr,
            type_,
            size: 0,
            data: ptr::null_mut(),
            refcount: 1,
            pending_unlink: 0,
            parent,
            children: ptr::null_mut(),
            next_sibling: ptr::null_mut(),
            prev_sibling: ptr::null_mut(),
        };

        node_ptr
    }
}

fn validate_path(path: *const c_char) -> bool {
    unsafe { !path.is_null() && *path == b'/' as c_char }
}

unsafe fn node_name_bytes<'a>(node: *const ramfs_node_t) -> &'a [u8] {
    unsafe {
        if node.is_null() || (*node).name.is_null() {
            return &[];
        }
        let len = cstr_len((*node).name);
        slice::from_raw_parts((*node).name as *const u8, len)
    }
}

unsafe fn ramfs_link_child(parent: *mut ramfs_node_t, child: *mut ramfs_node_t) {
    unsafe {
        if parent.is_null() || child.is_null() {
            return;
        }
        (*child).next_sibling = (*parent).children;
        if !(*parent).children.is_null() {
            (*(*parent).children).prev_sibling = child;
        }
        (*parent).children = child;
    }
}

unsafe fn ramfs_detach_node(node: *mut ramfs_node_t) {
    unsafe {
        if node.is_null() || (*node).parent.is_null() {
            return;
        }
        let parent = (*node).parent;
        if (*parent).children == node {
            (*parent).children = (*node).next_sibling;
        }
        if !(*node).prev_sibling.is_null() {
            (*(*node).prev_sibling).next_sibling = (*node).next_sibling;
        }
        if !(*node).next_sibling.is_null() {
            (*(*node).next_sibling).prev_sibling = (*node).prev_sibling;
        }
        (*node).parent = ptr::null_mut();
        (*node).prev_sibling = ptr::null_mut();
        (*node).next_sibling = ptr::null_mut();
    }
}

unsafe fn ramfs_find_child_component(
    parent: *mut ramfs_node_t,
    name: &[u8],
) -> *mut ramfs_node_t {
    unsafe {
        if parent.is_null() || (*parent).type_ != RAMFS_TYPE_DIRECTORY {
            return ptr::null_mut();
        }
        let mut child = (*parent).children;
        while !child.is_null() {
            let child_name = node_name_bytes(child);
            if child_name == name {
                return child;
            }
            child = (*child).next_sibling;
        }
        ptr::null_mut()
    }
}

fn component_is_dot(comp: &[u8]) -> bool {
    comp == [b'.']
}

fn component_is_dotdot(comp: &[u8]) -> bool {
    comp == [b'.', b'.']
}

unsafe fn ramfs_traverse_internal<'a>(
    state: &mut RamfsState,
    path: &'a [u8],
    create_mode: RamfsCreateMode,
    stop_before_last: bool,
    last_component: &mut Option<&'a [u8]>,
) -> *mut ramfs_node_t {
    unsafe {
        if path.is_empty() || state.root.is_null() || path[0] != b'/' {
            return ptr::null_mut();
        }

        let mut current = state.root;
        let mut idx = 0usize;

        while idx < path.len() {
            while idx < path.len() && path[idx] == b'/' {
                idx += 1;
            }
            if idx >= path.len() {
                if stop_before_last {
                    *last_component = Some(&[]);
                }
                return current;
            }

            let start = idx;
            while idx < path.len() && path[idx] != b'/' {
                idx += 1;
            }
            let component = &path[start..idx];
            while idx < path.len() && path[idx] == b'/' {
                idx += 1;
            }
            let is_last = idx >= path.len();

            if stop_before_last && is_last {
                *last_component = Some(component);
                return current;
            }

            if component_is_dot(component) {
                continue;
            }
            if component_is_dotdot(component) {
                if !(*current).parent.is_null() {
                    current = (*current).parent;
                }
                continue;
            }

            let mut next = ramfs_find_child_component(current, component);
            if next.is_null() {
                if create_mode == RamfsCreateMode::Directories {
                    next = alloc_node(component, RAMFS_TYPE_DIRECTORY, current);
                    if next.is_null() {
                        return ptr::null_mut();
                    }
                    ramfs_link_child(current, next);
                } else {
                    return ptr::null_mut();
                }
            }
            current = next;
        }

        current
    }
}

unsafe fn ramfs_free_node_recursive(node: *mut ramfs_node_t) {
    unsafe {
        if node.is_null() {
            return;
        }

        let mut child = (*node).children;
        while !child.is_null() {
            let next = (*child).next_sibling;
            ramfs_free_node_recursive(child);
            child = next;
        }

        if !(*node).data.is_null() {
            kfree((*node).data);
            (*node).data = ptr::null_mut();
        }

        if !(*node).name.is_null() {
            kfree((*node).name as *mut c_void);
            (*node).name = ptr::null_mut();
        }

        kfree(node as *mut c_void);
    }
}

fn ensure_initialized_locked(state: &mut RamfsState) -> c_int {
    if state.initialized {
        return 0;
    }

    unsafe {
        let root = alloc_node(b"/", RAMFS_TYPE_DIRECTORY, ptr::null_mut());
        if root.is_null() {
            wl_currency::award_loss();
            return -1;
        }
        state.root = root;
        state.initialized = true;
    }

    // Optional sample content for quick sanity checks, like the C implementation.
    static ETC: &[u8] = b"/etc\0";
    static README: &[u8] = b"/etc/readme.txt\0";
    static TMP: &[u8] = b"/tmp\0";
    let _ = ramfs_create_directory(ETC.as_ptr() as *const c_char);
    let sample = b"SlopOS ramfs online\n";
    let _ = ramfs_create_file(
        README.as_ptr() as *const c_char,
        sample.as_ptr() as *const c_void,
        sample.len(),
    );
    let _ = ramfs_create_directory(TMP.as_ptr() as *const c_char);

        serial_println!("ramfs: initialized");
        wl_currency::award_win();
    0
}

#[unsafe(no_mangle)]
pub fn ramfs_get_root() -> *mut ramfs_node_t {
    let mut state = RAMFS_STATE.lock();
    if ensure_initialized_locked(&mut state) != 0 {
        return ptr::null_mut();
    }
    state.root
}

#[unsafe(no_mangle)]
pub fn ramfs_init() -> c_int {
    let mut state = RAMFS_STATE.lock();
    ensure_initialized_locked(&mut state)
}

#[unsafe(no_mangle)]
pub fn ramfs_node_retain(node: *mut ramfs_node_t) {
    if node.is_null() {
        return;
    }
    let _guard = RAMFS_STATE.lock();
    unsafe {
        (*node).refcount = (*node).refcount.saturating_add(1);
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_node_release(node: *mut ramfs_node_t) {
    if node.is_null() {
        return;
    }
    let mut should_free = false;
    {
        let _guard = RAMFS_STATE.lock();
        unsafe {
            if (*node).refcount > 0 {
                (*node).refcount -= 1;
            }
            if (*node).refcount == 0 {
                should_free = true;
            }
        }
    }

    if should_free {
        unsafe { ramfs_free_node_recursive(node) };
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_find_node(path: *const c_char) -> *mut ramfs_node_t {
    if !validate_path(path) {
        return ptr::null_mut();
    }
    let bytes = unsafe { path_bytes(path) };
    if bytes.is_none() {
        return ptr::null_mut();
    }
    let mut state = RAMFS_STATE.lock();
    if ensure_initialized_locked(&mut state) != 0 {
        return ptr::null_mut();
    }
    unsafe { ramfs_traverse_internal(&mut state, bytes.unwrap(), RamfsCreateMode::None, false, &mut None) }
}

#[unsafe(no_mangle)]
pub fn ramfs_acquire_node(path: *const c_char) -> *mut ramfs_node_t {
    let node = ramfs_find_node(path);
    if node.is_null() {
        return ptr::null_mut();
    }
    ramfs_node_retain(node);
    node
}

#[unsafe(no_mangle)]
pub fn ramfs_create_directory(path: *const c_char) -> *mut ramfs_node_t {
    if !validate_path(path) {
        return ptr::null_mut();
    }
    let bytes = unsafe { path_bytes(path) }.unwrap_or(&[]);
    let mut state = RAMFS_STATE.lock();
    if ensure_initialized_locked(&mut state) != 0 {
        return ptr::null_mut();
    }

    let mut last_component: Option<&[u8]> = None;
    let parent = unsafe {
        ramfs_traverse_internal(
            &mut state,
            bytes,
            RamfsCreateMode::Directories,
            true,
            &mut last_component,
        )
    };
    if parent.is_null() || last_component.is_none() {
        return ptr::null_mut();
    }

    let name = last_component.unwrap();
    if component_is_dot(name) || component_is_dotdot(name) {
        return parent;
    }

    unsafe {
        let existing = ramfs_find_child_component(parent, name);
        if !existing.is_null() {
            if (*existing).type_ == RAMFS_TYPE_DIRECTORY {
                return existing;
            }
            return ptr::null_mut();
        }

        let node = alloc_node(name, RAMFS_TYPE_DIRECTORY, parent);
        if node.is_null() {
            return ptr::null_mut();
        }
        ramfs_link_child(parent, node);
        node
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_create_file(
    path: *const c_char,
    data: *const c_void,
    size: usize,
) -> *mut ramfs_node_t {
    if !validate_path(path) {
        return ptr::null_mut();
    }
    let bytes = unsafe { path_bytes(path) }.unwrap_or(&[]);
    let mut state = RAMFS_STATE.lock();
    if ensure_initialized_locked(&mut state) != 0 {
        return ptr::null_mut();
    }

    let mut last_component: Option<&[u8]> = None;
    let parent = unsafe {
        ramfs_traverse_internal(
            &mut state,
            bytes,
            RamfsCreateMode::Directories,
            true,
            &mut last_component,
        )
    };
    if parent.is_null() || last_component.is_none() {
        return ptr::null_mut();
    }
    let name = last_component.unwrap();
    if component_is_dot(name) || component_is_dotdot(name) {
        return ptr::null_mut();
    }

    unsafe {
        let existing = ramfs_find_child_component(parent, name);
        if !existing.is_null() {
            if (*existing).type_ == RAMFS_TYPE_FILE {
                return ptr::null_mut();
            }
            return ptr::null_mut();
        }

        let node = alloc_node(name, RAMFS_TYPE_FILE, parent);
        if node.is_null() {
            return ptr::null_mut();
        }

        if size > 0 {
            let data_ptr = kmalloc(size);
            if data_ptr.is_null() {
                ramfs_free_node_recursive(node);
                return ptr::null_mut();
            }
            if !data.is_null() {
                ptr::copy_nonoverlapping(data as *const u8, data_ptr as *mut u8, size);
            } else {
                ptr::write_bytes(data_ptr, 0, size);
            }
            (*node).data = data_ptr;
            (*node).size = size;
        }

        ramfs_link_child(parent, node);
        node
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_read_file(
    path: *const c_char,
    buffer: *mut c_void,
    buffer_size: usize,
    bytes_read: *mut usize,
) -> c_int {
    if !validate_path(path) || (buffer.is_null() && buffer_size > 0) {
        return -1;
    }
    if !bytes_read.is_null() {
        unsafe { *bytes_read = 0 };
    }

    let node = ramfs_acquire_node(path);
    if node.is_null() {
        return -1;
    }

    let rc = ramfs_read_bytes(node, 0, buffer, buffer_size, bytes_read);
    ramfs_node_release(node);
    rc
}

#[unsafe(no_mangle)]
pub fn ramfs_write_file(path: *const c_char, data: *const c_void, size: usize) -> c_int {
    if !validate_path(path) {
        return -1;
    }
    if size > 0 && data.is_null() {
        return -1;
    }

    let mut node = ramfs_acquire_node(path);
    if node.is_null() {
        node = ramfs_create_file(path, data, size);
        if node.is_null() {
            return -1;
        }
        ramfs_node_retain(node);
    }

    let rc = if size == 0 {
        let guard = RAMFS_STATE.lock();
        unsafe {
            if (*node).data.is_null() {
                (*node).size = 0;
            } else {
                kfree((*node).data);
                (*node).data = ptr::null_mut();
                (*node).size = 0;
            }
        }
        drop(guard);
        0
    } else {
        ramfs_write_bytes(node, 0, data, size)
    };

    ramfs_node_release(node);
    rc
}

#[unsafe(no_mangle)]
pub fn ramfs_read_bytes(
    node: *mut ramfs_node_t,
    offset: usize,
    buffer: *mut c_void,
    buffer_len: usize,
    bytes_read: *mut usize,
) -> c_int {
    if !bytes_read.is_null() {
        unsafe { *bytes_read = 0 };
    }
    if node.is_null() || unsafe { (*node).type_ } != RAMFS_TYPE_FILE || (buffer.is_null() && buffer_len > 0) {
        return -1;
    }

    let guard = RAMFS_STATE.lock();
    unsafe {
        if offset >= (*node).size {
            return 0;
        }
        let remaining = (*node).size - offset;
        let to_read = remaining.min(buffer_len);
        if to_read > 0 && !(*node).data.is_null() && !buffer.is_null() {
            ptr::copy_nonoverlapping(
                (*node).data.add(offset),
                buffer,
                to_read,
            );
        }
        if !bytes_read.is_null() {
            *bytes_read = to_read;
        }
    }
    drop(guard);
    0
}

unsafe fn ensure_capacity_locked(node: *mut ramfs_node_t, required_size: usize) -> c_int {
    unsafe {
        if node.is_null() {
            return -1;
        }
        if required_size <= (*node).size {
            if (*node).size > 0 && (*node).data.is_null() {
                let new_data = kmalloc((*node).size);
                if new_data.is_null() {
                    return -1;
                }
                ptr::write_bytes(new_data, 0, (*node).size);
                (*node).data = new_data;
            }
            return 0;
        }

        let new_data = kmalloc(required_size);
        if new_data.is_null() {
            return -1;
        }

        if (*node).size > 0 && !(*node).data.is_null() {
            ptr::copy_nonoverlapping((*node).data, new_data, (*node).size);
        } else if (*node).size > 0 {
            ptr::write_bytes(new_data, 0, (*node).size);
        }

        if required_size > (*node).size {
            let gap = required_size - (*node).size;
            ptr::write_bytes((new_data as *mut u8).add((*node).size), 0, gap);
        }

        if !(*node).data.is_null() {
            kfree((*node).data);
        }

        (*node).data = new_data;
        (*node).size = required_size;
        0
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_write_bytes(
    node: *mut ramfs_node_t,
    offset: usize,
    data: *const c_void,
    size: usize,
) -> c_int {
    if node.is_null() || unsafe { (*node).type_ } != RAMFS_TYPE_FILE || data.is_null() {
        return -1;
    }
    if offset.checked_add(size).is_none() {
        return -1;
    }

    let guard = RAMFS_STATE.lock();
    unsafe {
        if ensure_capacity_locked(node, offset + size) != 0 {
            return -1;
        }
        if size > 0 && !(*node).data.is_null() {
            ptr::copy_nonoverlapping(data, (*node).data.add(offset), size);
        }
    }
    drop(guard);
    0
}

#[unsafe(no_mangle)]
pub fn ramfs_list_directory(
    path: *const c_char,
    entries: *mut *mut *mut ramfs_node_t,
    count: *mut c_int,
) -> c_int {
    if !validate_path(path) || entries.is_null() || count.is_null() {
        return -1;
    }
    unsafe {
        *count = 0;
        *entries = ptr::null_mut();
    }

    let dir = ramfs_acquire_node(path);
    if dir.is_null() || unsafe { (*dir).type_ } != RAMFS_TYPE_DIRECTORY {
        if !dir.is_null() {
            ramfs_node_release(dir);
        }
        return -1;
    }

    let mut child_count = 0i32;
    {
        let _guard = RAMFS_STATE.lock();
        unsafe {
            let mut child = (*dir).children;
            while !child.is_null() {
                child_count += 1;
                child = (*child).next_sibling;
            }
        }
    }

    if child_count == 0 {
        ramfs_node_release(dir);
        unsafe {
            *count = 0;
            *entries = ptr::null_mut();
        }
        return 0;
    }

    let array_bytes = (child_count as usize)
        .saturating_mul(mem::size_of::<*mut ramfs_node_t>());
    let array_ptr = kmalloc(array_bytes) as *mut *mut ramfs_node_t;
    if array_ptr.is_null() {
        ramfs_node_release(dir);
        return -1;
    }

    let mut filled = 0i32;
    {
        let _guard = RAMFS_STATE.lock();
        unsafe {
            let mut child = (*dir).children;
            while !child.is_null() && filled < child_count {
                ramfs_node_retain(child);
                *array_ptr.add(filled as usize) = child;
                filled += 1;
                child = (*child).next_sibling;
            }
        }
    }

    ramfs_node_release(dir);
    unsafe {
        *entries = array_ptr;
        *count = filled;
    }
    0
}

#[unsafe(no_mangle)]
pub fn ramfs_release_list(entries: *mut *mut ramfs_node_t, count: c_int) {
    if entries.is_null() || count <= 0 {
        return;
    }
    for idx in 0..(count as usize) {
        unsafe {
            let node_ptr = *entries.add(idx);
            if !node_ptr.is_null() {
                ramfs_node_release(node_ptr);
            }
        }
    }
}

#[unsafe(no_mangle)]
pub fn ramfs_remove_file(path: *const c_char) -> c_int {
    if !validate_path(path) {
        return -1;
    }

    let mut guard = RAMFS_STATE.lock();
    if ensure_initialized_locked(&mut guard) != 0 {
        return -1;
    }
    unsafe {
        let node = ramfs_traverse_internal(&mut guard, path_bytes(path).unwrap_or(&[]), RamfsCreateMode::None, false, &mut None);
        if node.is_null() || (*node).type_ != RAMFS_TYPE_FILE || (*node).parent.is_null() {
            return -1;
        }
        if (*node).refcount > 1 {
            return -1;
        }
        ramfs_detach_node(node);
        (*node).children = ptr::null_mut();
        (*node).refcount = 0;
        drop(guard);
        ramfs_free_node_recursive(node);
    }
    0
}

#[unsafe(no_mangle)]
pub fn ramfs_get_size(node: *mut ramfs_node_t) -> usize {
    if node.is_null() {
        return 0;
    }
    let _guard = RAMFS_STATE.lock();
    unsafe { (*node).size }
}

