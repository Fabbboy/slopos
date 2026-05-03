//! Memory File Descriptor (memfd) Subsystem
//!
//! Provides anonymous, fd-backed shared memory objects that can be:
//! - Sized via ftruncate (one-shot, contiguous physical allocation)
//! - Mapped into multiple processes via mmap(MAP_SHARED)
//! - Passed between processes via sendmsg(SCM_RIGHTS) over Unix sockets
//! - Used by the compositor for zero-copy buffer sharing
//!
//! Replaces the old token-based shared memory system with standard fd semantics.

use core::ffi::c_int;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::pixel::PixelFormat;
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};
use slopos_utils::klog_debug;

use crate::page_alloc::{ALLOC_FLAG_ZERO, alloc_page_frames, free_page_frame};
use crate::paging_defs::PAGE_SIZE_4KB;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of concurrent memfd objects system-wide.
const MAX_MEMFDS: usize = 64;

/// Bits used for the slot index in the handle encoding.
const SLOT_BITS: u32 = 8;
const SLOT_MASK: usize = (1 << SLOT_BITS) - 1; // 0xFF — supports up to 256 slots

/// Maximum buffer size (64 MB).
const MAX_MEMFD_SIZE: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// MemfdHandle — type-safe handle for memfd kernel objects
// ---------------------------------------------------------------------------

/// Opaque handle identifying a memfd kernel object.
///
/// Encodes a registry slot index and a generation counter for stale-handle
/// detection. Stored in `OpenFileEntry.handle` and `VmaRegion.backing`.
/// The encoding is `(generation << SLOT_BITS) | slot_index`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(transparent)]
pub struct MemfdHandle(u32);

impl MemfdHandle {
    pub const NONE: Self = Self(0);

    fn new(slot: usize, generation: u32) -> Self {
        Self(((generation as usize) << SLOT_BITS | (slot & SLOT_MASK)) as u32)
    }

    fn slot(self) -> usize {
        (self.0 as usize) & SLOT_MASK
    }

    fn generation(self) -> u32 {
        (self.0 as usize >> SLOT_BITS) as u32
    }

    pub fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Convert to usize for storage in OpenFileEntry.handle.
    pub fn as_usize(self) -> usize {
        self.0 as usize
    }

    /// Reconstruct from usize stored in OpenFileEntry.handle.
    pub fn from_usize(v: usize) -> Self {
        Self(v as u32)
    }

    /// Raw u32 value for storage in RegionBacking::SharedMemfd.
    pub fn raw(self) -> u32 {
        self.0
    }

    /// Reconstruct from raw u32 stored in RegionBacking::SharedMemfd.
    pub fn from_raw(v: u32) -> Self {
        Self(v)
    }
}

// ---------------------------------------------------------------------------
// MemfdObject — the kernel-side backing for a memfd file descriptor
// ---------------------------------------------------------------------------

struct MemfdObject {
    /// Base physical address of contiguous pages (NULL until ftruncate).
    phys_addr: PhysAddr,
    /// Size in bytes (0 until ftruncate, then page-aligned).
    size: usize,
    /// Number of 4 KB pages allocated.
    pages: u32,
    /// Pixel format hint (defaults to Argb8888). Used by get_formats.
    #[allow(dead_code)]
    format: PixelFormat,
    /// Number of open fd references (dup, fork, SCM_RIGHTS all increment).
    refcount: u32,
    /// Number of active mmap regions pointing to this memfd's pages.
    /// Pages are freed only when refcount == 0 AND map_count == 0.
    map_count: u32,
    /// Whether this slot is in use.
    active: bool,
    /// Monotonic generation counter for stale-handle detection.
    generation: u32,
}

