//! Shared Memory Subsystem for Wayland-like Compositor
//!
//! Provides shared memory buffers that can be mapped into multiple processes.
//! Used for client-compositor buffer sharing in the graphics stack.

use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use crate::mm_constants::{PAGE_PRESENT, PAGE_SIZE_4KB, PAGE_USER, PAGE_WRITABLE};
use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use crate::paging::{map_page_4kb_in_dir, unmap_page_in_dir};
use crate::process_vm::process_vm_get_page_dir;
use slopos_lib::{align_up, klog_debug, klog_info};

/// Maximum number of shared buffers in the system
const MAX_SHARED_BUFFERS: usize = 64;

/// Maximum number of mappings per buffer
const MAX_MAPPINGS_PER_BUFFER: usize = 8;

/// Base virtual address for shared memory mappings in userland
/// This is above the heap region to avoid conflicts
const SHM_VADDR_BASE: u64 = 0x0000_7000_0000_0000;

/// Access permissions for shared memory mappings
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ShmAccess {
    /// Read-only access (for compositor reading client buffers)
    ReadOnly = 0,
    /// Read-write access (for buffer owner)
    ReadWrite = 1,
}

impl ShmAccess {
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(ShmAccess::ReadOnly),
            1 => Some(ShmAccess::ReadWrite),
            _ => None,
        }
    }
}

/// A mapping of a shared buffer into a specific process
#[derive(Clone, Copy)]
struct ShmMapping {
    /// Task/process ID that has this mapping
    task_id: u32,
    /// Virtual address in the task's address space
    virt_addr: u64,
    /// Whether this slot is in use
    active: bool,
}

impl ShmMapping {
    const fn empty() -> Self {
        Self {
            task_id: 0,
            virt_addr: 0,
            active: false,
        }
    }
}

/// A shared memory buffer
struct SharedBuffer {
    /// Physical address of the buffer (page-aligned)
    phys_addr: u64,
    /// Size in bytes
    size: usize,
    /// Number of 4KB pages allocated
    pages: u32,
    /// Task ID of the owner (who created it)
    owner_task: u32,
    /// Unique token for cross-process reference
    token: u32,
    /// Whether this slot is in use
    active: bool,
    /// Mappings into various processes
    mappings: [ShmMapping; MAX_MAPPINGS_PER_BUFFER],
    /// Number of active mappings
    mapping_count: u8,
    /// Width in pixels (for surface buffers, 0 if not a surface)
    surface_width: u32,
    /// Height in pixels (for surface buffers, 0 if not a surface)
    surface_height: u32,
}

impl SharedBuffer {
    const fn empty() -> Self {
        Self {
            phys_addr: 0,
            size: 0,
            pages: 0,
            owner_task: 0,
            token: 0,
            active: false,
            mappings: [ShmMapping::empty(); MAX_MAPPINGS_PER_BUFFER],
            mapping_count: 0,
            surface_width: 0,
            surface_height: 0,
        }
    }
}

/// Global registry of shared buffers
struct SharedBufferRegistry {
    buffers: [SharedBuffer; MAX_SHARED_BUFFERS],
    next_token: AtomicU32,
    /// Next virtual address offset for mappings (simple bump allocator)
    next_vaddr_offset: u64,
}

impl SharedBufferRegistry {
    const fn new() -> Self {
        Self {
            buffers: [const { SharedBuffer::empty() }; MAX_SHARED_BUFFERS],
            next_token: AtomicU32::new(1), // Token 0 is invalid
            next_vaddr_offset: 0,
        }
    }

    /// Find a free slot in the buffer registry
    fn find_free_slot(&mut self) -> Option<usize> {
        for (i, buf) in self.buffers.iter().enumerate() {
            if !buf.active {
                return Some(i);
            }
        }
        None
    }

    /// Find a buffer by token
    fn find_by_token(&self, token: u32) -> Option<usize> {
        if token == 0 {
            return None;
        }
        for (i, buf) in self.buffers.iter().enumerate() {
            if buf.active && buf.token == token {
                return Some(i);
            }
        }
        None
    }

