use core::ffi::{c_int, c_void};
use core::mem;
use core::ptr::{self, NonNull};

use slopos_abi::addr::VirtAddr;
use slopos_ostd::sync::cpu_local::{CacheAligned, CpuLocal};
use slopos_ostd::sync::{ByteChain, LOCK_LEVEL_ALLOCATOR, RawLink, SpinLock};
use slopos_ostd::{align_down_u64, align_up_usize, klog_debug, klog_info};

use crate::memory_layout_defs::{KERNEL_HEAP_VBASE, KERNEL_HEAP_VEND};
use crate::page_alloc::{alloc_kernel_page, free_page_frame};
use crate::paging::{map_page_4kb, paging_bump_kernel_mapping_gen, unmap_page};
use crate::paging_defs::{PAGE_SIZE_4KB, PageFlags};

const NUM_SIZE_CLASSES: usize = 8;
const SLAB_DEBUG: bool = false;
const MAX_ALLOC_SIZE: usize = 0x100000;
const SLAB_MAGIC: u32 = 0x534C_4142;
const LARGE_MAGIC: u32 = 0x4C_4152_47;
const LARGE_FREE_MAGIC: u32 = 0x4C_4652_45;
const SLAB_POISON_FREED: u8 = 0x6B;
#[allow(dead_code)]
const SLAB_POISON_ALLOC: u8 = 0x5A;
#[allow(dead_code)]
const SLAB_REDZONE_HEAD: u32 = 0xBB_BB_BB_BB;
#[allow(dead_code)]
const SLAB_REDZONE_TAIL: u32 = 0xCC_CC_CC_CC;
const SIZE_CLASSES: [usize; NUM_SIZE_CLASSES] = [16, 32, 64, 128, 256, 512, 1024, 2048];

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct HeapStats {
    pub total_size: u64,
    pub allocated_size: u64,
    pub free_size: u64,
    pub total_blocks: u32,
    pub allocated_blocks: u32,
    pub free_blocks: u32,
    pub allocation_count: u32,
    pub free_count: u32,
}

