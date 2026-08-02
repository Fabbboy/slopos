//! Per-size-class slab allocator.
//!
//! Each class is its own type parameterised over `const SIZE: usize`;
//! the aggregator [`super::KernelSlab`] owns one instance of each.
//! Each instance holds:
//! - its own `SpinLock<SlabClassState>` over the partial-slab list,
//! - per-CPU magazines for a lock-free fast path,
//! - per-class counters.
//!
//! Slab pages are allocated lazily from the buddy via
//! [`super::page::alloc_slab_page`] and tracked intrusively through
//! the slab page header's `next: RawLink<SlabHeader>` field — no extra
//! heap allocation, which is required because the slab IS the heap and
//! cannot recurse into itself during init.
//!
//! ## Verification (Inv. 9)
//!
//! Once [`SlabAllocator::build_slab_page`] claims a page from the buddy the
//! caller links it on the class's partial list and never returns it — there is no
//! `free_kernel_page` on the steady-state alloc/dealloc path, so a cell
//! handed out by [`SlabAllocator::alloc_one`] can never outlive its page.
//! `verification/proofs/slab_lifetime.rs` machine-checks the stronger
//! general rule (a page may only be reclaimed with zero outstanding cells)
//! and proves the broken "reclaim with live cells" violates Inv. 9 — so the
//! never-free discipline here is the conservative instance of a verified
//! guard. See `verification/STATUS.md`.

use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use slopos_arch::pcr::{MAX_CPUS, get_current_cpu};
use slopos_ostd::panic::AbortOnUnwind;
use slopos_ostd::sync::cpu_local::{CacheAligned, CpuLocal};
use slopos_ostd::sync::{ByteChain, IrqPreemptGuard, LockClassKey, RawLink, SpinLock};
use slopos_ostd::{klog_info, mm::Slab};

use super::magazine::{MAGAZINE_CAPACITY, Magazine};
use super::page::{SLAB_MAGIC, SlabHeader, alloc_slab_page, free_kernel_page, slab_page_count_inc};
use super::poison::{POISON_FREED, poison_object_body};
use crate::paging_defs::PAGE_SIZE_4KB;

/// Per-size-class slab state. Held under a `SpinLock` inside
/// [`SlabAllocator`]; the magazine fast path reads from per-CPU
/// magazines without touching this lock.
pub(crate) struct SlabClassState {
    /// Head of the partial-fill slab list. Full slabs are unlinked
    /// and re-linked on free; empty slabs (all objects free) stay
    /// linked too so they can satisfy future requests.
    pub(crate) slabs: RawLink<SlabHeader>,
}

impl SlabClassState {
    pub(crate) const fn new() -> Self {
        Self {
            slabs: RawLink::null(),
        }
    }
}

/// Per-class allocation/free counters. Atomic so the magazine fast
/// path can update them without taking the class lock.
pub(crate) struct SlabClassStats {
    pub(crate) alloc_count: AtomicU64,
    pub(crate) free_count: AtomicU64,
    pub(crate) total_objects: AtomicU32,
    pub(crate) free_objects: AtomicU32,
}

impl SlabClassStats {
    pub(crate) const fn new() -> Self {
        Self {
            alloc_count: AtomicU64::new(0),
            free_count: AtomicU64::new(0),
            total_objects: AtomicU32::new(0),
            free_objects: AtomicU32::new(0),
        }
    }
}

/// Per-size-class slab allocator.
///
/// `SIZE` is a const generic so each class is a distinct type, giving
/// `impl Slab` a fixed `Slot = NonNull<u8>` per class.
pub struct SlabAllocator<const SIZE: usize> {
    inner: SpinLock<SlabClassState>,
    magazines: CpuLocal<Magazine>,
    pub(crate) stats: SlabClassStats,
    /// Cached `class_idx` (0..=7). Stamped on every slab page header
    /// at creation time so `kfree` can route to the right class via
    /// a one-byte read.
    class_idx: u8,
}

impl<const SIZE: usize> SlabAllocator<SIZE> {
    /// Const constructor with the size-class index. Used only by
    /// [`super::KernelSlab::new_uninit`] which hardcodes the eight
    /// `(SIZE, class_idx, lock class)` triples.
    ///
    /// The lock class comes from the caller because a `lock_class!` minted
    /// here would be one class shared by all eight size classes — the key is
    /// its declaration site, and a generic function has one of those however
    /// many times it is instantiated. Merged, an inversion between two size
    /// classes could not be seen and their legitimate nesting could not be
    /// told from an error.
    pub(crate) const fn new_with_class(class_idx: u8, class: &'static LockClassKey) -> Self {
        const INIT: CacheAligned<Magazine> = CacheAligned(Magazine::new());
        Self {
            inner: SpinLock::new(SlabClassState::new(), class),
            magazines: CpuLocal::new_with([INIT; MAX_CPUS]),
            stats: SlabClassStats::new(),
            class_idx,
        }
    }