    /// Allocate a virtual address range for a mapping
    fn alloc_vaddr(&mut self, size: usize) -> u64 {
        let aligned_size = align_up(size, PAGE_SIZE_4KB as usize) as u64;
        let vaddr = SHM_VADDR_BASE + self.next_vaddr_offset;
        self.next_vaddr_offset += aligned_size;
        // Add a guard page gap between allocations
        self.next_vaddr_offset += PAGE_SIZE_4KB;
        vaddr
    }
}

static REGISTRY: Mutex<SharedBufferRegistry> = Mutex::new(SharedBufferRegistry::new());

/// Create a new shared memory buffer.
///
/// # Arguments
/// * `owner_task` - Task ID of the creator (owner)
/// * `size` - Size in bytes (will be rounded up to page boundary)
/// * `flags` - Allocation flags (currently only ALLOC_FLAG_ZERO supported)
///
/// # Returns
/// Buffer token on success, 0 on failure
pub fn shm_create(owner_task: u32, size: u64, flags: u32) -> u32 {
    if size == 0 || size > 64 * 1024 * 1024 {
        // Limit to 64MB per buffer
        klog_info!("shm_create: invalid size {}", size);
        return 0;
    }

    let aligned_size = align_up(size as usize, PAGE_SIZE_4KB as usize);
    let pages = (aligned_size / PAGE_SIZE_4KB as usize) as u32;

    // Allocate physical pages
    let phys_addr = alloc_page_frames(pages, flags | ALLOC_FLAG_ZERO);
    if phys_addr == 0 {
        klog_info!("shm_create: failed to allocate {} pages", pages);
        return 0;
    }

    let mut registry = REGISTRY.lock();
    let slot = match registry.find_free_slot() {
        Some(s) => s,
        None => {
            // Free the allocated pages
            for i in 0..pages {
                free_page_frame(phys_addr + (i as u64) * PAGE_SIZE_4KB);
            }
            klog_info!("shm_create: no free slots");
            return 0;
        }
    };

    let token = registry.next_token.fetch_add(1, Ordering::Relaxed);

    registry.buffers[slot] = SharedBuffer {
        phys_addr,
        size: aligned_size,
        pages,
        owner_task,
        token,
        active: true,
        mappings: [ShmMapping::empty(); MAX_MAPPINGS_PER_BUFFER],
        mapping_count: 0,
        surface_width: 0,
        surface_height: 0,
    };

    klog_debug!(
        "shm_create: created buffer token={} size={} pages={} for task={}",
        token,
        aligned_size,
        pages,
        owner_task
    );

    token
}

/// Map a shared buffer into a task's address space.
///
/// # Arguments
/// * `task_id` - Task to map into
/// * `token` - Buffer token from shm_create
/// * `access` - Access permissions (ReadOnly or ReadWrite)
///
/// # Returns
/// Virtual address on success, 0 on failure
pub fn shm_map(task_id: u32, token: u32, access: ShmAccess) -> u64 {
    let page_dir = process_vm_get_page_dir(task_id);
    if page_dir.is_null() {
        klog_info!("shm_map: invalid task_id {}", task_id);
        return 0;
    }

    let mut registry = REGISTRY.lock();
    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => {
            klog_info!("shm_map: invalid token {}", token);
            return 0;
        }
    };

    // First pass: check if already mapped and gather info
    {
        let buffer = &registry.buffers[slot];

        // Check if already mapped for this task
        for mapping in buffer.mappings.iter() {
            if mapping.active && mapping.task_id == task_id {
                klog_debug!("shm_map: already mapped for task {}", task_id);
                return mapping.virt_addr;
            }
        }

        // Find a free mapping slot
        if buffer.mappings.iter().all(|m| m.active) {
            klog_info!("shm_map: no free mapping slots for token {}", token);
            return 0;
        }
    }

    // Extract needed info before second mutable borrow
    let buffer_size = registry.buffers[slot].size;
    let owner_task = registry.buffers[slot].owner_task;
    let phys_base = registry.buffers[slot].phys_addr;
    let pages = registry.buffers[slot].pages;

    // Only owner can have RW access
    let actual_access = if task_id == owner_task {
        access
    } else {
        ShmAccess::ReadOnly
    };

    // Allocate virtual address range
    let vaddr = registry.alloc_vaddr(buffer_size);

    // Map each page into the task's address space
    let map_flags = if actual_access == ShmAccess::ReadWrite {
        PAGE_PRESENT | PAGE_USER | PAGE_WRITABLE
    } else {
        PAGE_PRESENT | PAGE_USER
    };

    for i in 0..pages {
        let page_vaddr = vaddr + (i as u64) * PAGE_SIZE_4KB;
        let page_phys = phys_base + (i as u64) * PAGE_SIZE_4KB;

        if map_page_4kb_in_dir(page_dir, page_vaddr, page_phys, map_flags) != 0 {
            // Rollback on failure
            for j in 0..i {
                let rollback_vaddr = vaddr + (j as u64) * PAGE_SIZE_4KB;
                unmap_page_in_dir(page_dir, rollback_vaddr);
            }
            klog_info!("shm_map: failed to map page {} for token {}", i, token);
            return 0;
        }
    }

    // Second pass: record the mapping
    let buffer = &mut registry.buffers[slot];
    let mapping_slot = buffer.mappings.iter().position(|m| !m.active).unwrap();
    buffer.mappings[mapping_slot] = ShmMapping {
        task_id,
        virt_addr: vaddr,
        active: true,
    };
    buffer.mapping_count += 1;

    klog_debug!(
        "shm_map: mapped token={} at vaddr={:#x} for task={} access={:?}",
        token,
        vaddr,
        task_id,
        actual_access
    );

    vaddr
}