impl HeapStats {
    fn record_slab_alloc(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_add(size);
        self.allocated_blocks = self.allocated_blocks.saturating_add(1);
        self.free_blocks = self.free_blocks.saturating_sub(1);
        self.allocation_count = self.allocation_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_large_alloc(&mut self, size: u64) {
        self.allocated_blocks = self.allocated_blocks.saturating_add(1);
        self.allocated_size = self.allocated_size.saturating_add(size);
        self.allocation_count = self.allocation_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_slab_free(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_sub(size);
        self.allocated_blocks = self.allocated_blocks.saturating_sub(1);
        self.free_blocks = self.free_blocks.saturating_add(1);
        self.free_count = self.free_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }

    fn record_large_free(&mut self, size: u64) {
        self.allocated_size = self.allocated_size.saturating_sub(size);
        self.allocated_blocks = self.allocated_blocks.saturating_sub(1);
        self.free_count = self.free_count.saturating_add(1);
        self.free_size = self.total_size.saturating_sub(self.allocated_size);
    }
}

#[repr(C)]
struct SlabHeader {
    magic: u32,
    object_size: u32,
    total_count: u16,
    free_count: u16,
    next: RawLink<SlabHeader>,
    free_list: ByteChain,
}

impl SlabHeader {
    /// Byte offset where the object array starts inside a slab page.
    #[inline]
    fn object_start_offset() -> usize {
        align_up_usize(mem::size_of::<SlabHeader>(), 16)
    }

    /// Pointer to object `idx` inside a slab page whose header lives at
    /// `slab_base`. Returns `None` if the object would extend past the
    /// page. Caller owns the slab page exclusively (slab pages are not
    /// shared across allocators).
    #[inline]
    fn object_at(slab_base: NonNull<u8>, idx: usize, object_size: usize) -> Option<NonNull<u8>> {
        let start = Self::object_start_offset();
        let off = start.checked_add(idx.checked_mul(object_size)?)?;
        if off.checked_add(object_size)? > PAGE_SIZE_4KB as usize {
            return None;
        }
        // SAFETY: bounds-checked above; the slab page is exclusively
        // owned by the lock holder for the duration of this call.
        let raw = unsafe { slab_base.as_ptr().add(off) };
        NonNull::new(raw)
    }

    /// Mutable byte view of object `obj`'s body region (the bytes after
    /// the inline link slot). Caller owns the slab page exclusively.
    #[inline]
    fn body_slice_mut<'a>(obj: NonNull<u8>, object_size: usize) -> Option<&'a mut [u8]> {
        let link_bytes = mem::size_of::<*mut u8>();
        if object_size <= link_bytes {
            return None;
        }
        let body_len = object_size - link_bytes;
        // Caller owns the slab page; `obj + link_bytes` lies strictly
        // inside the object's allocation slot.
        Some(slopos_ostd::util::ptr_buf::borrow_at_mut::<u8>(
            obj, link_bytes, body_len,
        ))
    }
}

#[repr(C)]
struct LargeAllocHeader {
    magic: u32,
    pages: u32,
    size: u32,
    reserved: u32,
    next: RawLink<LargeAllocHeader>,
}

impl LargeAllocHeader {
    /// Byte offset of the user-visible body within a large-alloc region.
    #[inline]
    fn body_offset() -> usize {
        align_up_usize(mem::size_of::<LargeAllocHeader>(), 16)
    }

    /// Body pointer for a header `header`. Caller owns the underlying
    /// region.
    #[inline]
    fn body_ptr(header: NonNull<LargeAllocHeader>) -> NonNull<u8> {
        // SAFETY: header was produced by `map_heap_pages`, so the
        // following bytes belong to the same large-alloc region.
        let raw = unsafe { (header.as_ptr() as *mut u8).add(Self::body_offset()) };
        // The body offset is always > 0 and `raw` is derived from a
        // non-null pointer, so the result is non-null.
        NonNull::new(raw).expect("large-alloc body pointer must be non-null")
    }

    /// Mutable byte view spanning `len` bytes starting at the body of a
    /// large-alloc header. Caller owns the region for the lifetime of the
    /// returned slice.
    #[inline]
    fn body_view_mut<'a>(header: NonNull<LargeAllocHeader>, len: usize) -> &'a mut [u8] {
        let body = Self::body_ptr(header);
        // Caller owns the large-alloc region; `body + len` is within
        // bounds whenever the caller passed a `len` derived from the
        // header's `pages` count.
        slopos_ostd::util::ptr_buf::borrow_nonnull_mut(body, len)
    }
}

struct SlabCache {
    object_size: usize,
    slabs: RawLink<SlabHeader>,
}

impl SlabCache {
    const fn empty() -> Self {
        Self {
            object_size: 0,
            slabs: RawLink::null(),
        }
    }
}

struct KernelHeap {
    start_addr: u64,
    end_addr: u64,
    current_break: u64,
    caches: [SlabCache; NUM_SIZE_CLASSES],
    large_free_list: RawLink<LargeAllocHeader>,
    stats: HeapStats,
    initialized: bool,
    diagnostics_enabled: bool,
}

impl KernelHeap {
    const fn new() -> Self {
        Self {
            start_addr: 0,
            end_addr: 0,
            current_break: 0,
            caches: [const { SlabCache::empty() }; NUM_SIZE_CLASSES],
            large_free_list: RawLink::null(),
            stats: HeapStats {
                total_size: 0,
                allocated_size: 0,
                free_size: 0,
                total_blocks: 0,
                allocated_blocks: 0,
                free_blocks: 0,
                allocation_count: 0,
                free_count: 0,
            },
            initialized: false,
            diagnostics_enabled: true,
        }
    }
}

static KERNEL_HEAP: SpinLock<KernelHeap> = SpinLock::new(KernelHeap::new(), LOCK_LEVEL_ALLOCATOR);

// ---------------------------------------------------------------------------
// Per-CPU object magazine caches — lock-free fast path for kmalloc/kfree.
//
// Each CPU has a magazine (small stack) per size class. The fast path pops/
// pushes from the magazine with only a PreemptGuard. The global KERNEL_HEAP
// lock is only taken for batch refill/drain (amortized over MAGAZINE_CAPACITY
// allocations).
// ---------------------------------------------------------------------------

use slopos_arch::pcr::{MAX_CPUS, get_current_cpu};
use slopos_ostd::sync::IrqPreemptGuard;

const MAGAZINE_CAPACITY: usize = 32;

/// One slot inside a magazine. `repr(transparent)` over `usize` so the
/// layout is identical to a raw pointer slot, but `usize` is naturally
/// `Send + Sync`, sparing the magazine its own marker.
#[repr(transparent)]
#[derive(Copy, Clone)]
struct ObjSlot(usize);

impl ObjSlot {
    const NULL: Self = Self(0);

    #[inline]
    fn from_ptr(p: *mut c_void) -> Self {
        Self(p as usize)
    }

    #[inline]
    fn as_ptr(self) -> *mut c_void {
        self.0 as *mut c_void
    }

    #[inline]
    fn is_null(self) -> bool {
        self.0 == 0
    }
}

/// Per-size-class magazine: a small stack of pre-allocated object pointers.
struct Magazine {
    objects: [ObjSlot; MAGAZINE_CAPACITY],
    count: u32,
}

impl Magazine {
    const fn new() -> Self {
        Self {
            objects: [ObjSlot::NULL; MAGAZINE_CAPACITY],
            count: 0,
        }
    }

    #[inline]
    fn pop(&mut self) -> *mut c_void {
        if self.count == 0 {
            return ptr::null_mut();
        }
        self.count -= 1;
        let slot = self.objects[self.count as usize];
        self.objects[self.count as usize] = ObjSlot::NULL;
        slot.as_ptr()
    }

    #[inline]
    fn push(&mut self, ptr: *mut c_void) -> bool {
        if (self.count as usize) >= MAGAZINE_CAPACITY {
            return false;
        }
        self.objects[self.count as usize] = ObjSlot::from_ptr(ptr);
        self.count += 1;
        true
    }
}

/// Per-CPU heap cache: one magazine per size class.
struct PerCpuHeapCache {
    magazines: [Magazine; NUM_SIZE_CLASSES],
}

impl PerCpuHeapCache {
    const fn new() -> Self {
        Self {
            magazines: [const { Magazine::new() }; NUM_SIZE_CLASSES],
        }
    }
}

static HEAP_CACHES: CpuLocal<PerCpuHeapCache> = {
    const INIT: CacheAligned<PerCpuHeapCache> = CacheAligned(PerCpuHeapCache::new());
    CpuLocal::new_with([INIT; MAX_CPUS])
};

static HEAP_CACHES_ENABLED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Heap bounds for lock-free range checks in the kfree fast path.
static HEAP_START: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
static HEAP_END: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// Magazine-level stats (objects currently cached, not visible to slab stats).
/// These represent objects that the slab considers "allocated" (they were
/// slab_alloc'd during refill) but are idle in a magazine.
static MAG_CACHED_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Cumulative magazine-served allocations (for allocation_count adjustment).
static MAG_ALLOC_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Cumulative magazine-absorbed frees (for free_count adjustment).
static MAG_FREE_COUNT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

/// Drain all per-CPU magazines, discarding cached objects.
///
/// Must be called before heap reinitialization to prevent stale pointers
/// from being handed out after the slab caches are reset.
pub fn drain_all_heap_caches() {
    if !HEAP_CACHES_ENABLED.load(core::sync::atomic::Ordering::Relaxed) {
        return;
    }
    HEAP_CACHES.for_each_mut_at_shutdown(|_cpu, cache| {
        for mag in cache.magazines.iter_mut() {
            mag.count = 0;
            mag.objects = [ObjSlot::NULL; MAGAZINE_CAPACITY];
        }
    });
    MAG_CACHED_COUNT.store(0, core::sync::atomic::Ordering::Relaxed);
    MAG_ALLOC_COUNT.store(0, core::sync::atomic::Ordering::Relaxed);
    MAG_FREE_COUNT.store(0, core::sync::atomic::Ordering::Relaxed);
}

/// Enable per-CPU heap caches. Called after heap and SMP are initialized.
pub fn enable_heap_caches() {
    // Publish heap bounds for lock-free range checks.
    let heap = KERNEL_HEAP.lock();
    HEAP_START.store(heap.start_addr, core::sync::atomic::Ordering::Relaxed);
    HEAP_END.store(heap.end_addr, core::sync::atomic::Ordering::Relaxed);
    drop(heap);
    HEAP_CACHES_ENABLED.store(true, core::sync::atomic::Ordering::Release);
}

/// Get the current CPU's heap cache. Caller must already hold a
/// `PreemptGuard` so `cpu` stays pinned for the borrow.
#[inline]
fn heap_cache(cpu: usize) -> &'static mut PerCpuHeapCache {
    HEAP_CACHES.get_pinned_mut(cpu)
}

/// Refill a magazine from the global slab. Takes KERNEL_HEAP lock once.
fn magazine_refill(mag: &mut Magazine, class_idx: usize) {
    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }
    let batch = MAGAZINE_CAPACITY / 2; // Refill half
    for _ in 0..batch {
        if (mag.count as usize) >= MAGAZINE_CAPACITY {
            break;
        }
        let ptr = slab_alloc_from_cache(&mut heap, class_idx);
        if ptr.is_null() {
            break;
        }
        mag.objects[mag.count as usize] = ObjSlot::from_ptr(ptr);
        mag.count += 1;
        MAG_CACHED_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    }
}

