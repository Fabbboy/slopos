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
use slopos_ostd::lock_class;

use slopos_abi::addr::PhysAddr;
use slopos_abi::file_ops::{FileKind, FileOps};
use slopos_abi::fs::UserFsStat;
use slopos_abi::io::{IoBufRead, IoBufWrite};
use slopos_abi::pixel::PixelFormat;
use slopos_abi::quota::ObjectRow;
use slopos_ostd::handle::{Handle, HandleTable};
use slopos_ostd::klog_debug;
use slopos_ostd::mm::frame::{claim_owned_anon_page, release_owned_anon_page};
use slopos_ostd::process::AccountId;
use slopos_ostd::process::quota::{Charge, try_charge};
use slopos_ostd::sync::{LOCK_LEVEL_RESOURCE, SpinLock};

use crate::page_alloc::{alloc_kernel_pages, free_page_frame};
use crate::paging_defs::PAGE_SIZE_4KB;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Maximum number of concurrent memfd objects system-wide.
const MAX_MEMFDS: usize = 64;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]). 8 bits cover MAX_MEMFDS (≤ 256) slots.
const SLOT_BITS: u32 = 8;

/// Maximum buffer size (64 MB).
const MAX_MEMFD_SIZE: usize = 64 * 1024 * 1024;

/// Handle to a memfd kernel object: a generation-checked [`Handle`] over the
/// registry's [`MemfdObject`] slots, so a handle left over from a closed memfd
/// whose slot was recycled resolves to a typed miss rather than aliasing the
/// recycled object. Stored typed in `VmaRegion`'s backing and packed into the
/// fd layer's `OpenFile.handle` via [`Handle::pack`].
pub type MemfdHandle = Handle<MemfdObject>;

/// Unpack the fd-stored `usize` (`OpenFile.handle`) into a generation-checked
/// handle.
pub(crate) fn handle_from_raw(raw: usize) -> MemfdHandle {
    Handle::unpack(raw, SLOT_BITS)
}

// ---------------------------------------------------------------------------
// MemfdObject — the kernel-side backing for a memfd file descriptor
// ---------------------------------------------------------------------------

/// One memfd's backing. Slot index and generation are owned by the
/// [`HandleTable`]; only the per-object state lives here. Public as the type
/// parameter of [`MemfdHandle`]; its fields stay private to this module.
pub struct MemfdObject {
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
    /// Number of active mapped pages pointing to this memfd's pages.
    /// Pages are freed only when refcount == 0 AND map_count == 0.
    map_count: u32,
}

// ---------------------------------------------------------------------------
// Registry — a generation-checked table of MemfdObject slots
// ---------------------------------------------------------------------------

static MEMFD_REGISTRY: SpinLock<Option<HandleTable<MemfdObject>>> =
    SpinLock::new(None, lock_class!("MEMFD_REGISTRY", LOCK_LEVEL_RESOURCE));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<MemfdObject>) -> R) -> R {
    let mut guard = MEMFD_REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_MEMFDS).expect("memfd registry alloc")
    });
    f(table)
}

// ---------------------------------------------------------------------------
// Lock-free hot-path arrays (for fb_flip compositor speed)
//
// Keyed by slot index, which the fixed-capacity table keeps stable for a
// memfd's lifetime, so a lock-free `memfd_get_phys` can index them directly
// without touching the registry table.
// ---------------------------------------------------------------------------

/// Physical address per slot — published on ftruncate, cleared on cleanup.
static MEMFD_PHYS: [AtomicU64; MAX_MEMFDS] = [const { AtomicU64::new(0) }; MAX_MEMFDS];
/// Size per slot — published on ftruncate, cleared on cleanup.
static MEMFD_SIZE: [AtomicU32; MAX_MEMFDS] = [const { AtomicU32::new(0) }; MAX_MEMFDS];

// ---------------------------------------------------------------------------
// Internal: free pages if both refcount and map_count are zero
// ---------------------------------------------------------------------------