impl MemfdObject {
    const fn new() -> Self {
        Self {
            phys_addr: PhysAddr::NULL,
            size: 0,
            pages: 0,
            format: PixelFormat::Argb8888,
            refcount: 0,
            map_count: 0,
            active: false,
            generation: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Registry — fixed-size array of MemfdObject slots
// ---------------------------------------------------------------------------

struct MemfdRegistry {
    slots: [MemfdObject; MAX_MEMFDS],
    next_generation: u32,
}

impl MemfdRegistry {
    const fn new() -> Self {
        Self {
            slots: [const { MemfdObject::new() }; MAX_MEMFDS],
            next_generation: 1,
        }
    }

    fn find_free_slot(&self) -> Option<usize> {
        self.slots.iter().position(|s| !s.active)
    }
}

static MEMFD_REGISTRY: SpinLock<MemfdRegistry> =
    SpinLock::new(MemfdRegistry::new(), LOCK_LEVEL_RESOURCE);

// ---------------------------------------------------------------------------
// Lock-free hot-path arrays (for fb_flip compositor speed)
// ---------------------------------------------------------------------------

/// Physical address per slot — published on ftruncate, cleared on cleanup.
static MEMFD_PHYS: [AtomicU64; MAX_MEMFDS] = [const { AtomicU64::new(0) }; MAX_MEMFDS];
/// Size per slot — published on ftruncate, cleared on cleanup.
static MEMFD_SIZE: [AtomicU32; MAX_MEMFDS] = [const { AtomicU32::new(0) }; MAX_MEMFDS];

// ---------------------------------------------------------------------------
// Handle encoding: (generation << SLOT_BITS) | slot_index
// ---------------------------------------------------------------------------

/// Validate a handle and return the slot index, or None if stale/invalid.
fn validate_handle(reg: &MemfdRegistry, handle: MemfdHandle) -> Option<usize> {
    let slot = handle.slot();
    if slot >= MAX_MEMFDS {
        return None;
    }
    let obj = &reg.slots[slot];
    if !obj.active || obj.generation != handle.generation() {
        return None;
    }
    Some(slot)
}

// ---------------------------------------------------------------------------
// Internal: free pages if both refcount and map_count are zero
// ---------------------------------------------------------------------------

/// Must be called with registry lock held. Frees pages and clears slot if
/// both refcount and map_count have reached zero.
fn try_cleanup(reg: &mut MemfdRegistry, slot: usize) {
    let obj = &mut reg.slots[slot];
    if obj.refcount == 0 && obj.map_count == 0 && obj.active {
        let phys = obj.phys_addr;
        let pages = obj.pages;

        // Clear hot-path atomics FIRST (prevents new lookups)
        MEMFD_PHYS[slot].store(0, Ordering::Release);
        MEMFD_SIZE[slot].store(0, Ordering::Release);

        // Free physical pages
        if !phys.is_null() && pages > 0 {
            for i in 0..pages {
                let page_addr = PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
                free_page_frame(page_addr);
            }
        }

        klog_debug!(
            "memfd: cleanup slot={} gen={} pages={}",
            slot,
            obj.generation,
            pages
        );

        obj.phys_addr = PhysAddr::NULL;
        obj.size = 0;
        obj.pages = 0;
        obj.refcount = 0;
        obj.map_count = 0;
        obj.active = false;
    }
}

// ---------------------------------------------------------------------------
// Public API — called by syscall handlers and mmap/munmap
// ---------------------------------------------------------------------------

/// Create a new memfd object. Returns (handle_as_usize, &FileOps) for fd installation.
pub fn memfd_create(_flags: u32) -> Option<(usize, &'static dyn FileOps)> {
    let mut reg = MEMFD_REGISTRY.lock();

    let slot = reg.find_free_slot()?;
    let gn = reg.next_generation;
    reg.next_generation = gn.wrapping_add(1);

    let obj = &mut reg.slots[slot];
    *obj = MemfdObject {
        phys_addr: PhysAddr::NULL,
        size: 0,
        pages: 0,
        format: PixelFormat::Argb8888,
        refcount: 1,
        map_count: 0,
        active: true,
        generation: gn,
    };

    let handle = MemfdHandle::new(slot, gn);
    klog_debug!(
        "memfd: create slot={} gen={} handle={:#x}",
        slot,
        gn,
        handle.raw()
    );

    Some((handle.as_usize(), &MEMFD_FILE_OPS))
}

/// Set the size of a memfd (one-shot: only works when size is currently 0).
/// Allocates contiguous physical pages eagerly.
pub fn memfd_ftruncate(handle: usize, size: usize) -> c_int {
    let h = MemfdHandle::from_usize(handle);
    if size == 0 || size > MAX_MEMFD_SIZE {
        return -22; // EINVAL
    }

    let aligned_size = (size + PAGE_SIZE_4KB as usize - 1) & !(PAGE_SIZE_4KB as usize - 1);
    let page_count = (aligned_size / PAGE_SIZE_4KB as usize) as u32;

    let phys = alloc_page_frames(page_count, ALLOC_FLAG_ZERO);
    if phys.is_null() {
        return -12; // ENOMEM
    }

    let mut reg = MEMFD_REGISTRY.lock();
    let Some(slot) = validate_handle(&reg, h) else {
        for i in 0..page_count {
            free_page_frame(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
        return -9; // EBADF
    };

    let obj = &mut reg.slots[slot];
    if obj.size != 0 {
        for i in 0..page_count {
            free_page_frame(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
        return -22; // EINVAL
    }

    obj.phys_addr = phys;
    obj.size = aligned_size;
    obj.pages = page_count;

    MEMFD_PHYS[slot].store(phys.as_u64(), Ordering::Release);
    MEMFD_SIZE[slot].store(aligned_size as u32, Ordering::Release);

    klog_debug!(
        "memfd: ftruncate slot={} size={} pages={}",
        slot,
        aligned_size,
        page_count
    );
    0
}

/// Lock-free physical address read (compositor fb_flip hot path).
pub fn memfd_get_phys(handle: usize) -> (PhysAddr, usize) {
    let h = MemfdHandle::from_usize(handle);
    let slot = h.slot();
    if slot >= MAX_MEMFDS {
        return (PhysAddr::NULL, 0);
    }
    let phys = MEMFD_PHYS[slot].load(Ordering::Acquire);
    let size = MEMFD_SIZE[slot].load(Ordering::Acquire) as usize;
    (PhysAddr::new(phys), size)
}

/// Get physical address, size, and page count (for mmap, takes lock).
pub fn memfd_get_info(handle: usize) -> Option<(PhysAddr, usize, u32)> {
    let h = MemfdHandle::from_usize(handle);
    let reg = MEMFD_REGISTRY.lock();
    let slot = validate_handle(&reg, h)?;
    let obj = &reg.slots[slot];
    if obj.size == 0 {
        return None;
    }
    Some((obj.phys_addr, obj.size, obj.pages))
}

/// Increment map_count (called by mmap for shared mappings).
pub fn memfd_inc_mapcount(handle: usize) {
    let h = MemfdHandle::from_usize(handle);
    let mut reg = MEMFD_REGISTRY.lock();
    if let Some(slot) = validate_handle(&reg, h) {
        reg.slots[slot].map_count += 1;
    }
}

/// Decrement map_count (called by munmap / process exit).
pub fn memfd_dec_mapcount(handle: usize) {
    let h = MemfdHandle::from_usize(handle);
    let mut reg = MEMFD_REGISTRY.lock();
    if let Some(slot) = validate_handle(&reg, h) {
        reg.slots[slot].map_count = reg.slots[slot].map_count.saturating_sub(1);
        try_cleanup(&mut reg, slot);
    }
}

/// Increment fd refcount (called by dup, fork, SCM_RIGHTS).
pub fn memfd_inc_ref(handle: usize) {
    let h = MemfdHandle::from_usize(handle);
    let mut reg = MEMFD_REGISTRY.lock();
    if let Some(slot) = validate_handle(&reg, h) {
        reg.slots[slot].refcount += 1;
    }
}

/// Decrement fd refcount (called by close / FileOps::release).
pub fn memfd_release(handle: usize) {
    let h = MemfdHandle::from_usize(handle);
    let mut reg = MEMFD_REGISTRY.lock();
    if let Some(slot) = validate_handle(&reg, h) {
        reg.slots[slot].refcount = reg.slots[slot].refcount.saturating_sub(1);
        try_cleanup(&mut reg, slot);
    }
}

/// Get the current size of a memfd.
pub fn memfd_size(handle: usize) -> usize {
    let h = MemfdHandle::from_usize(handle);
    let reg = MEMFD_REGISTRY.lock();
    match validate_handle(&reg, h) {
        Some(slot) => reg.slots[slot].size,
        None => 0,
    }
}

// ---------------------------------------------------------------------------
// FileOps implementation — plugs memfd into the VFS/fd system
// ---------------------------------------------------------------------------

struct MemfdFileOps;

static MEMFD_FILE_OPS: MemfdFileOps = MemfdFileOps;

/// Returns a dummy FileOps reference for array initialization.
/// Never actually called — entries are overwritten before use.
pub fn dummy_file_ops() -> &'static dyn FileOps {
    &MEMFD_FILE_OPS
}

impl FileOps for MemfdFileOps {
    fn kind(&self) -> FileKind {
        FileKind::Memfd
    }

    fn read(&self, _handle: usize, _buf: &mut dyn IoBufWrite, _offset: u64, _flags: u32) -> isize {
        -22 // EINVAL — use mmap instead
    }

    fn write(&self, _handle: usize, _buf: &dyn IoBufRead, _offset: u64, _flags: u32) -> isize {
        -22 // EINVAL — use mmap instead
    }

    fn release(&self, handle: usize) {
        memfd_release(handle);
    }

    fn dup(&self, handle: usize) -> Option<usize> {
        memfd_inc_ref(handle);
        Some(handle)
    }

    fn stat(&self, handle: usize, out: &mut UserFsStat) -> i32 {
        let size = memfd_size(handle);
        out.size = size as u32;
        out.type_ = 0;
        0
    }

    fn size(&self, handle: usize) -> Option<u64> {
        Some(memfd_size(handle) as u64)
    }

    fn seekable(&self) -> bool {
        false
    }
}