/// Drain half a magazine back to the global slab. Takes KERNEL_HEAP lock once.
fn magazine_drain(mag: &mut Magazine, _class_idx: usize) {
    let drain_count = mag.count as usize / 2;
    if drain_count == 0 {
        return;
    }
    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }
    for _ in 0..drain_count {
        if mag.count == 0 {
            break;
        }
        mag.count -= 1;
        let slot = mag.objects[mag.count as usize];
        mag.objects[mag.count as usize] = ObjSlot::NULL;
        if !slot.is_null() {
            let _ = slab_free(&mut heap, slot.as_ptr());
            MAG_CACHED_COUNT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
        }
    }
}

fn slab_object_start() -> usize {
    SlabHeader::object_start_offset()
}

fn slab_poison_object_body(obj: NonNull<u8>, object_size: usize) {
    if let Some(body) = SlabHeader::body_slice_mut(obj, object_size) {
        body.fill(SLAB_POISON_FREED);
    }
}

/// Read the leading two `u32`s (magic + object_size) of a candidate slab
/// page. Caller must have already verified `base_va` lies inside the
/// heap address range (HEAP_START..HEAP_END).
#[inline]
fn slab_magic_and_size_at(base_va: u64) -> (u32, u32) {
    // SAFETY: caller's bounds check guarantees `base_va` lies in the heap
    // mapping; the leading 8 bytes of any in-range slab page are either
    // (SLAB_MAGIC, object_size) for active slabs or (LARGE_MAGIC|0, _)
    // for large-alloc headers — both representations are valid `u32`.
    unsafe {
        let p = base_va as *const u32;
        (*p, *p.add(1))
    }
}

/// Read just the leading `u32` magic of a candidate heap region.
#[inline]
fn heap_magic_at(base_va: u64) -> u32 {
    // SAFETY: caller has checked `base_va` lies inside the heap mapping.
    unsafe { *(base_va as *const u32) }
}

/// Zero `len` bytes starting at `ptr`. Caller must own the buffer
/// exclusively (typically because they just allocated it).
#[inline]
fn zero_user_buffer(ptr: *mut u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    // Caller-owned buffer of `len` bytes.
    slopos_ostd::util::ptr_buf::borrow_buf_mut(ptr, len).fill(0);
}

