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

// =============================================================================
// Pixel Format Types (Wayland wl_shm compatible)
// =============================================================================

/// Pixel format for shared memory buffers (matches Wayland wl_shm formats).
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    /// 32-bit ARGB (alpha in high byte, red in bits 16-23)
    Argb8888 = 0,
    /// 32-bit XRGB (alpha ignored, red in bits 16-23)
    Xrgb8888 = 1,
    /// 24-bit RGB (no alpha)
    Rgb888 = 2,
    /// 24-bit BGR (no alpha)
    Bgr888 = 3,
    /// 32-bit RGBA (red in high byte, alpha in bits 0-7)
    Rgba8888 = 4,
    /// 32-bit BGRA (blue in high byte, alpha in bits 0-7)
    Bgra8888 = 5,
}

impl PixelFormat {
    /// Convert from u32 representation.
    pub fn from_u32(val: u32) -> Option<Self> {
        match val {
            0 => Some(Self::Argb8888),
            1 => Some(Self::Xrgb8888),
            2 => Some(Self::Rgb888),
            3 => Some(Self::Bgr888),
            4 => Some(Self::Rgba8888),
            5 => Some(Self::Bgra8888),
            _ => None,
        }
    }

    /// Get bytes per pixel for this format.
    pub fn bytes_per_pixel(&self) -> u8 {
        match self {
            Self::Argb8888 | Self::Xrgb8888 | Self::Rgba8888 | Self::Bgra8888 => 4,
            Self::Rgb888 | Self::Bgr888 => 3,
        }
    }

    /// Check if format has an alpha channel.
    pub fn has_alpha(&self) -> bool {
        matches!(self, Self::Argb8888 | Self::Rgba8888 | Self::Bgra8888)
    }
}

/// Bitmap of supported pixel formats (for format negotiation syscall).
/// Bit N is set if PixelFormat with value N is supported.
pub const SUPPORTED_FORMATS_BITMAP: u32 =
    (1 << PixelFormat::Argb8888 as u32)
    | (1 << PixelFormat::Xrgb8888 as u32)
    | (1 << PixelFormat::Rgba8888 as u32)
    | (1 << PixelFormat::Bgra8888 as u32);

/// Default pixel format (matches typical framebuffer setup)
pub const DEFAULT_PIXEL_FORMAT: PixelFormat = PixelFormat::Argb8888;

/// Maximum number of shared buffers in the system
const MAX_SHARED_BUFFERS: usize = 64;

/// Maximum number of mappings per buffer
const MAX_MAPPINGS_PER_BUFFER: usize = 8;

/// Maximum number of entries in the virtual address free list
const MAX_VADDR_FREE_LIST: usize = 64;

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

/// Entry in the virtual address free list for address reclamation
#[derive(Clone, Copy)]
struct FreeListEntry {
    /// Starting virtual address of the free region
    vaddr: u64,
    /// Size of the free region in bytes (page-aligned)
    size: usize,
    /// Whether this entry is in use
    active: bool,
}

impl FreeListEntry {
    const fn empty() -> Self {
        Self {
            vaddr: 0,
            size: 0,
            active: false,
        }
    }
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

/// A shared memory buffer with Wayland-style reference counting
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
    /// Reference count for Wayland-style buffer lifecycle
    /// Starts at 1 (owner), incremented by compositor acquire, decremented by release
    ref_count: u32,
    /// True when compositor has released the buffer (client can reuse)
    released: bool,
    /// Pixel format of this buffer (for compositor rendering)
    format: PixelFormat,
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
            ref_count: 0,
            released: false,
            format: PixelFormat::Argb8888, // Default format
        }
    }
}

/// Global registry of shared buffers
struct SharedBufferRegistry {
    buffers: [SharedBuffer; MAX_SHARED_BUFFERS],
    next_token: AtomicU32,
    /// Next virtual address offset for mappings (bump allocator fallback)
    next_vaddr_offset: u64,
    /// Free list for virtual address reclamation
    free_list: [FreeListEntry; MAX_VADDR_FREE_LIST],
}