    /// Allocate one object. Magazine fast path first, then slab page
    /// list, then grow a fresh page from the buddy.
    pub(crate) fn alloc_one(&self) -> Option<NonNull<u8>> {
        // Magazine fast path: only when armed AND when the class lock
        // is not held by us (defends against any pathological re-entry).
        if super::HEAP_CACHES_ENABLED.load(Ordering::Acquire) && !self.inner.is_locked() {
            let _pin = IrqPreemptGuard::new();
            let cpu = get_current_cpu();
            let mag = self.magazines.get_pinned_mut(cpu);
            if let Some(ptr) = mag.pop() {
                self.stats.alloc_count.fetch_add(1, Ordering::Relaxed);
                self.stats.free_objects.fetch_sub(1, Ordering::Relaxed);
                return Some(ptr);
            }
            // Magazine empty — refill from the global pool.
            self.refill_magazine(mag);
            if let Some(ptr) = mag.pop() {
                self.stats.alloc_count.fetch_add(1, Ordering::Relaxed);
                self.stats.free_objects.fetch_sub(1, Ordering::Relaxed);
                return Some(ptr);
            }
        }

        // Slow path: pop directly from an existing slab page's free-list
        // under the class lock.
        if let Some(ptr) = self.with_class_locked(|state| self.pop_from_existing_slabs(state)) {
            self.stats.alloc_count.fetch_add(1, Ordering::Relaxed);
            self.stats.free_objects.fetch_sub(1, Ordering::Relaxed);
            return Some(ptr);
        }
        // Every existing slab is full. Build a fresh page WITHOUT holding
        // the class lock: `build_slab_page` -> buddy alloc may perform a
        // cross-CPU LUF/TLB drain that waits for peer IPI acks, and holding
        // the IRQ-off `SpinLock<SlabClassState>` across it deadlocks (a peer
        // spinning on this same lock can't service the ack IPI). Then link
        // it and pop from it under the lock.
        let new_slab = self.build_slab_page()?;
        let obj = self.with_class_locked(|state| {
            self.link_slab_at_head(state, new_slab);
            RawLink::<SlabHeader>::with_mut_at(Some(new_slab), |slab| {
                slab.free_count = slab.free_count.saturating_sub(1);
                slab.free_list.pop_front()
            })
            .flatten()
        })?;
        self.stats.alloc_count.fetch_add(1, Ordering::Relaxed);
        self.stats.free_objects.fetch_sub(1, Ordering::Relaxed);
        Some(obj)
    }