/// Write a `HeapStats` snapshot to a C-ABI output slot if non-null.
#[inline]
fn write_optional_heap_stats(out: *mut HeapStats, value: HeapStats) {
    if out.is_null() {
        return;
    }
    // SAFETY: out is non-null per the check above; caller-owned slot.
    unsafe { *out = value };
}

fn size_class_index(size: usize) -> Option<usize> {
    for (idx, class) in SIZE_CLASSES.iter().enumerate() {
        if size <= *class {
            return Some(idx);
        }
    }
    None
}

fn map_heap_pages(heap: &mut KernelHeap, pages: u32) -> Option<u64> {
    if pages == 0 {
        return None;
    }

    let total_bytes = pages as u64 * PAGE_SIZE_4KB;
    if heap.current_break == 0 || heap.current_break + total_bytes > heap.end_addr {
        return None;
    }

    let start = heap.current_break;
    let mut mapped_pages = 0u32;

    for i in 0..pages {
        let phys_page = alloc_kernel_page();
        if phys_page.is_null() {
            rollback_mapping(start, mapped_pages);
            return None;
        }
        let virt_page = start + (i as u64) * PAGE_SIZE_4KB;
        if map_page_4kb(
            VirtAddr::new(virt_page),
            phys_page,
            PageFlags::KERNEL_RW.bits(),
        ) != 0
        {
            free_page_frame(phys_page);
            rollback_mapping(start, mapped_pages);
            return None;
        }
        mapped_pages += 1;
    }

    heap.current_break += total_bytes;
    heap.stats.total_size = heap.stats.total_size.saturating_add(total_bytes);
    heap.stats.free_size = heap
        .stats
        .total_size
        .saturating_sub(heap.stats.allocated_size);
    paging_bump_kernel_mapping_gen();

    Some(start)
}

fn rollback_mapping(start: u64, mapped_pages: u32) {
    for i in 0..mapped_pages {
        let virt_page = start + (i as u64) * PAGE_SIZE_4KB;
        let phys = unmap_page(VirtAddr::new(virt_page));
        if !phys.is_null() {
            free_page_frame(phys);
        }
    }
}

/// Build a `ByteChain` linking `total_count` objects starting at the given
/// `slab_base` page. Pushes in reverse so popping yields ascending indices,
/// matching the legacy in-order layout. Optionally poisons each object body
/// when `SLAB_DEBUG` is enabled.
fn slab_build_free_list(
    slab_base: NonNull<u8>,
    object_size: usize,
    total_count: usize,
) -> ByteChain {
    let chain = ByteChain::new();
    for i in (0..total_count).rev() {
        if let Some(obj) = SlabHeader::object_at(slab_base, i, object_size) {
            chain.push_front(obj);
        }
    }
    if SLAB_DEBUG {
        for i in 0..total_count {
            if let Some(obj) = SlabHeader::object_at(slab_base, i, object_size) {
                slab_poison_object_body(obj, object_size);
            }
        }
    }
    chain
}

fn slab_create(heap: &mut KernelHeap, object_size: usize) -> Option<NonNull<SlabHeader>> {
    let start = slab_object_start();
    if start >= PAGE_SIZE_4KB as usize {
        return None;
    }

    let available = PAGE_SIZE_4KB as usize - start;
    let total_count = available / object_size;
    if total_count == 0 {
        return None;
    }

    let slab_addr = map_heap_pages(heap, 1)?;
    let slab_base = NonNull::new(slab_addr as *mut u8)?;
    let free_list = slab_build_free_list(slab_base, object_size, total_count);

    let header_nn = slab_base.cast::<SlabHeader>();
    RawLink::<SlabHeader>::with_mut_at(Some(header_nn), |h| {
        h.magic = SLAB_MAGIC;
        h.object_size = object_size as u32;
        h.total_count = total_count as u16;
        h.free_count = total_count as u16;
        // `next` is null per the just-mapped zero-initialised page;
        // `free_list` is overwritten with the freshly built chain.
        h.next = RawLink::null();
        h.free_list = free_list;
    });

    heap.stats.total_blocks = heap.stats.total_blocks.saturating_add(total_count as u32);
    heap.stats.free_blocks = heap.stats.free_blocks.saturating_add(total_count as u32);

    Some(header_nn)
}

/// Outcome of one slab visit during `slab_alloc_from_cache`.
enum SlabVisit {
    /// Allocated `obj` from this slab; record the object size for stats.
    Allocated { obj: NonNull<u8>, object_size: u32 },
    /// Slab said it had a free object but its free-list head was empty.
    /// Indicates metadata corruption; abort the search.
    HeadlessFree,
    /// This slab has no free objects; advance to the next.
    Skip,
}