impl SharedBufferRegistry {
    const fn new() -> Self {
        Self {
            buffers: [const { SharedBuffer::empty() }; MAX_SHARED_BUFFERS],
            next_token: AtomicU32::new(1), // Token 0 is invalid
            next_vaddr_offset: 0,
            free_list: [const { FreeListEntry::empty() }; MAX_VADDR_FREE_LIST],
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

    /// Allocate a virtual address range for a mapping.
    /// Uses first-fit from free list, then falls back to bump allocation.
    fn alloc_vaddr(&mut self, size: usize) -> u64 {
        let aligned_size = align_up(size, PAGE_SIZE_4KB as usize);

        // First-fit search in free list
        for entry in self.free_list.iter_mut() {
            if entry.active && entry.size >= aligned_size {
                let vaddr = entry.vaddr;
                // Remove entry from free list (exact fit or discard remainder)
                // For simplicity, we don't split entries - just use exact match or larger
                entry.active = false;
                klog_debug!(
                    "alloc_vaddr: reused free entry vaddr={:#x} size={}",
                    vaddr,
                    aligned_size
                );
                return vaddr;
            }
        }

        // Fall back to bump allocation
        let vaddr = SHM_VADDR_BASE + self.next_vaddr_offset;
        self.next_vaddr_offset += aligned_size as u64;
        // Add a guard page gap between allocations
        self.next_vaddr_offset += PAGE_SIZE_4KB;
        vaddr
    }

    /// Return a virtual address range to the free list for reuse.
    fn free_vaddr(&mut self, vaddr: u64, size: usize) {
        let aligned_size = align_up(size, PAGE_SIZE_4KB as usize);

        // Find a free slot in the free list
        for entry in self.free_list.iter_mut() {
            if !entry.active {
                entry.vaddr = vaddr;
                entry.size = aligned_size;
                entry.active = true;
                klog_debug!(
                    "free_vaddr: added to free list vaddr={:#x} size={}",
                    vaddr,
                    aligned_size
                );
                return;
            }
        }

        // Free list is full, can't reclaim this address
        // This is not an error - we just lose this vaddr range
        klog_debug!(
            "free_vaddr: free list full, discarding vaddr={:#x} size={}",
            vaddr,
            aligned_size
        );
    }
}

static REGISTRY: Mutex<SharedBufferRegistry> = Mutex::new(SharedBufferRegistry::new());

/// Create a new shared memory buffer.
///
/// # Arguments
/// * `owner_process` - Process ID of the creator (owner, from task.process_id)
/// * `size` - Size in bytes (will be rounded up to page boundary)
/// * `flags` - Allocation flags (currently only ALLOC_FLAG_ZERO supported)
///
/// # Returns
/// Buffer token on success, 0 on failure
pub fn shm_create(owner_process: u32, size: u64, flags: u32) -> u32 {
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
        owner_task: owner_process,
        token,
        active: true,
        mappings: [ShmMapping::empty(); MAX_MAPPINGS_PER_BUFFER],
        mapping_count: 0,
        surface_width: 0,
        surface_height: 0,
        ref_count: 1, // Owner holds initial reference
        released: false,
        format: DEFAULT_PIXEL_FORMAT,
    };

    klog_debug!(
        "shm_create: created buffer token={} size={} pages={} for process={}",
        token,
        aligned_size,
        pages,
        owner_process
    );

    token
}

/// Map a shared buffer into a task's address space.
///
/// # Arguments
/// * `process_id` - Process to map into (from task.process_id)
/// * `token` - Buffer token from shm_create
/// * `access` - Access permissions (ReadOnly or ReadWrite)
///
/// # Returns
/// Virtual address on success, 0 on failure
pub fn shm_map(process_id: u32, token: u32, access: ShmAccess) -> u64 {
    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        klog_info!("shm_map: invalid process_id {}", process_id);
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

        // Check if already mapped for this process
        for mapping in buffer.mappings.iter() {
            if mapping.active && mapping.task_id == process_id {
                klog_debug!("shm_map: already mapped for process {}", process_id);
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
    let actual_access = if process_id == owner_task {
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
        task_id: process_id,
        virt_addr: vaddr,
        active: true,
    };
    buffer.mapping_count += 1;

    klog_debug!(
        "shm_map: mapped token={} at vaddr={:#x} for process={} access={:?}",
        token,
        vaddr,
        process_id,
        actual_access
    );

    vaddr
}

/// Unmap a shared buffer from a process's address space.
///
/// # Arguments
/// * `process_id` - Process to unmap from (from task.process_id)
/// * `virt_addr` - Virtual address returned by shm_map
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_unmap(process_id: u32, virt_addr: u64) -> c_int {
    let page_dir = process_vm_get_page_dir(process_id);
    if page_dir.is_null() {
        return -1;
    }

    let mut registry = REGISTRY.lock();

    // First pass: find the buffer and mapping, capture size for free list
    let mut found_info: Option<(usize, usize, usize)> = None; // (buffer_idx, mapping_idx, size)

    for (buf_idx, buffer) in registry.buffers.iter().enumerate() {
        if !buffer.active {
            continue;
        }

        for (map_idx, mapping) in buffer.mappings.iter().enumerate() {
            if mapping.active && mapping.task_id == process_id && mapping.virt_addr == virt_addr {
                found_info = Some((buf_idx, map_idx, buffer.size));
                break;
            }
        }
        if found_info.is_some() {
            break;
        }
    }

    let (buf_idx, map_idx, buffer_size) = match found_info {
        Some(info) => info,
        None => return -1,
    };

    // Unmap all pages
    let pages = registry.buffers[buf_idx].pages;
    for i in 0..pages {
        let page_vaddr = virt_addr + (i as u64) * PAGE_SIZE_4KB;
        unmap_page_in_dir(page_dir, page_vaddr);
    }

    // Clear the mapping
    registry.buffers[buf_idx].mappings[map_idx] = ShmMapping::empty();
    registry.buffers[buf_idx].mapping_count =
        registry.buffers[buf_idx].mapping_count.saturating_sub(1);

    // Return virtual address to free list for reuse
    registry.free_vaddr(virt_addr, buffer_size);

    klog_debug!(
        "shm_unmap: unmapped vaddr={:#x} for process={}, returned to free list",
        virt_addr,
        process_id
    );

    0
}

/// Destroy a shared buffer and free its memory.
///
/// Only the owner process can destroy a buffer.
/// All mappings will be forcibly unmapped.
///
/// # Arguments
/// * `process_id` - Process requesting destruction (must be owner, from task.process_id)
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_destroy(process_id: u32, token: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    // Only owner can destroy
    if registry.buffers[slot].owner_task != process_id {
        klog_info!(
            "shm_destroy: process {} is not owner of token {}",
            process_id,
            token
        );
        return -1;
    }

    // Collect mapping info for free list before clearing
    let buffer_size = registry.buffers[slot].size;
    let pages = registry.buffers[slot].pages;
    let phys_addr = registry.buffers[slot].phys_addr;

    // Collect virtual addresses to free (max 8 mappings per buffer)
    let mut vaddrs_to_free: [(u64, u32); MAX_MAPPINGS_PER_BUFFER] = [(0, 0); MAX_MAPPINGS_PER_BUFFER];
    let mut vaddr_count = 0;

    for mapping in registry.buffers[slot].mappings.iter() {
        if mapping.active {
            vaddrs_to_free[vaddr_count] = (mapping.virt_addr, mapping.task_id);
            vaddr_count += 1;
        }
    }

    // Unmap all pages from all tasks and collect vaddrs
    for i in 0..vaddr_count {
        let (vaddr, map_task_id) = vaddrs_to_free[i];
        let page_dir = process_vm_get_page_dir(map_task_id);
        if !page_dir.is_null() {
            for j in 0..pages {
                let page_vaddr = vaddr + (j as u64) * PAGE_SIZE_4KB;
                unmap_page_in_dir(page_dir, page_vaddr);
            }
        }
    }

    // Clear all mappings
    for mapping in registry.buffers[slot].mappings.iter_mut() {
        *mapping = ShmMapping::empty();
    }

    // Free physical pages
    for i in 0..pages {
        free_page_frame(phys_addr + (i as u64) * PAGE_SIZE_4KB);
    }

    // Clear the buffer slot
    registry.buffers[slot] = SharedBuffer::empty();

    // Return all virtual addresses to free list
    for i in 0..vaddr_count {
        let (vaddr, _) = vaddrs_to_free[i];
        registry.free_vaddr(vaddr, buffer_size);
    }

    klog_debug!("shm_destroy: destroyed token={} for process={}", token, process_id);

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
pub fn surface_attach(process_id: u32, token: u32, width: u32, height: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];

    // Only owner can attach (owner_task stores process_id)
    if buffer.owner_task != process_id {
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
        "surface_attach: token={} registered as {}x{} surface for process={}",
        token,
        width,
        height,
        process_id
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

    // Collect vaddrs to free from this task's mappings in other buffers
    // (max 64 buffers * 1 mapping per buffer for this task = 64 entries)
    let mut vaddrs_from_mappings: [(u64, usize); MAX_SHARED_BUFFERS] = [(0, 0); MAX_SHARED_BUFFERS];
    let mut mapping_vaddr_count = 0;

    // First, remove all mappings for this task from all buffers
    for buffer in registry.buffers.iter_mut() {
        if !buffer.active {
            continue;
        }

        for mapping in buffer.mappings.iter_mut() {
            if mapping.active && mapping.task_id == task_id {
                // Collect vaddr for free list
                if mapping_vaddr_count < MAX_SHARED_BUFFERS {
                    vaddrs_from_mappings[mapping_vaddr_count] = (mapping.virt_addr, buffer.size);
                    mapping_vaddr_count += 1;
                }
                // Note: page directory may already be torn down
                // Just clear the mapping record
                *mapping = ShmMapping::empty();
                buffer.mapping_count = buffer.mapping_count.saturating_sub(1);
            }
        }
    }

    // Collect info about buffers owned by this task
    // (max 64 buffers, each with up to 8 mappings)
    let mut owned_buffer_slots: [usize; MAX_SHARED_BUFFERS] = [0; MAX_SHARED_BUFFERS];
    let mut owned_count = 0;

    for (idx, buffer) in registry.buffers.iter().enumerate() {
        if buffer.active && buffer.owner_task == task_id {
            owned_buffer_slots[owned_count] = idx;
            owned_count += 1;
        }
    }

    // Collect vaddrs from owned buffers' mappings
    let mut vaddrs_from_owned: [(u64, usize); MAX_SHARED_BUFFERS] = [(0, 0); MAX_SHARED_BUFFERS];
    let mut owned_vaddr_count = 0;

    // Destroy all buffers owned by this task
    for i in 0..owned_count {
        let slot = owned_buffer_slots[i];
        let buffer = &mut registry.buffers[slot];
        let buffer_size = buffer.size;

        // Free physical pages
        for j in 0..buffer.pages {
            free_page_frame(buffer.phys_addr + (j as u64) * PAGE_SIZE_4KB);
        }

        // Collect vaddrs and unmap from other tasks
        for mapping in buffer.mappings.iter_mut() {
            if mapping.active {
                // Collect vaddr for free list
                if owned_vaddr_count < MAX_SHARED_BUFFERS {
                    vaddrs_from_owned[owned_vaddr_count] = (mapping.virt_addr, buffer_size);
                    owned_vaddr_count += 1;
                }

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

    // Return all virtual addresses to free list
    for i in 0..mapping_vaddr_count {
        let (vaddr, size) = vaddrs_from_mappings[i];
        registry.free_vaddr(vaddr, size);
    }
    for i in 0..owned_vaddr_count {
        let (vaddr, size) = vaddrs_from_owned[i];
        registry.free_vaddr(vaddr, size);
    }
}

// =============================================================================
// Wayland-style Buffer Acquire/Release Protocol
// =============================================================================

/// Acquire a buffer reference (compositor use only).
///
/// Called by the compositor when it starts using a client's buffer.
/// Increments the reference count and clears the released flag.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_acquire(token: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];
    buffer.ref_count = buffer.ref_count.saturating_add(1);
    buffer.released = false;

    klog_debug!(
        "shm_acquire: token={} ref_count={}",
        token,
        buffer.ref_count
    );

    0
}

/// Release a buffer reference (compositor use only).
///
/// Called by the compositor when it finishes using a client's buffer.
/// Decrements the reference count and signals the client that the buffer
/// can be reused by setting the released flag.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 0 on success, -1 on failure
pub fn shm_release(token: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];
    buffer.ref_count = buffer.ref_count.saturating_sub(1);
    buffer.released = true;

    klog_debug!(
        "shm_release: token={} ref_count={} released=true",
        token,
        buffer.ref_count
    );

    0
}

/// Poll whether a buffer has been released by the compositor.
///
/// Called by clients to check if they can safely reuse a buffer.
/// After polling returns true, the released flag is cleared.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// 1 if released (client can reuse), 0 if not released, -1 on error
pub fn shm_poll_released(token: u32) -> c_int {
    let mut registry = REGISTRY.lock();

    let slot = match registry.find_by_token(token) {
        Some(s) => s,
        None => return -1,
    };

    let buffer = &mut registry.buffers[slot];
    if buffer.released {
        buffer.released = false; // Clear after polling
        1
    } else {
        0
    }
}

/// Get the current reference count for a buffer.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// Reference count, or 0 if token is invalid
pub fn shm_get_ref_count(token: u32) -> u32 {
    let registry = REGISTRY.lock();

    match registry.find_by_token(token) {
        Some(slot) => registry.buffers[slot].ref_count,
        None => 0,
    }
}

// =============================================================================
// Pixel Format Negotiation (Wayland wl_shm format protocol)
// =============================================================================

/// Get the bitmap of supported pixel formats.
///
/// Returns a bitmap where bit N is set if PixelFormat with value N is supported.
/// Clients should query this to determine which formats to use.
///
/// # Returns
/// Bitmap of supported formats
pub fn shm_get_formats() -> u32 {
    SUPPORTED_FORMATS_BITMAP
}

/// Create a new shared memory buffer with a specific pixel format.
///
/// # Arguments
/// * `owner_task` - Task ID of the creator (owner)
/// * `size` - Size in bytes (will be rounded up to page boundary)
/// * `format` - Pixel format for this buffer
///
/// # Returns
/// Buffer token on success, 0 on failure
pub fn shm_create_with_format(owner_task: u32, size: u64, format: PixelFormat) -> u32 {
    // Validate format is supported
    if (SUPPORTED_FORMATS_BITMAP & (1 << format as u32)) == 0 {
        klog_info!("shm_create_with_format: unsupported format {:?}", format);
        return 0;
    }

    if size == 0 || size > 64 * 1024 * 1024 {
        klog_info!("shm_create_with_format: invalid size {}", size);
        return 0;
    }

    let aligned_size = align_up(size as usize, PAGE_SIZE_4KB as usize);
    let pages = (aligned_size / PAGE_SIZE_4KB as usize) as u32;

    // Allocate physical pages
    let phys_addr = alloc_page_frames(pages, ALLOC_FLAG_ZERO);
    if phys_addr == 0 {
        klog_info!("shm_create_with_format: failed to allocate {} pages", pages);
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
            klog_info!("shm_create_with_format: no free slots");
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
        ref_count: 1,
        released: false,
        format,
    };

    klog_debug!(
        "shm_create_with_format: created buffer token={} size={} format={:?} for task={}",
        token,
        aligned_size,
        format,
        owner_task
    );

    token
}

/// Get the pixel format of a shared buffer.
///
/// # Arguments
/// * `token` - Buffer token
///
/// # Returns
/// Pixel format as u32 (PixelFormat enum value), or u32::MAX on error
pub fn shm_get_format(token: u32) -> u32 {
    let registry = REGISTRY.lock();

    match registry.find_by_token(token) {
        Some(slot) => registry.buffers[slot].format as u32,
        None => u32::MAX,
    }
}