    /// Return an object. Magazine first, then slab page free-list.
    /// Double-frees are swallowed: if `ptr` is already cached in
    /// either the per-CPU magazine or the slab page's free chain, the
    /// dealloc is a no-op.
    pub(crate) fn dealloc_one(&self, ptr: NonNull<u8>) {
        if super::HEAP_CACHES_ENABLED.load(Ordering::Acquire) && !self.inner.is_locked() {
            let _pin = IrqPreemptGuard::new();
            let cpu = get_current_cpu();
            let mag = self.magazines.get_pinned_mut(cpu);
            // Defend against double-free: rejecting a duplicate now
            // prevents the magazine from later handing the same
            // pointer to two callers (silent use-after-free).
            if mag.contains(ptr) {
                return;
            }
            if mag.push(ptr) {
                self.stats.free_count.fetch_add(1, Ordering::Relaxed);
                self.stats.free_objects.fetch_add(1, Ordering::Relaxed);
                return;
            }
            // Magazine full — drain half into the global pool, then
            // retry the push.
            self.drain_magazine_half(mag);
            if mag.push(ptr) {
                self.stats.free_count.fetch_add(1, Ordering::Relaxed);
                self.stats.free_objects.fetch_add(1, Ordering::Relaxed);
                return;
            }
        }
        // Fallback path: directly insert into the owning slab page's
        // free-list under the class lock. `push_to_slab` walks the
        // chain and rejects duplicates, so double-frees that bypass
        // the magazine (caches disabled, or magazine fast path skipped
        // for any other reason) are still swallowed here.
        if self.with_class_locked(|state| self.push_to_slab(state, ptr)) {
            self.stats.free_count.fetch_add(1, Ordering::Relaxed);
            self.stats.free_objects.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Drain every per-CPU magazine for this class — push every
    /// cached object back into the global slab pool. Called from
    /// [`super::drain_all_heap_caches`].
    pub(crate) fn drain_magazines(&self) {
        self.magazines.for_each_mut_at_shutdown(|_cpu, mag| {
            while let Some(ptr) = mag.pop() {
                let _ = self.with_class_locked(|state| self.push_to_slab(state, ptr));
            }
        });
    }

    // ---- internals --------------------------------------------------

    /// Run `f` under the class lock with an unwind-abort guard armed.
    /// Every class-lock section splices intrusive free lists; an unwind
    /// mid-splice would release the lock around a torn chain, so it aborts
    /// instead. The guard is armed after the lock (the abort fires before
    /// the lock guard's release) and disarmed on the normal path, so a
    /// completed section never aborts even while a panic is unwinding
    /// elsewhere in the task.
    fn with_class_locked<R>(&self, f: impl FnOnce(&SlabClassState) -> R) -> R {
        let state = self.inner.lock();
        let abort_guard = AbortOnUnwind::new();
        let result = f(&state);
        abort_guard.disarm();
        result
    }

    fn refill_magazine(&self, mag: &mut Magazine) {
        let batch = MAGAZINE_CAPACITY / 2;
        // Best-effort: drain existing partial slabs only. If none are
        // available the magazine stays partial and the caller falls to the
        // slow path, which grows a fresh page outside this lock.
        self.with_class_locked(|state| {
            for _ in 0..batch {
                let Some(ptr) = self.pop_from_existing_slabs(state) else {
                    break;
                };
                if !mag.push(ptr) {
                    // Return the just-popped object back to the slab so
                    // the count stays consistent.
                    let _ = self.push_to_slab(state, ptr);
                    break;
                }
            }
        });
    }

    fn drain_magazine_half(&self, mag: &mut Magazine) {
        let target = mag.count() / 2;
        self.with_class_locked(|state| {
            for _ in 0..target {
                let Some(ptr) = mag.pop() else { break };
                let _ = self.push_to_slab(state, ptr);
            }
        });
    }

    /// Pop one object from an existing partial slab under the class lock.
    /// Returns `None` if every linked slab is full — the caller grows a
    /// fresh page OUTSIDE the lock (`build_slab_page`) and links it via
    /// `link_slab_at_head`, so the buddy allocation never runs under the
    /// IRQ-off class lock.
    fn pop_from_existing_slabs(&self, state: &SlabClassState) -> Option<NonNull<u8>> {
        let mut current = state.slabs.load();
        while let Some(slab_nn) = current {
            let outcome = RawLink::<SlabHeader>::with_mut_at(Some(slab_nn), |slab| {
                if slab.free_count == 0 {
                    return SlabVisit::Empty(slab.next.load());
                }
                let Some(obj) = slab.free_list.pop_front() else {
                    return SlabVisit::HeadlessFree;
                };
                // Sanity-check the new head before we commit.
                if let Some(next) = slab.free_list.head() {
                    let next_addr = next.as_ptr() as usize;
                    let slab_start = slab_nn.as_ptr() as usize;
                    let slab_end = slab_start + PAGE_SIZE_4KB as usize;
                    if next_addr < slab_start || next_addr >= slab_end {
                        klog_info!(
                            "slab<{}>: corrupt next ptr 0x{:x} in obj 0x{:x}, slab [0x{:x}..0x{:x}]",
                            SIZE,
                            next_addr,
                            obj.as_ptr() as usize,
                            slab_start,
                            slab_end
                        );
                        slab.free_list.set_head(None);
                        slab.free_count = 0;
                    }
                }
                slab.free_count = slab.free_count.saturating_sub(1);
                SlabVisit::Got(obj)
            });
            match outcome {
                Some(SlabVisit::Got(obj)) => return Some(obj),
                Some(SlabVisit::HeadlessFree) => return None,
                Some(SlabVisit::Empty(next)) => current = next,
                None => return None,
            }
        }
        // No partial slab — the caller grows one outside the class lock.
        None
    }

    /// Push `ptr` onto the owning slab page's free-list. Returns
    /// `true` if the page was located and the free completed, `false`
    /// if the pointer didn't belong to any tracked slab of this
    /// class (caller-side bug, swallowed).
    fn push_to_slab(&self, _state: &SlabClassState, ptr: NonNull<u8>) -> bool {
        let base_addr = (ptr.as_ptr() as u64) & !(PAGE_SIZE_4KB - 1);
        let Some(base) = NonNull::new(base_addr as *mut SlabHeader) else {
            return false;
        };
        let slab_start = base.as_ptr() as usize;
        let slab_end = slab_start + PAGE_SIZE_4KB as usize;
        let ptr_addr = ptr.as_ptr() as usize;

        let outcome = RawLink::<SlabHeader>::with_mut_at(Some(base), |slab| {
            if slab.magic != SLAB_MAGIC || slab.class_idx != self.class_idx {
                return false;
            }
            let object_size = slab.object_size as usize;
            let object_base = slab_start.saturating_add(SlabHeader::object_start_offset());
            if ptr_addr < object_base || ptr_addr >= slab_end {
                return false;
            }
            let offset = ptr_addr - object_base;
            if offset % object_size != 0 {
                return false;
            }
            // Walk the free chain to detect double-free / corruption.
            let mut current = slab.free_list.head();
            while let Some(curr) = current {
                let cur_addr = curr.as_ptr() as usize;
                if cur_addr < slab_start || cur_addr >= slab_end {
                    klog_info!(
                        "slab<{}>: corrupt free-list ptr 0x{:x} outside slab [0x{:x}..0x{:x}]",
                        SIZE,
                        cur_addr,
                        slab_start,
                        slab_end
                    );
                    break;
                }
                if cur_addr == ptr_addr {
                    return false;
                }
                current = ByteChain::read_next(curr);
            }
            // Optional poison.
            SlabHeader::with_body_slice_mut(ptr, object_size, |body| {
                poison_object_body(body, POISON_FREED)
            });
            slab.free_list.push_front(ptr);
            slab.free_count = slab.free_count.saturating_add(1);
            true
        });
        matches!(outcome, Some(true))
    }

    /// Build a fresh slab page WITHOUT taking the class lock. Allocates a
    /// backing page from the buddy (which may perform a cross-CPU LUF/TLB
    /// drain that waits for peer IPI acks — see `crate::mmu::luf`), stamps
    /// the header (magic, size, class_idx) and builds the in-page
    /// free-list. The returned slab is NOT yet linked into the class list;
    /// the caller links it under the lock via [`Self::link_slab_at_head`].
    /// Keeping the buddy allocation off the class lock is load-bearing:
    /// holding the IRQ-off `SpinLock<SlabClassState>` across the cross-CPU
    /// drain deadlocks (a peer spinning on this same lock can't service the
    /// ack IPI).
    fn build_slab_page(&self) -> Option<NonNull<SlabHeader>> {
        let (slab_base, _paddr) = alloc_slab_page()?;
        let start = SlabHeader::object_start_offset();
        if start >= PAGE_SIZE_4KB as usize {
            free_kernel_page(slab_base);
            return None;
        }
        let available = PAGE_SIZE_4KB as usize - start;
        let total_count = available / SIZE;
        if total_count == 0 {
            free_kernel_page(slab_base);
            return None;
        }

        // Build the in-page free-list (reverse so pop yields ascending
        // indices — the layout the test suite asserts on).
        let chain = ByteChain::new();
        for i in (0..total_count).rev() {
            if let Some(obj) = SlabHeader::object_at(slab_base, i, SIZE) {
                chain.push_front(obj);
            }
        }

        let header_nn = slab_base.cast::<SlabHeader>();
        RawLink::<SlabHeader>::with_mut_at(Some(header_nn), |h| {
            h.magic = SLAB_MAGIC;
            h.object_size = SIZE as u32;
            h.total_count = total_count as u16;
            h.free_count = total_count as u16;
            h.class_idx = self.class_idx;
            h._pad = [0; 3];
            // Not yet linked; `link_slab_at_head` stores the real `next`.
            h.next = RawLink::null();
            h.free_list = chain;
        });

        slab_page_count_inc();
        self.stats
            .total_objects
            .fetch_add(total_count as u32, Ordering::Relaxed);
        self.stats
            .free_objects
            .fetch_add(total_count as u32, Ordering::Relaxed);
        Some(header_nn)
    }

    /// Link an already-built slab page at the head of the class's partial
    /// list. Must be called under the class lock (`state` witnesses it).
    fn link_slab_at_head(&self, state: &SlabClassState, header: NonNull<SlabHeader>) {
        let prev_head = state.slabs.load();
        RawLink::<SlabHeader>::with_mut_at(Some(header), |h| {
            h.next.store(prev_head);
        });
        state.slabs.store(Some(header));
    }
}

impl<const SIZE: usize> Slab for SlabAllocator<SIZE> {
    type Slot = NonNull<u8>;

    fn alloc(&self) -> Option<Self::Slot> {
        self.alloc_one()
    }

    fn dealloc(&self, slot: Self::Slot) {
        self.dealloc_one(slot);
    }
}

enum SlabVisit {
    Got(NonNull<u8>),
    Empty(Option<NonNull<SlabHeader>>),
    HeadlessFree,
}