fn slab_alloc_from_cache(heap: &mut KernelHeap, idx: usize) -> *mut c_void {
    let mut current = heap.caches[idx].slabs.load();
    while let Some(slab_nn) = current {
        let slab_start = slab_nn.as_ptr() as usize;
        let slab_end = slab_start + PAGE_SIZE_4KB as usize;
        // Operate on the slab through the safe RawLink reborrow.
        let visit = RawLink::<SlabHeader>::with_mut_at(Some(slab_nn), |slab| {
            if slab.free_count == 0 {
                return SlabVisit::Skip;
            }
            let Some(obj) = slab.free_list.pop_front() else {
                return SlabVisit::HeadlessFree;
            };
            // After pop_front, the chain head advanced to the previous
            // object's embedded next-pointer. Validate it lies within
            // this slab's page; sever and continue if it escaped.
            if let Some(next) = slab.free_list.head() {
                let next_addr = next.as_ptr() as usize;
                if next_addr < slab_start || next_addr >= slab_end {
                    klog_info!(
                        "slab_alloc: corrupt next ptr 0x{:x} in obj 0x{:x}, slab [0x{:x}..0x{:x}], obj_size={}",
                        next_addr,
                        obj.as_ptr() as usize,
                        slab_start,
                        slab_end,
                        slab.object_size
                    );
                    slab.free_list.set_head(None);
                    slab.free_count = 0;
                    return SlabVisit::Allocated {
                        obj,
                        object_size: slab.object_size,
                    };
                }
            }

            if SLAB_DEBUG {
                let _ = (SLAB_REDZONE_HEAD, SLAB_REDZONE_TAIL);
                let object_size = slab.object_size as usize;
                if let Some(body) = SlabHeader::body_slice_mut(obj, object_size) {
                    let mut corrupt_off: Option<usize> = None;
                    let mut corrupt_byte: u8 = 0;
                    for (off, &b) in body.iter().enumerate() {
                        if b != SLAB_POISON_FREED {
                            corrupt_off = Some(off);
                            corrupt_byte = b;
                            break;
                        }
                    }
                    if corrupt_off.is_some() {
                        klog_info!(
                            "slab_alloc: POISON CHECK FAILED at 0x{:x}, expected 0x{:02X} found 0x{:02X}, obj_size={}",
                            obj.as_ptr() as usize,
                            SLAB_POISON_FREED,
                            corrupt_byte,
                            object_size
                        );
                        let preview_len = body.len().min(16);
                        let preview = &body[..preview_len];
                        klog_info!("slab_alloc: body_first{}={:02x?}", preview_len, preview);
                    }
                }
            }

            slab.free_count = slab.free_count.saturating_sub(1);
            SlabVisit::Allocated {
                obj,
                object_size: slab.object_size,
            }
        });

        match visit {
            Some(SlabVisit::Allocated { obj, object_size }) => {
                heap.stats.record_slab_alloc(object_size as u64);
                return obj.as_ptr() as *mut c_void;
            }
            Some(SlabVisit::HeadlessFree) => return ptr::null_mut(),
            Some(SlabVisit::Skip) | None => {
                // Step to the next slab in this cache's list.
                current =
                    RawLink::<SlabHeader>::with_mut_at(Some(slab_nn), |s| s.next.load()).flatten();
            }
        }
    }

    // No slab has free space — create a new one and prepend it.
    let object_size = heap.caches[idx].object_size;
    let new_slab = match slab_create(heap, object_size) {
        Some(s) => s,
        None => return ptr::null_mut(),
    };
    let prev_head = heap.caches[idx].slabs.load();
    RawLink::<SlabHeader>::with_mut_at(Some(new_slab), |s| s.next.store(prev_head));
    heap.caches[idx].slabs.store(Some(new_slab));

    slab_alloc_from_cache(heap, idx)
}

/// Read-only snapshot of a `LargeAllocHeader` taken under exclusive
/// access. Returned by `with_large_header_*` helpers so callers can
/// chain control flow without holding a reborrowed `&mut`.
#[derive(Clone, Copy)]
struct LargeHeaderSnapshot {
    pages: u32,
    next: Option<NonNull<LargeAllocHeader>>,
}

fn alloc_large(heap: &mut KernelHeap, size: usize) -> *mut c_void {
    let header_size = LargeAllocHeader::body_offset();
    let total = size.saturating_add(header_size);
    let pages = align_up_usize(total, PAGE_SIZE_4KB as usize) / PAGE_SIZE_4KB as usize;

    if pages == 0 {
        return ptr::null_mut();
    }

    // Free-list first-fit walk: find a header whose `pages >= pages`.
    let mut prev: Option<NonNull<LargeAllocHeader>> = None;
    let mut current = heap.large_free_list.load();
    while let Some(curr_nn) = current {
        let snap =
            RawLink::<LargeAllocHeader>::with_mut_at(Some(curr_nn), |h| LargeHeaderSnapshot {
                pages: h.pages,
                next: h.next.load(),
            });
        let snap = match snap {
            Some(s) => s,
            None => break,
        };

        if snap.pages as usize >= pages {
            // Detach `curr` from the free list.
            match prev {
                None => heap.large_free_list.store(snap.next),
                Some(p) => {
                    RawLink::<LargeAllocHeader>::with_mut_at(Some(p), |h| h.next.store(snap.next));
                }
            }

            if SLAB_DEBUG {
                let total_bytes = (snap.pages as usize) * PAGE_SIZE_4KB as usize - header_size;
                let check_len = size.min(total_bytes).min(64);
                let body_view = LargeAllocHeader::body_view_mut(curr_nn, check_len);
                let mut corrupt_off: Option<usize> = None;
                let mut corrupt_byte: u8 = 0;
                for (off, &b) in body_view.iter().enumerate() {
                    if b != SLAB_POISON_FREED {
                        corrupt_off = Some(off);
                        corrupt_byte = b;
                        break;
                    }
                }
                if let Some(off) = corrupt_off {
                    klog_info!(
                        "alloc_large: POISON CHECK FAILED at 0x{:x}+{}, expected 0x{:02X} found 0x{:02X}, size={}, pages={}",
                        curr_nn.as_ptr() as u64,
                        off,
                        SLAB_POISON_FREED,
                        corrupt_byte,
                        size,
                        snap.pages
                    );
                }
            }

            RawLink::<LargeAllocHeader>::with_mut_at(Some(curr_nn), |h| {
                h.magic = LARGE_MAGIC;
                h.size = size as u32;
                h.next = RawLink::null();
            });
            heap.stats.record_large_alloc(size as u64);
            return LargeAllocHeader::body_ptr(curr_nn).as_ptr() as *mut c_void;
        }

        prev = Some(curr_nn);
        current = snap.next;
    }

    // No reusable header — allocate a fresh region.
    let base = match map_heap_pages(heap, pages as u32) {
        Some(addr) => addr,
        None => return ptr::null_mut(),
    };
    let header_nn = match NonNull::new(base as *mut LargeAllocHeader) {
        Some(p) => p,
        None => return ptr::null_mut(),
    };
    RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
        h.magic = LARGE_MAGIC;
        h.pages = pages as u32;
        h.size = size as u32;
        h.reserved = 0;
        h.next = RawLink::null();
    });

    heap.stats.total_blocks = heap.stats.total_blocks.saturating_add(1);
    heap.stats.record_large_alloc(size as u64);

    LargeAllocHeader::body_ptr(header_nn).as_ptr() as *mut c_void
}

