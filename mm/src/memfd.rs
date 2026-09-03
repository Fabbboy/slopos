//! Anonymous, fd-backed shared memory objects: sized once via ftruncate into a
//! contiguous physical allocation, mapped into multiple processes via
//! `mmap(MAP_SHARED)`, and passed between them via `sendmsg(SCM_RIGHTS)`.

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

const MAX_MEMFDS: usize = 64;

/// Slot-index bit width in the packed fd handle; the remaining bits hold the
/// generation (see [`Handle::pack`]). 8 bits cover MAX_MEMFDS (≤ 256) slots.
const SLOT_BITS: u32 = 8;

const MAX_MEMFD_SIZE: usize = 64 * 1024 * 1024;

/// Generation-checked handle over the registry's [`MemfdObject`] slots, so one
/// left over from a closed memfd whose slot was recycled resolves to a typed
/// miss rather than aliasing the recycled object.
pub type MemfdHandle = Handle<MemfdObject>;

/// Unpack an `OpenFile.handle` value into a generation-checked handle.
pub(crate) fn handle_from_raw(raw: usize) -> MemfdHandle {
    Handle::unpack(raw, SLOT_BITS)
}

/// One memfd's backing; slot index and generation are owned by the
/// [`HandleTable`].
pub struct MemfdObject {
    /// Base of the contiguous allocation; NULL until ftruncate.
    phys_addr: PhysAddr,
    /// Bytes; 0 until ftruncate, then page-aligned.
    size: usize,
    pages: u32,
    #[allow(dead_code)]
    format: PixelFormat,
    /// Open fd references: dup, fork and SCM_RIGHTS all increment.
    refcount: u32,
    /// Mapped pages referencing this memfd; pages are freed only once both
    /// this and `refcount` reach zero.
    map_count: u32,
}

static MEMFD_REGISTRY: SpinLock<Option<HandleTable<MemfdObject>>> =
    SpinLock::new(None, lock_class!("MEMFD_REGISTRY", LOCK_LEVEL_RESOURCE));

fn with_registry<R>(f: impl FnOnce(&mut HandleTable<MemfdObject>) -> R) -> R {
    let mut guard = MEMFD_REGISTRY.lock();
    let table = guard.get_or_insert_with(|| {
        HandleTable::with_fixed_capacity(MAX_MEMFDS).expect("memfd registry alloc")
    });
    f(table)
}

// Keyed by slot index, which the fixed-capacity table keeps stable for a
// memfd's lifetime, so `memfd_get_phys` can index them without the registry
// lock. Published on ftruncate, cleared on cleanup.
static MEMFD_PHYS: [AtomicU64; MAX_MEMFDS] = [const { AtomicU64::new(0) }; MAX_MEMFDS];
static MEMFD_SIZE: [AtomicU32; MAX_MEMFDS] = [const { AtomicU32::new(0) }; MAX_MEMFDS];