/// Must be called with the registry lock held. Frees pages and removes the
/// slot once both refcount and map_count have reached zero.
fn try_cleanup(table: &mut HandleTable<MemfdObject>, handle: MemfdHandle) {
    let slot = handle.slot() as usize;
    let (phys, pages) = match table.get(handle) {
        Ok(obj) if obj.refcount == 0 && obj.map_count == 0 => (obj.phys_addr, obj.pages),
        _ => return,
    };

    // Clear the hot-path atomics BEFORE freeing so a racing lock-free
    // `memfd_get_phys` (fb_flip) never reads a phys addr pointing at a
    // reclaimed page.
    MEMFD_PHYS[slot].store(0, Ordering::Release);
    MEMFD_SIZE[slot].store(0, Ordering::Release);

    // Drop the slot (bumps its generation — any leftover handle goes stale).
    let _ = table.remove(handle);

    if !phys.is_null() && pages > 0 {
        for i in 0..pages {
            let page_addr = PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
            // Release the memfd's owning MetaSlot ref (claimed in
            // `memfd_ftruncate`). `map_count == 0` here means every mapping
            // ref is already gone, so this is the last ref: `Frame::drop`
            // republishes the slot UNUSED and returns the page to the buddy.
            // NEVER raw `free_page_frame` here — that would bypass the
            // MetaSlot and dump a still-live `Anonymous` frame into the free
            // list (the resize-time PathCorrupt double-owner bug).
            if !release_owned_anon_page(page_addr) {
                klog_debug!(
                    "memfd: cleanup slot={} page {} not live on release (desync)",
                    slot,
                    i
                );
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Public API — called by syscall handlers and mmap/munmap
// ---------------------------------------------------------------------------

/// Sole owner of one registry entry's fd reference; dropping it retires
/// the reference and frees the pages once no mapping pins them.
///
/// The object charge is the creator's, taken once. A per-alias charge would
/// double-count a shared memfd — the pages exist once however many processes
/// map them, and each *mapping* is a separate `Pages` charge on the mapper.
#[derive(slopos_ostd::Charged)]
struct MemfdBacking {
    handle: usize,
    object_charge: Charge<ObjectRow>,
}

slopos_ostd::charge_audit!(MemfdBacking);

impl slopos_ostd::process::quota::FileBacking for MemfdBacking {}

impl Drop for MemfdBacking {
    fn drop(&mut self) {
        memfd_release(self.handle);
    }
}

/// Create a new memfd object. Returns (packed_handle, &FileOps, backing)
/// for fd installation. The packed handle is stored in `OpenFile.handle`;
/// the backing's `Drop` is the close.
pub fn memfd_create(
    _flags: u32,
    account: AccountId,
) -> Option<(
    usize,
    &'static dyn FileOps,
    slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking>,
)> {
    let reservation = try_charge::<ObjectRow>(account, 1).ok()?;
    let raw = with_registry(|t| {
        t.insert(MemfdObject {
            phys_addr: PhysAddr::NULL,
            size: 0,
            pages: 0,
            format: PixelFormat::Argb8888,
            refcount: 1,
            map_count: 0,
        })
        .ok()
        .map(|h| h.pack(SLOT_BITS))
    })?;
    let backing: slopos_ostd::KArc<dyn slopos_ostd::process::quota::FileBacking> =
        match slopos_ostd::KArc::try_new(MemfdBacking {
            handle: raw,
            object_charge: Charge::commit(reservation),
        }) {
            Ok(b) => b,
            Err(_) => {
                memfd_release(raw);
                return None;
            }
        };
    Some((raw, &MEMFD_FILE_OPS, backing))
}

/// Set the size of a memfd (one-shot: only works when size is currently 0).
/// Allocates contiguous physical pages eagerly. `handle` is the packed fd
/// value from `OpenFile.handle`.
pub fn memfd_ftruncate(handle: usize, size: usize) -> c_int {
    let h = handle_from_raw(handle);
    if size == 0 || size > MAX_MEMFD_SIZE {
        return -22; // EINVAL
    }

    let aligned_size = (size + PAGE_SIZE_4KB as usize - 1) & !(PAGE_SIZE_4KB as usize - 1);
    let page_count = (aligned_size / PAGE_SIZE_4KB as usize) as u32;

    let phys = alloc_kernel_pages(page_count);
    if phys.is_null() {
        return -12; // ENOMEM
    }

    // Claim one owning MetaSlot ref per backing page so the memfd is the
    // SINGLE owner of these pages (SlopRing model). `mmap(MAP_SHARED)` then
    // adds a ref per mapping via `from_in_use`, and the page returns to the
    // buddy exactly once — when the last of {this owning ref, every
    // mapping} drops via `Frame::drop`. Without this, the first mapping's
    // `from_unused` would own the only MetaSlot ref and a later
    // `munmap`/exit would free the page out from under the still-open memfd
    // (the double-owner PathCorrupt). A claim failure means the buddy
    // handed back a non-UNUSED frame (an upstream desync); roll back and
    // surface ENOMEM rather than alias a live frame.
    let mut claimed = 0u32;
    while claimed < page_count {
        let page_addr = PhysAddr::new(phys.as_u64() + (claimed as u64) * PAGE_SIZE_4KB);
        if !claim_owned_anon_page(page_addr) {
            break;
        }
        claimed += 1;
    }
    if claimed != page_count {
        // Release the refs we did claim (each frees its page properly), then
        // raw-free the still-UNUSED tail starting at the page that failed.
        for i in 0..claimed {
            release_owned_anon_page(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
        for i in claimed..page_count {
            free_page_frame(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
        return -12; // ENOMEM
    }

    let slot = h.slot() as usize;
    let rc = with_registry(|t| match t.get_mut(h) {
        Ok(obj) if obj.size == 0 => {
            obj.phys_addr = phys;
            obj.size = aligned_size;
            obj.pages = page_count;
            // Publish the hot-path atomics under the lock, in lock-step with
            // the table view, so a lock-free fb_flip never sees a sized memfd
            // with a stale-zero atomic.
            MEMFD_PHYS[slot].store(phys.as_u64(), Ordering::Release);
            MEMFD_SIZE[slot].store(aligned_size as u32, Ordering::Release);
            0
        }
        Ok(_) => -22, // already sized (one-shot)
        Err(_) => -9, // EBADF — stale/invalid
    });

    if rc != 0 {
        // Registry update failed after we claimed the owning refs above —
        // release them (each frees its page through `Frame::drop`), never
        // raw-free a page whose MetaSlot we now own.
        for i in 0..page_count {
            release_owned_anon_page(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
    }
    rc
}

/// Lock-free physical address read (compositor fb_flip hot path). `handle` is
/// the packed fd value.
pub fn memfd_get_phys(handle: usize) -> (PhysAddr, usize) {
    let slot = handle_from_raw(handle).slot() as usize;
    if slot >= MAX_MEMFDS {
        return (PhysAddr::NULL, 0);
    }
    let phys = MEMFD_PHYS[slot].load(Ordering::Acquire);
    let size = MEMFD_SIZE[slot].load(Ordering::Acquire) as usize;
    (PhysAddr::new(phys), size)
}

/// Get physical address, size, and page count (for mmap, takes the lock).
pub(crate) fn memfd_get_info(handle: MemfdHandle) -> Option<(PhysAddr, usize, u32)> {
    with_registry(|t| match t.get(handle) {
        Ok(obj) if obj.size != 0 => Some((obj.phys_addr, obj.size, obj.pages)),
        _ => None,
    })
}

/// Increment map_count by `count` mapped pages.
pub(crate) fn memfd_inc_mapcount_by(handle: MemfdHandle, count: u32) {
    if count == 0 {
        return;
    }
    with_registry(|t| {
        if let Ok(obj) = t.get_mut(handle) {
            obj.map_count = obj.map_count.saturating_add(count);
        }
    });
}

/// Decrement map_count by `count` mapped pages.
pub(crate) fn memfd_dec_mapcount_by(handle: MemfdHandle, count: u32) {
    if count == 0 {
        return;
    }
    with_registry(|t| {
        if let Ok(obj) = t.get_mut(handle) {
            obj.map_count = obj.map_count.saturating_sub(count);
        }
        try_cleanup(t, handle);
    });
}

/// Decrement fd refcount (fired by the backing's `Drop` on last close).
/// `handle` is the packed fd value.
pub fn memfd_release(handle: usize) {
    let h = handle_from_raw(handle);
    with_registry(|t| {
        if let Ok(obj) = t.get_mut(h) {
            obj.refcount = obj.refcount.saturating_sub(1);
        }
        try_cleanup(t, h);
    });
}

/// Get the current size of a memfd. `handle` is the packed fd value.
pub fn memfd_size(handle: usize) -> usize {
    let h = handle_from_raw(handle);
    with_registry(|t| t.get(h).map(|o| o.size).unwrap_or(0))
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