fn free_large(heap: &mut KernelHeap, base: u64) -> c_int {
    let header_nn = match NonNull::new(base as *mut LargeAllocHeader) {
        Some(p) => p,
        None => return -1,
    };

    let mut size_freed: u64 = 0;
    let mut total_pages: u32 = 0;
    let prev_head = heap.large_free_list.load();
    let updated = RawLink::<LargeAllocHeader>::with_mut_at(Some(header_nn), |h| {
        if h.magic != LARGE_MAGIC {
            return false;
        }
        size_freed = h.size as u64;
        total_pages = h.pages;
        h.magic = LARGE_FREE_MAGIC;
        h.next.store(prev_head);
        true
    });
    if !matches!(updated, Some(true)) {
        return -1;
    }
    heap.large_free_list.store(Some(header_nn));
    heap.stats.record_large_free(size_freed);

    if SLAB_DEBUG {
        let hdr_sz = LargeAllocHeader::body_offset();
        let total_bytes = (total_pages as usize) * PAGE_SIZE_4KB as usize;
        if total_bytes > hdr_sz {
            let body_len = total_bytes - hdr_sz;
            LargeAllocHeader::body_view_mut(header_nn, body_len).fill(SLAB_POISON_FREED);
        }
    }

    0
}

fn slab_free(heap: &mut KernelHeap, ptr_in: *mut c_void) -> c_int {
    let base_addr = align_down_u64(ptr_in as u64, PAGE_SIZE_4KB);
    let base_nn = match NonNull::new(base_addr as *mut SlabHeader) {
        Some(p) => p,
        None => return -1,
    };
    let obj_nn = match NonNull::new(ptr_in as *mut u8) {
        Some(p) => p,
        None => return -1,
    };

    let slab_start = base_nn.as_ptr() as usize;
    let slab_end = slab_start + PAGE_SIZE_4KB as usize;
    let ptr_addr = ptr_in as usize;
    let mut object_size_for_stats: u32 = 0;

    let outcome = RawLink::<SlabHeader>::with_mut_at(Some(base_nn), |slab| -> c_int {
        if slab.magic != SLAB_MAGIC {
            return -1;
        }

        let object_size = slab.object_size as usize;
        let object_base = slab_start.saturating_add(slab_object_start());
        if ptr_addr < object_base || ptr_addr >= slab_end {
            return -1;
        }

        let offset = ptr_addr - object_base;
        if offset % object_size != 0 {
            return -1;
        }

        // Walk the existing free chain to detect double-free / corruption.
        let mut current = slab.free_list.head();
        while let Some(curr) = current {
            let cur_addr = curr.as_ptr() as usize;
            if cur_addr < slab_start || cur_addr >= slab_end {
                klog_info!(
                    "slab_free: corrupt free-list ptr 0x{:x} outside slab [0x{:x}..0x{:x}], obj_size={}",
                    cur_addr,
                    slab_start,
                    slab_end,
                    object_size
                );
                break;
            }
            if cur_addr == ptr_addr {
                return -1;
            }
            current = ByteChain::read_next(curr);
        }

        slab.free_list.push_front(obj_nn);
        slab.free_count = slab.free_count.saturating_add(1);
        object_size_for_stats = slab.object_size;

        if SLAB_DEBUG {
            let _ = (SLAB_REDZONE_HEAD, SLAB_REDZONE_TAIL);
            slab_poison_object_body(obj_nn, object_size);
        }

        0
    });

    match outcome {
        Some(0) => {
            heap.stats.record_slab_free(object_size_for_stats as u64);
            0
        }
        Some(rc) => rc,
        None => -1,
    }
}