/// Caller holds the registry lock. Frees the pages and drops the slot once
/// both refcount and map_count have reached zero.
fn try_cleanup(table: &mut HandleTable<MemfdObject>, handle: MemfdHandle) {
    let slot = handle.slot() as usize;
    let (phys, pages) = match table.get(handle) {
        Ok(obj) if obj.refcount == 0 && obj.map_count == 0 => (obj.phys_addr, obj.pages),
        _ => return,
    };

    // Clear the hot-path atomics before freeing so a racing lock-free
    // `memfd_get_phys` never reads a phys addr pointing at a reclaimed page.
    MEMFD_PHYS[slot].store(0, Ordering::Release);
    MEMFD_SIZE[slot].store(0, Ordering::Release);

    let _ = table.remove(handle);

    if !phys.is_null() && pages > 0 {
        for i in 0..pages {
            let page_addr = PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB);
            // Releases the memfd's own MetaSlot ref, claimed in
            // `memfd_ftruncate`; with map_count == 0 this is the last ref.
            // Never raw `free_page_frame` here — that bypasses the MetaSlot
            // and dumps a still-live `Anonymous` frame into the free list.
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

/// Sole owner of one registry entry's fd reference; dropping it retires the
/// reference and frees the pages once no mapping pins them.
///
/// The object charge is the creator's, taken once: a per-alias charge would
/// double-count a shared memfd, whose pages exist once however many processes
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

/// Create a new memfd object. The packed handle is stored in
/// `OpenFile.handle`; the returned backing's `Drop` is the close.
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

/// Set the size of a memfd; one-shot, refused once the size is non-zero.
/// Allocates the contiguous physical pages eagerly.
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

    // Claim one owning MetaSlot ref per backing page so the memfd is the sole
    // owner: `mmap(MAP_SHARED)` then adds a ref per mapping, and the page
    // returns to the buddy only when the last of them drops. Without it the
    // first mapping would own the only ref and a later `munmap`/exit would
    // free the page out from under the still-open memfd. A claim failure means
    // the buddy handed back a non-UNUSED frame, so roll back rather than alias
    // a live frame.
    let mut claimed = 0u32;
    while claimed < page_count {
        let page_addr = PhysAddr::new(phys.as_u64() + (claimed as u64) * PAGE_SIZE_4KB);
        if !claim_owned_anon_page(page_addr) {
            break;
        }
        claimed += 1;
    }
    if claimed != page_count {
        // The claimed prefix must go back through its MetaSlot ref; the tail
        // from the failing page on is still UNUSED and raw-frees.
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
            // Published under the lock, in lock-step with the table view, so a
            // lock-free reader never sees a sized memfd with a zeroed atomic.
            MEMFD_PHYS[slot].store(phys.as_u64(), Ordering::Release);
            MEMFD_SIZE[slot].store(aligned_size as u32, Ordering::Release);
            0
        }
        Ok(_) => -22, // already sized (one-shot)
        Err(_) => -9, // EBADF — stale/invalid
    });

    if rc != 0 {
        // The owning refs are already claimed, so unwind through them; never
        // raw-free a page whose MetaSlot we now own.
        for i in 0..page_count {
            release_owned_anon_page(PhysAddr::new(phys.as_u64() + (i as u64) * PAGE_SIZE_4KB));
        }
    }
    rc
}

/// Lock-free physical address read, for the compositor's fb_flip hot path.
pub fn memfd_get_phys(handle: usize) -> (PhysAddr, usize) {
    let slot = handle_from_raw(handle).slot() as usize;
    if slot >= MAX_MEMFDS {
        return (PhysAddr::NULL, 0);
    }
    let phys = MEMFD_PHYS[slot].load(Ordering::Acquire);
    let size = MEMFD_SIZE[slot].load(Ordering::Acquire) as usize;
    (PhysAddr::new(phys), size)
}

/// Physical address, size and page count for mmap; takes the registry lock.
pub(crate) fn memfd_get_info(handle: MemfdHandle) -> Option<(PhysAddr, usize, u32)> {
    with_registry(|t| match t.get(handle) {
        Ok(obj) if obj.size != 0 => Some((obj.phys_addr, obj.size, obj.pages)),
        _ => None,
    })
}

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

/// Retire one fd reference; fired by the backing's `Drop` on last close.
pub fn memfd_release(handle: usize) {
    let h = handle_from_raw(handle);
    with_registry(|t| {
        if let Ok(obj) = t.get_mut(h) {
            obj.refcount = obj.refcount.saturating_sub(1);
        }
        try_cleanup(t, h);
    });
}

pub fn memfd_size(handle: usize) -> usize {
    let h = handle_from_raw(handle);
    with_registry(|t| t.get(h).map(|o| o.size).unwrap_or(0))
}

struct MemfdFileOps;

static MEMFD_FILE_OPS: MemfdFileOps = MemfdFileOps;

/// Placeholder for array initialisation; entries are overwritten before use.
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

    /// No backing store: durable the moment it is written.
    fn sync(&self, _handle: usize, _data_only: bool) -> i32 {
        0
    }

    fn seekable(&self) -> bool {
        false
    }
}