/// Unmap a shared buffer from a task's address space.
///
/// # Arguments
/// * `task_id` - Task to unmap from
/// * `virt_addr` - Virtual address returned by shm_map
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_unmap(task_id: u32, virt_addr: u64) -> c_int {
    let page_dir = process_vm_get_page_dir(task_id);
    if page_dir.is_null() {
        return -1;
    }

    let mut registry = REGISTRY.lock();

    // Find the buffer and mapping
    for buffer in registry.buffers.iter_mut() {
        if !buffer.active {
            continue;
        }

        for mapping in buffer.mappings.iter_mut() {
            if mapping.active && mapping.task_id == task_id && mapping.virt_addr == virt_addr {
                // Unmap all pages
                for i in 0..buffer.pages {
                    let page_vaddr = virt_addr + (i as u64) * PAGE_SIZE_4KB;
                    unmap_page_in_dir(page_dir, page_vaddr);
                }

                // Clear the mapping
                *mapping = ShmMapping::empty();
                buffer.mapping_count = buffer.mapping_count.saturating_sub(1);

                klog_debug!(
                    "shm_unmap: unmapped vaddr={:#x} for task={}",
                    virt_addr,
                    task_id
                );
                return 0;
            }
        }
    }

    -1
}

/// Destroy a shared buffer and free its memory.
///
/// Only the owner task can destroy a buffer.
/// All mappings must be unmapped first.
///
/// # Arguments
/// * `task_id` - Task requesting destruction (must be owner)
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_destroy(task_id: u32, token: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];

    // Only owner can destroy
    if buffer.owner_task != task_id {
        klog_info!(
            "shm_destroy: task {} is not owner of token {}",
            task_id,
            token
        );
        return -1;
    }

    // Check for active mappings (other than owner's)
    // We'll forcibly unmap all mappings
    for mapping in buffer.mappings.iter_mut() {
        if mapping.active {
            let page_dir = process_vm_get_page_dir(mapping.task_id);
            if !page_dir.is_null() {
                for i in 0..buffer.pages {
                    let page_vaddr = mapping.virt_addr + (i as u64) * PAGE_SIZE_4KB;
                    unmap_page_in_dir(page_dir, page_vaddr);
                }
            }
            *mapping = ShmMapping::empty();
        }
    }

    // Free physical pages
    for i in 0..buffer.pages {
        free_page_frame(buffer.phys_addr + (i as u64) * PAGE_SIZE_4KB);
    }

    klog_debug!("shm_destroy: destroyed token={} for task={}", token, task_id);

    // Clear the buffer slot
    *buffer = SharedBuffer::empty();

    0
}