/// Allocate `size` bytes of kernel heap memory, zeroed.
///
/// Always zero-initialised. Slab/magazine recycling is the bug class
/// behind `0xdfdedddcdbdad9d8`-shape wild RIPs — a freed chunk that
/// retained `(i & 0xFF) as u8`-pattern bytes from a kernel test, was
/// reused as control-flow data, and decoded as a return address. The
/// safe public surface scrubs unconditionally; there is no
/// uninit-leak escape hatch.
pub fn kmalloc(size: usize) -> *mut c_void {
    if size == 0 || size > MAX_ALLOC_SIZE {
        return ptr::null_mut();
    }

    let rounded_size = align_up_usize(size, 16);

    // Per-CPU magazine fast path for slab-sized allocations.
    // The magazine avoids the global KERNEL_HEAP lock for common sizes.
    let raw = if let Some(class_idx) = size_class_index(rounded_size) {
        if HEAP_CACHES_ENABLED.load(core::sync::atomic::Ordering::Relaxed)
            && !KERNEL_HEAP.is_locked()
        {
            let _pin = IrqPreemptGuard::new();
            let cpu = get_current_cpu();
            let cache = heap_cache(cpu);
            let mag = &mut cache.magazines[class_idx];

            // Fast path: pop from magazine. No lock, no atomic.
            let ptr = mag.pop();
            if !ptr.is_null() {
                MAG_CACHED_COUNT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                MAG_ALLOC_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                ptr
            } else {
                // Slow path: refill magazine from global slab, then pop.
                magazine_refill(mag, class_idx);
                let ptr = mag.pop();
                if !ptr.is_null() {
                    MAG_CACHED_COUNT.fetch_sub(1, core::sync::atomic::Ordering::Relaxed);
                    MAG_ALLOC_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                }
                ptr
            }
        } else {
            ptr::null_mut()
        }
    } else {
        ptr::null_mut()
    };

    let ptr_out = if raw.is_null() {
        // Fallback: global lock (large allocs, or heap caches not yet enabled).
        let mut heap = KERNEL_HEAP.lock();
        if !heap.initialized {
            return ptr::null_mut();
        }
        if let Some(idx) = size_class_index(rounded_size) {
            slab_alloc_from_cache(&mut heap, idx)
        } else {
            alloc_large(&mut heap, rounded_size)
        }
    } else {
        raw
    };

    if ptr_out.is_null() {
        return ptr::null_mut();
    }
    // Zero exactly the requested size (rounded to 16-byte slab quantum
    // upstream). Slab objects' tail padding past `size` is never read
    // by the caller, so we don't need to scrub the whole rounded chunk.
    zero_user_buffer(ptr_out as *mut u8, size);
    ptr_out
}

/// **Deprecated, kept for source compatibility.** [`kmalloc`] now
/// zeroes by default; this is just a transparent alias.
pub fn kzalloc(size: usize) -> *mut c_void {
    kmalloc(size)
}

pub fn kfree(ptr_in: *mut c_void) {
    if ptr_in.is_null() {
        return;
    }

    // Per-CPU magazine fast path: if this is a slab object and caches are
    // enabled, push it to the per-CPU magazine without the global lock.
    if HEAP_CACHES_ENABLED.load(core::sync::atomic::Ordering::Relaxed) {
        let base = align_down_u64(ptr_in as u64, PAGE_SIZE_4KB);
        let heap_start = HEAP_START.load(core::sync::atomic::Ordering::Relaxed);
        let heap_end = HEAP_END.load(core::sync::atomic::Ordering::Relaxed);

        // Only access the slab header if the pointer is within the heap range
        // and the heap lock isn't already held (avoids recursive locking in drain).
        if base >= heap_start && base < heap_end && !KERNEL_HEAP.is_locked() {
            let (magic, raw_size) = slab_magic_and_size_at(base);
            if magic == SLAB_MAGIC {
                let object_size = raw_size as usize;
                if let Some(class_idx) = size_class_index(object_size) {
                    let _pin = IrqPreemptGuard::new();
                    let cpu = get_current_cpu();
                    let cache = heap_cache(cpu);
                    let mag = &mut cache.magazines[class_idx];
                    if mag.push(ptr_in) {
                        MAG_CACHED_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        MAG_FREE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                    magazine_drain(mag, class_idx);
                    if mag.push(ptr_in) {
                        MAG_CACHED_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        MAG_FREE_COUNT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                        return;
                    }
                }
            }
        }
    }

    // Fallback: global lock path (large allocs, or magazine full).
    let mut heap = KERNEL_HEAP.lock();
    if !heap.initialized {
        return;
    }

    let base = align_down_u64(ptr_in as u64, PAGE_SIZE_4KB);
    if base < heap.start_addr || base >= heap.current_break {
        klog_info!(
            "kfree: ptr 0x{:x} outside heap [0x{:x}..0x{:x}]",
            ptr_in as u64,
            heap.start_addr,
            heap.current_break
        );
        return;
    }
    let slab_result = slab_free(&mut heap, ptr_in);
    if slab_result == 0 {
        return;
    }

    let large_result = free_large(&mut heap, base);
    if large_result == 0 {
        return;
    }

    let magic_at_base = heap_magic_at(base);
    klog_info!(
        "kfree: no owner for ptr 0x{:x} base 0x{:x} magic=0x{:08x} (slab_rc={} large_rc={})",
        ptr_in as u64,
        base,
        magic_at_base,
        slab_result,
        large_result
    );
}

/// Minimum pages required for soft reboot coherency fix.
/// See documentation in `init_kernel_heap()` for details.
pub const HEAP_WARMUP_PAGES: u32 = 4;

pub fn init_kernel_heap() -> c_int {
    // Drain per-CPU magazines before reinitializing — prevents stale
    // pointers from old slabs being handed out after the reset.
    drain_all_heap_caches();

    let mut heap = KERNEL_HEAP.lock();
    heap.start_addr = KERNEL_HEAP_VBASE;
    heap.end_addr = KERNEL_HEAP_VEND;
    heap.current_break = heap.start_addr;

    for (idx, size) in SIZE_CLASSES.iter().enumerate() {
        heap.caches[idx].object_size = *size;
        heap.caches[idx].slabs.store(None);
    }

    heap.stats = HeapStats::default();
    heap.large_free_list.store(None);

    // ============================================================================
    // SOFT REBOOT COHERENCY FIX - DO NOT REMOVE
    // ============================================================================
    //
    // After soft reboot (PS/2 0xFE reset), x86 paging structure caches may retain
    // stale entries from the previous boot. Limine creates fresh page tables, but
    // the CPU's internal paging structure caches aren't automatically coherent.
    //
    // This causes framebuffer performance to degrade from ~60 FPS to ~1 FPS because:
    // 1. Stale paging structure cache entries point to old page table locations
    // 2. PAT (Page Attribute Table) settings for Write-Combining aren't applied
    // 3. Framebuffer writes fall back to uncached mode (~37,000 cycles/pixel)
    //
    // The fix requires BOTH:
    // - ≥2 physical frame allocations: Forces buddy allocator metadata coherency
    //   via read-after-write serialization on the bitmap/free list structures
    // - ≥1 page mapping: Forces page table walks that populate paging structure
    //   caches with fresh entries from Limine's new page tables
    //
    // `map_heap_pages(4)` satisfies both requirements (4 allocs + 4 maps).
    // Experiments confirmed 2 pages minimum works, but 4 provides safety margin.
    //
    // References:
    // - Intel Application Note 317080-002: "TLBs, Paging-Structure Caches, and
    //   Their Invalidation"
    // - https://blog.stuffedcow.net/2015/08/pagewalk-coherence/
    //
    // WARNING: Removing or reducing this below 2 pages WILL cause framebuffer
    // performance regression after soft reboot. See test_heap_warmup_pages_minimum().
    // ============================================================================
    if map_heap_pages(&mut heap, HEAP_WARMUP_PAGES).is_none() {
        panic!("Failed to initialize kernel heap");
    }

    heap.initialized = true;
    klog_debug!("Kernel heap initialized at 0x{:x}", heap.start_addr);
    0
}

pub fn get_heap_stats(stats: *mut HeapStats) {
    let heap = KERNEL_HEAP.lock();
    let mut s = heap.stats;
    // Magazine-served allocs/frees don't touch slab stats. Add them so
    // callers see accurate totals.
    let mag_allocs = MAG_ALLOC_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    let mag_frees = MAG_FREE_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    s.allocation_count = s.allocation_count.saturating_add(mag_allocs);
    s.free_count = s.free_count.saturating_add(mag_frees);
    write_optional_heap_stats(stats, s);
}

/// Owned-return variant of [`get_heap_stats`] for callers that want a
/// safe-fn surface without raw-ptr handoff. The pre-existing
/// `get_heap_stats(*mut HeapStats)` form stays for the C-ABI shim.
pub fn get_heap_stats_owned() -> HeapStats {
    let heap = KERNEL_HEAP.lock();
    let mut s = heap.stats;
    let mag_allocs = MAG_ALLOC_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    let mag_frees = MAG_FREE_COUNT.load(core::sync::atomic::Ordering::Relaxed);
    s.allocation_count = s.allocation_count.saturating_add(mag_allocs);
    s.free_count = s.free_count.saturating_add(mag_frees);
    s
}

pub fn kernel_heap_enable_diagnostics(enable: c_int) {
    let mut heap = KERNEL_HEAP.lock();
    heap.diagnostics_enabled = enable != 0;
}

pub fn print_heap_stats() {
    let heap = KERNEL_HEAP.lock();

    klog_info!("=== Kernel Heap Statistics ===");
    klog_info!("Total size: {} bytes", heap.stats.total_size);
    klog_info!("Allocated: {} bytes", heap.stats.allocated_size);
    klog_info!("Free: {} bytes", heap.stats.free_size);
    klog_info!("Allocations: {}", heap.stats.allocation_count);
    klog_info!("Frees: {}", heap.stats.free_count);

    if !heap.diagnostics_enabled {
        return;
    }

    for cache in heap.caches.iter() {
        if cache.object_size == 0 {
            continue;
        }

        let mut total = 0u32;
        let mut free = 0u32;
        let mut current = cache.slabs.load();
        while let Some(slab_nn) = current {
            let next = RawLink::<SlabHeader>::with_mut_at(Some(slab_nn), |s| {
                total += s.total_count as u32;
                free += s.free_count as u32;
                s.next.load()
            })
            .flatten();
            current = next;
        }

        if total > 0 {
            klog_info!(
                "Slab {}B: free {} / total {}",
                cache.object_size,
                free,
                total
            );
        }
    }
}