/// Get information about a shared buffer by token.
///
/// # Returns
/// (phys_addr, size, owner_task) or (0, 0, 0) if not found
pub fn shm_get_buffer_info(token: u32) -> (u64, usize, u32) {
    let registry = REGISTRY.lock();
    match registry.find_by_token(token) {
        Some(slot) => {
            let buf = &registry.buffers[slot];
            (buf.phys_addr, buf.size, buf.owner_task)
        }
        None => (0, 0, 0),
    }
}

/// Register a shared buffer as a surface for the compositor.
///
/// # Arguments
/// * `task_id` - Task ID of the surface owner
/// * `token` - Buffer token
/// * `width` - Surface width in pixels
/// * `height` - Surface height in pixels
///
/// # Returns
/// 0 on success, -1 on failure
pub fn surface_attach(task_id: u32, token: u32, width: u32, height: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];

    // Only owner can attach
    if buffer.owner_task != task_id {
        return -1;
    }

    // Verify size is sufficient (assume 4 bytes per pixel)
    let required_size = (width as usize) * (height as usize) * 4;
    if required_size > buffer.size {
        klog_info!(
            "surface_attach: buffer too small ({}), need {}",
            buffer.size,
            required_size
        );
        return -1;
    }

    buffer.surface_width = width;
    buffer.surface_height = height;

    klog_debug!(
        "surface_attach: token={} registered as {}x{} surface for task={}",
        token,
        width,
        height,
        task_id
    );

    0
}

/// Get surface info for a task.
///
/// # Returns
/// (token, width, height, phys_addr) or (0, 0, 0, 0) if no surface
pub fn get_surface_for_task(task_id: u32) -> (u32, u32, u32, u64) {
    let registry = REGISTRY.lock();

    for buffer in registry.buffers.iter() {
        if buffer.active
            && buffer.owner_task == task_id
            && buffer.surface_width > 0
            && buffer.surface_height > 0
        {
            return (
                buffer.token,
                buffer.surface_width,
                buffer.surface_height,
                buffer.phys_addr,
            );
        }
    }

    (0, 0, 0, 0)
}

/// Get the physical address of a shared buffer by token.
/// Used by FB_FLIP syscall.
pub fn shm_get_phys_addr(token: u32) -> u64 {
    let registry = REGISTRY.lock();
    match registry.find_by_token(token) {
        Some(slot) => registry.buffers[slot].phys_addr,
        None => 0,
    }
}

/// Get the size of a shared buffer by token.
pub fn shm_get_size(token: u32) -> usize {
    let registry = REGISTRY.lock();
    match registry.find_by_token(token) {
        Some(slot) => registry.buffers[slot].size,
        None => 0,
    }
}

/// Clean up all shared buffers owned by a task.
/// Called when a task terminates.
pub fn shm_cleanup_task(task_id: u32) {
    let mut registry = REGISTRY.lock();

    // First, remove all mappings for this task from all buffers
    for buffer in registry.buffers.iter_mut() {
        if !buffer.active {
            continue;
        }

        for mapping in buffer.mappings.iter_mut() {
            if mapping.active && mapping.task_id == task_id {
                // Note: page directory may already be torn down
                // Just clear the mapping record
                *mapping = ShmMapping::empty();
                buffer.mapping_count = buffer.mapping_count.saturating_sub(1);
            }
        }
    }

    // Then, destroy all buffers owned by this task
    for buffer in registry.buffers.iter_mut() {
        if buffer.active && buffer.owner_task == task_id {
            // Free physical pages
            for i in 0..buffer.pages {
                free_page_frame(buffer.phys_addr + (i as u64) * PAGE_SIZE_4KB);
            }

            // Clear all mappings (other tasks' mappings of this buffer)
            for mapping in buffer.mappings.iter_mut() {
                if mapping.active {
                    let page_dir = process_vm_get_page_dir(mapping.task_id);
                    if !page_dir.is_null() {
                        for j in 0..buffer.pages {
                            let page_vaddr = mapping.virt_addr + (j as u64) * PAGE_SIZE_4KB;
                            unmap_page_in_dir(page_dir, page_vaddr);
                        }
                    }
                }
            }

            klog_debug!("shm_cleanup_task: destroyed buffer token={}", buffer.token);
            *buffer = SharedBuffer::empty();
        }
    }
}
