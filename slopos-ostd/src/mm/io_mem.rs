//! `IoMem`: typed safe wrapper for memory-mapped I/O regions.
//!
//! Closes the `&T`-over-MMIO bug class by exposing device registers
//! exclusively through `Pod`-typed volatile read/write methods. There
//! is no path that yields a Rust reference into MMIO storage; the
//! `compile_fail` doctests on [`IoMem`] lock that discipline in.
//!
//! # Construction
//!
//! `IoMem` is built through one of two sanctioned paths:
//!
//! 1. [`IoMemRegistry::reserve`] — the gated path. Checks containment
//!    against a `&'static [PhysRange]` of architecturally-fixed
//!    insensitive ranges (Inv. 7), plus the heap-free secondary
//!    [dynamic registry](register_io_mem_range) for ranges discovered
//!    at boot (HPET / IOAPIC / PCI ECAM / device BARs / framebuffer),
//!    and delegates virt-allocation + page-table mapping to a
//!    registered [`IoMemMapper`]. The mapper and static-range
//!    registrations are one-shot, mirroring the pattern in
//!    [`crate::mm::frame_alloc`] and [`crate::mm::phys`]. The dynamic
//!    registry is append-only single-writer; in SlopOS this is
//!    automatic because driver init runs serially on the BSP.
//!
//! 2. [`IoMem::empty`] — a const placeholder constructor producing a
//!    zero-sized handle (`virt_base = 0`, `phys_base = NULL`,
//!    `size = 0`). Reads / writes against an empty handle OOB-panic;
//!    the placeholder exists so callers can park an `IoMem` in a
//!    `const fn` constructor or a `[T; N]` array slot before the real
//!    region is reserved.
//!
//! # Cache policy
//!
//! [`IoMemCachePolicy`] selects between Uncacheable (the safe default
//! for device registers), Write-Combining (framebuffers, video
//! memory), Write-Through, and Write-Back. The mapping translates
//! these to platform-specific PAT bits inside the [`IoMemMapper`].
//!
//! # Lifetime
//!
//! `IoMem` is `Clone` but neither `Copy` nor `Drop`. Cloning produces
//! a second handle pointing into the same mapping; the kernel virtual
//! window stays mapped for the kernel's lifetime. Real unmap on Drop
//! is a Phase-2 concern and ships when the kernel virtual allocator
//! grows recyclable ranges.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{align_of, size_of};
use core::sync::atomic::{AtomicPtr, AtomicUsize, Ordering};

use slopos_abi::addr::PhysAddr;

use crate::mm::pod::Pod;

// ---------------------------------------------------------------------------
// PhysRange.
// ---------------------------------------------------------------------------

/// Half-open physical address range `[base, base + len)`.
///
/// Used by [`IoMemRegistry`] to describe the set of MMIO ranges the
/// firmware has certified as insensitive (free for OSTD clients to
/// access).
#[derive(Clone, Copy, Debug)]
pub struct PhysRange {
    pub base: PhysAddr,
    pub len: usize,
}

impl PhysRange {
    /// True if `[base, base + len)` is entirely contained within
    /// `self`. Overflow-safe: `base + len` is computed via
    /// `checked_add`; any overflow returns false.
    #[inline]
    pub fn contains_range(&self, base: PhysAddr, len: usize) -> bool {
        let req_start = base.as_u64();
        let Some(req_end) = req_start.checked_add(len as u64) else {
            return false;
        };
        let self_start = self.base.as_u64();
        let Some(self_end) = self_start.checked_add(self.len as u64) else {
            return false;
        };
        req_start >= self_start && req_end <= self_end
    }
}

// ---------------------------------------------------------------------------
// Cache policy.
// ---------------------------------------------------------------------------

/// Per-region caching attribute. Maps to platform-specific PAT bits
/// inside the [`IoMemMapper`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoMemCachePolicy {
    /// Strongly uncacheable. Safe default for device registers.
    Uncacheable,
    /// Write-combining. Framebuffers, video memory, large bulk MMIO.
    WriteCombining,
    /// Write-through. Reads cached, writes propagate.
    WriteThrough,
    /// Write-back. Used by RAM-backed I/O windows (rare).
    WriteBack,
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

/// Failure modes for `IoMem` operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IoMemError {
    /// `offset + size_of::<T>()` exceeds the region.
    OutOfBounds,
    /// `(virt_base + offset) % align_of::<T>() != 0`.
    Misaligned,
    /// `IoMemRegistry::reserve` could not find a containing range.
    NotReserved,
    /// The registered [`IoMemMapper`] failed to install the mapping.
    MappingFailed,
    /// One or both of [`IoMemRegistry`] / [`IoMemMapper`] have not
    /// been registered yet.
    Uninitialised,
}

// ---------------------------------------------------------------------------
// IoMemMapper trait + registration.
// ---------------------------------------------------------------------------

/// Pluggable mapper that owns kernel virtual address allocation and
/// page-table installation for `IoMem`. `slopos-ostd` cannot call the
/// kernel's paging layer directly (the dependency arrow points the
/// other way), so the legacy / production mapper lives outside this
/// crate and registers itself here at boot.
pub trait IoMemMapper: Send + Sync + 'static {
    /// Allocate a kernel virtual window covering `[phys, phys + size)`
    /// and install page-table entries with the requested cache policy.
    /// Returns the starting virtual address.
    fn map(&self, phys: PhysAddr, size: usize, policy: IoMemCachePolicy)
    -> Result<u64, IoMemError>;

    /// Tear down a mapping previously returned by [`Self::map`]. Not
    /// invoked by the current `IoMem` (mappings are leaked); declared
    /// so a future recyclable-virt allocator does not have to widen
    /// the trait surface.
    fn unmap(&self, virt: u64, size: usize);
}

struct MapperSlot {
    inner: AtomicPtr<()>,
}

static IO_MEM_MAPPER: MapperSlot = MapperSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point for the kernel's [`IoMemMapper`].
///
/// # Safety
///
/// The caller certifies that `slot` outlives the kernel (`'static`)
/// and that the underlying `dyn IoMemMapper` is sound for concurrent
/// `map` / `unmap` from any CPU.
pub unsafe fn register_io_mem_mapper(slot: &'static &'static dyn IoMemMapper) {
    let raw = slot as *const &'static dyn IoMemMapper as *mut ();
    let prev = IO_MEM_MAPPER.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::io_mem::register_io_mem_mapper called twice"
    );
}

fn current_io_mem_mapper() -> Option<&'static dyn IoMemMapper> {
    let raw = IO_MEM_MAPPER.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: Inv. 7. `raw` was produced by `register_io_mem_mapper`
    // from a `&'static &'static dyn IoMemMapper`; that storage is
    // `'static` by contract, so the dereference is sound.
    let slot = unsafe { &*(raw as *const &'static dyn IoMemMapper) };
    Some(*slot)
}

// ---------------------------------------------------------------------------
// IoMemRegistry: insensitive-range list + registration.
// ---------------------------------------------------------------------------

struct RegistrySlot {
    base: AtomicPtr<PhysRange>,
    len: AtomicUsize,
}

static IO_MEM_REGISTRY: RegistrySlot = RegistrySlot {
    base: AtomicPtr::new(core::ptr::null_mut()),
    len: AtomicUsize::new(0),
};

/// One-shot wiring point for the insensitive-range list. Boot
/// constructs the slice from ACPI MCFG (PCIe ECAM), MADT (LAPIC,
/// IOAPIC), HPET, and the Limine framebuffer response, then installs
/// it via this hook. The slice is immutable for the kernel's lifetime
/// — hot-plug is not supported.
///
/// # Safety
///
/// The caller certifies that `ranges` lives for the static lifetime of
/// the kernel and that every entry describes a region the firmware /
/// platform has marked as insensitive (Inv. 7).
pub unsafe fn register_io_mem_registry(ranges: &'static [PhysRange]) {
    let raw = ranges.as_ptr() as *mut PhysRange;
    let prev = IO_MEM_REGISTRY.base.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::io_mem::register_io_mem_registry called twice"
    );
    IO_MEM_REGISTRY.len.store(ranges.len(), Ordering::Release);
}

fn current_io_mem_registry() -> Option<&'static [PhysRange]> {
    let base = IO_MEM_REGISTRY.base.load(Ordering::Acquire);
    if base.is_null() {
        return None;
    }
    let len = IO_MEM_REGISTRY.len.load(Ordering::Acquire);
    // SAFETY: Inv. 7. `base` was produced by `register_io_mem_registry`
    // from a `&'static [PhysRange]` of length `len`; the slice is
    // `'static` and immutable.
    Some(unsafe { core::slice::from_raw_parts(base, len) })
}

// ---------------------------------------------------------------------------
// Dynamic-range secondary registry.
//
// The static slice registered above carries architecturally-fixed
// MMIO (the LAPIC at 0xFEE0_0000 on x86_64). HPET, IOAPIC, PCI ECAM,
// device BARs, and the framebuffer have firmware-discovered phys
// addresses that aren't known until boot is well underway — far too
// late to compose into a `&'static [PhysRange]` for the one-shot
// hook above.
//
// This append-only fixed-size table backs [`register_io_mem_range`].
// It is single-writer-multi-reader: the writer (`register_…_range`)
// stores the new entry first and publishes the count via a release
// store; readers (`IoMemRegistry::reserve`) acquire-load the count
// and then read up to that many `PhysRange` slots. PhysRange is
// `Copy`, so torn reads are impossible — each slot is written before
// the count increment that makes it visible.
//
// **Single-writer precondition.** Two CPUs concurrently calling
// `register_io_mem_range` would race on slot allocation. SlopOS boot
// runs all driver init serially on the BSP before SMP fully settles,
// so the contract is satisfied automatically. Late callers (post-boot
// hot-plug) would have to serialise externally.
// ---------------------------------------------------------------------------

const MAX_DYNAMIC_RANGES: usize = 64;

#[repr(transparent)]
struct DynamicSlot(UnsafeCell<PhysRange>);

// SAFETY: writer is single-threaded by API contract; readers only
// touch slots whose count has been published (release-acquire fence).
unsafe impl Sync for DynamicSlot {}

const EMPTY_SLOT: DynamicSlot = DynamicSlot(UnsafeCell::new(PhysRange {
    base: PhysAddr::NULL,
    len: 0,
}));

static DYNAMIC_RANGES: [DynamicSlot; MAX_DYNAMIC_RANGES] =
    [const { EMPTY_SLOT }; MAX_DYNAMIC_RANGES];
static DYNAMIC_COUNT: AtomicUsize = AtomicUsize::new(0);

/// Append a runtime-discovered insensitive range to the dynamic
/// secondary registry. Used by drivers that map MMIO from
/// firmware-discovered phys addresses (HPET, IOAPIC, PCI ECAM,
/// device BARs, framebuffer): the kernel-side
/// `slopos_mm::mmio::MmioRegionExt::map` calls this implicitly before
/// reaching [`IoMemRegistry::reserve`] so drivers don't need to know
/// the registry exists.
///
/// Append-only: there is no deregistration path (matches the static
/// registry's "no hot-plug" stance). Idempotent at the caller's
/// discretion — re-registering the same range is harmless because
/// `reserve` only checks containment, not exact match.
///
/// Returns:
/// - `Err(OutOfBounds)` for a zero-length range.
/// - `Err(MappingFailed)` once the table is full
///   (`MAX_DYNAMIC_RANGES = 64` ranges; raise the constant if a
///   platform genuinely needs more).
///
/// # Single-writer precondition
///
/// Callers must serialise with respect to one another. SlopOS driver
/// init satisfies this automatically (all calls run on the BSP during
/// the boot phase priority sequence). Concurrent callers would race
/// on slot allocation and one of the writes would be lost.
pub fn register_io_mem_range(range: PhysRange) -> Result<(), IoMemError> {
    if range.len == 0 {
        return Err(IoMemError::OutOfBounds);
    }
    let slot = DYNAMIC_COUNT.load(Ordering::Relaxed);
    if slot >= MAX_DYNAMIC_RANGES {
        return Err(IoMemError::MappingFailed);
    }
    // SAFETY: single-writer contract — only this CPU may touch
    // `DYNAMIC_RANGES[slot]` until the release-store of `slot + 1`
    // below makes the entry visible to readers.
    unsafe {
        *DYNAMIC_RANGES[slot].0.get() = range;
    }
    DYNAMIC_COUNT.store(slot + 1, Ordering::Release);
    Ok(())
}

fn dynamic_ranges_view() -> &'static [PhysRange] {
    let count = DYNAMIC_COUNT.load(Ordering::Acquire);
    if count == 0 {
        return &[];
    }
    // SAFETY: `DynamicSlot` is `#[repr(transparent)]` over
    // `UnsafeCell<PhysRange>`; `UnsafeCell<T>` is in turn
    // `repr(transparent)` over `T`, so the array layout is identical
    // to `[PhysRange; N]`. The release-acquire pair guarantees the
    // first `count` slots have valid `PhysRange` writes; `PhysRange`
    // is `Copy` so concurrent readers cannot tear the read.
    unsafe { core::slice::from_raw_parts(DYNAMIC_RANGES.as_ptr() as *const PhysRange, count) }
}

/// Test-only reset hook. Clears the mapper, both registries, and the
/// dynamic-range table so host integration tests can install a fresh
/// wiring per binary.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    IO_MEM_MAPPER
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
    IO_MEM_REGISTRY
        .base
        .store(core::ptr::null_mut(), Ordering::Release);
    IO_MEM_REGISTRY.len.store(0, Ordering::Release);
    DYNAMIC_COUNT.store(0, Ordering::Release);
}

/// Insensitive-range gate over [`IoMem`] construction. Stateless;
/// every method is associated.
pub struct IoMemRegistry;

impl IoMemRegistry {
    /// Reserve `[phys, phys + size)` as an `IoMem` with the requested
    /// cache policy.
    ///
    /// Returns:
    /// - `Err(Uninitialised)` if either the registry list or the
    ///   mapper has not been registered.
    /// - `Err(NotReserved)` if no insensitive range contains the
    ///   request.
    /// - `Err(MappingFailed)` if the mapper rejects the request (e.g.
    ///   kernel virtual address space exhausted).
    pub fn reserve(
        phys: PhysAddr,
        size: usize,
        policy: IoMemCachePolicy,
    ) -> Result<IoMem, IoMemError> {
        if size == 0 {
            return Err(IoMemError::OutOfBounds);
        }
        let ranges = current_io_mem_registry().ok_or(IoMemError::Uninitialised)?;
        let mapper = current_io_mem_mapper().ok_or(IoMemError::Uninitialised)?;
        let in_static = ranges.iter().any(|r| r.contains_range(phys, size));
        let contained = in_static
            || dynamic_ranges_view()
                .iter()
                .any(|r| r.contains_range(phys, size));
        if !contained {
            return Err(IoMemError::NotReserved);
        }
        let virt_base = mapper.map(phys, size, policy)?;
        Ok(IoMem {
            virt_base,
            phys_base: phys,
            size,
            _not_send_pinned: PhantomData,
        })
    }
}

// ---------------------------------------------------------------------------
// IoMem.
// ---------------------------------------------------------------------------

/// Typed handle to a memory-mapped I/O region.
///
/// All access goes through volatile [`Pod`] reads/writes; there is no
/// path that hands out `&T` or `&[u8]` over MMIO storage.
///
/// ## No-reference discipline (compile-fail doctests)
///
/// `IoMem` deliberately does **not** implement `Deref`,
/// `Index<Range<usize>>`, or expose `as_slice`. Each of the following
/// must fail to compile; if any starts passing, a soundness invariant
/// has broken.
///
/// `Deref`:
/// ```compile_fail
/// use core::ops::Deref;
/// use slopos_ostd::mm::io_mem::IoMem;
/// let m: IoMem = unimplemented!();
/// let _ = m.deref();
/// ```
///
/// `Index<Range<usize>>` / `&iomem[..]`:
/// ```compile_fail
/// use slopos_ostd::mm::io_mem::IoMem;
/// let m: IoMem = unimplemented!();
/// let _: &[u8] = &m[0..4];
/// ```
///
/// `as_slice`:
/// ```compile_fail
/// use slopos_ostd::mm::io_mem::IoMem;
/// let m: IoMem = unimplemented!();
/// let _: &[u8] = m.as_slice();
/// ```
#[derive(Debug)]
pub struct IoMem {
    virt_base: u64,
    phys_base: PhysAddr,
    size: usize,
    /// `PhantomData<()>` placeholder. Reserved so Phase-2 can attach a
    /// lifetime / ref-count without breaking the `IoMem` constructor
    /// shape.
    _not_send_pinned: PhantomData<()>,
}

// SAFETY: Inv. 7. `IoMem` carries only a virt base + phys base + size;
// the mapping it points into is shared (Clone produces aliases) and
// the underlying device storage is responsible for its own
// concurrency. Sharing across threads is sound — multiple readers /
// writers of MMIO are a driver-side concern.
unsafe impl Send for IoMem {}
unsafe impl Sync for IoMem {}

impl Clone for IoMem {
    #[inline]
    fn clone(&self) -> Self {
        Self {
            virt_base: self.virt_base,
            phys_base: self.phys_base,
            size: self.size,
            _not_send_pinned: PhantomData,
        }
    }
}

impl IoMem {
    /// Const placeholder constructor. Returns a zero-sized handle
    /// (`virt_base = 0`, `phys_base = NULL`, `size = 0`) suitable for
    /// `static`/`const fn` initialisers and `[T; N]` array slots that
    /// will be overwritten with a real reservation later. Reads /
    /// writes against the result OOB-panic; `is_mapped()` returns
    /// `false`. See module docs for the two sanctioned construction
    /// paths.
    #[inline]
    pub const fn empty() -> Self {
        Self {
            virt_base: 0,
            phys_base: PhysAddr::NULL,
            size: 0,
            _not_send_pinned: PhantomData,
        }
    }

    /// Physical base address of the region.
    #[inline]
    pub fn phys_base(&self) -> PhysAddr {
        self.phys_base
    }

    /// Kernel virtual base address the mapper installed for this
    /// region. Mostly useful for legacy MMIO call-sites that store the
    /// address in an `AtomicU64` for lock-free hot-path reads;
    /// dereferencing via raw pointer is `unsafe` and bypasses the
    /// `read` / `write` bounds + alignment checks. Prefer the volatile
    /// accessors for normal driver code.
    #[inline]
    pub fn virt_base(&self) -> u64 {
        self.virt_base
    }

    /// Region size in bytes.
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// True for handles produced by [`IoMemRegistry::reserve`]; false
    /// for handles produced by [`Self::empty`] or any other zero-sized
    /// placeholder.
    #[inline]
    pub fn is_mapped(&self) -> bool {
        self.size != 0
    }

    /// True if `offset + access_size` lies within the region.
    #[inline]
    pub fn is_valid_offset(&self, offset: usize, access_size: usize) -> bool {
        offset
            .checked_add(access_size)
            .is_some_and(|end| end <= self.size)
    }

    /// Read a `Pod` value at `offset`. Panics on out-of-bounds or
    /// misaligned access — driver-side miscoding is unrecoverable. Use
    /// [`Self::try_read`] for a fallible variant.
    #[inline]
    pub fn read<T: Pod>(&self, offset: usize) -> T {
        let size = size_of::<T>();
        let end = offset
            .checked_add(size)
            .expect("IoMem::read offset overflow");
        assert!(
            end <= self.size,
            "IoMem::read out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );
        let addr = self.virt_base.wrapping_add(offset as u64);
        assert!(
            addr as usize % align_of::<T>() == 0,
            "IoMem::read misaligned: virt={:#x}, align={}",
            addr,
            align_of::<T>()
        );
        // SAFETY: Inv. 7. The region was certified insensitive by
        // `IoMemRegistry::reserve` and the mapping for `[virt_base,
        // virt_base + size)` was installed by the registered
        // `IoMemMapper`; bounds + alignment were just checked, so
        // `read_volatile::<T>` reads a fully-mapped, suitably-aligned
        // address. `T: Pod` makes every byte pattern a valid `T`.
        unsafe { core::ptr::read_volatile(addr as *const T) }
    }

    /// Write a `Pod` value at `offset`. Panics on out-of-bounds or
    /// misaligned access. Use [`Self::try_write`] for a fallible
    /// variant.
    #[inline]
    pub fn write<T: Pod>(&self, offset: usize, value: T) {
        let size = size_of::<T>();
        let end = offset
            .checked_add(size)
            .expect("IoMem::write offset overflow");
        assert!(
            end <= self.size,
            "IoMem::write out of bounds: offset={}, size={}, region_size={}",
            offset,
            size,
            self.size
        );
        let addr = self.virt_base.wrapping_add(offset as u64);
        assert!(
            addr as usize % align_of::<T>() == 0,
            "IoMem::write misaligned: virt={:#x}, align={}",
            addr,
            align_of::<T>()
        );
        // SAFETY: Inv. 7. Same justification as `read`: the region is
        // certified insensitive, the mapping covers the address, and
        // `T: Pod` permits arbitrary byte writes.
        unsafe { core::ptr::write_volatile(addr as *mut T, value) }
    }

    /// Fallible variant of [`Self::read`]. Returns
    /// `Err(OutOfBounds)` / `Err(Misaligned)` instead of panicking.
    #[inline]
    pub fn try_read<T: Pod>(&self, offset: usize) -> Result<T, IoMemError> {
        let size = size_of::<T>();
        let end = offset.checked_add(size).ok_or(IoMemError::OutOfBounds)?;
        if end > self.size {
            return Err(IoMemError::OutOfBounds);
        }
        let addr = self.virt_base.wrapping_add(offset as u64);
        if addr as usize % align_of::<T>() != 0 {
            return Err(IoMemError::Misaligned);
        }
        // SAFETY: Inv. 7. As `read`, with bounds + alignment proven
        // by the checks above.
        Ok(unsafe { core::ptr::read_volatile(addr as *const T) })
    }

    /// Fallible variant of [`Self::write`].
    #[inline]
    pub fn try_write<T: Pod>(&self, offset: usize, value: T) -> Result<(), IoMemError> {
        let size = size_of::<T>();
        let end = offset.checked_add(size).ok_or(IoMemError::OutOfBounds)?;
        if end > self.size {
            return Err(IoMemError::OutOfBounds);
        }
        let addr = self.virt_base.wrapping_add(offset as u64);
        if addr as usize % align_of::<T>() != 0 {
            return Err(IoMemError::Misaligned);
        }
        // SAFETY: Inv. 7. As `write`, with bounds + alignment proven
        // by the checks above.
        unsafe { core::ptr::write_volatile(addr as *mut T, value) };
        Ok(())
    }

    /// Carve a sub-region out of this region. The returned handle
    /// shares the parent's mapping; `phys_base` is offset by `offset`
    /// and `size` shrinks accordingly. Returns `None` on overrun.
    pub fn sub_region(&self, offset: usize, size: usize) -> Option<IoMem> {
        let end = offset.checked_add(size)?;
        if end > self.size {
            return None;
        }
        let phys_off = self.phys_base.as_u64().checked_add(offset as u64)?;
        let virt_off = self.virt_base.checked_add(offset as u64)?;
        Some(IoMem {
            virt_base: virt_off,
            phys_base: PhysAddr::new(phys_off),
            size,
            _not_send_pinned: PhantomData,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests (host-side, pure logic).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phys_range_contains_simple() {
        let r = PhysRange {
            base: PhysAddr::new(0x1000),
            len: 0x1000,
        };
        assert!(r.contains_range(PhysAddr::new(0x1000), 0x1000));
        assert!(r.contains_range(PhysAddr::new(0x1100), 0x800));
        assert!(!r.contains_range(PhysAddr::new(0x0fff), 0x10));
        assert!(!r.contains_range(PhysAddr::new(0x1000), 0x1001));
    }

    #[test]
    fn phys_range_contains_handles_overflow() {
        let r = PhysRange {
            base: PhysAddr::new(0),
            len: usize::MAX,
        };
        // PhysAddr::MAX + len(=usize::MAX) overflows -> false.
        assert!(!r.contains_range(PhysAddr::MAX, usize::MAX));
    }

    #[test]
    fn io_mem_error_is_eq() {
        assert_eq!(IoMemError::OutOfBounds, IoMemError::OutOfBounds);
        assert_ne!(IoMemError::OutOfBounds, IoMemError::Misaligned);
    }

    #[test]
    fn io_mem_cache_policy_is_eq() {
        assert_eq!(IoMemCachePolicy::Uncacheable, IoMemCachePolicy::Uncacheable);
        assert_ne!(
            IoMemCachePolicy::Uncacheable,
            IoMemCachePolicy::WriteCombining
        );
    }

    #[test]
    fn io_mem_is_clone() {
        // Construct via the crate-private path: reach in from the
        // module's own test mod (allowed because we're inside it).
        let m = IoMem {
            virt_base: 0xffff_8000_dead_0000,
            phys_base: PhysAddr::new(0xfee0_0000),
            size: 0x1000,
            _not_send_pinned: PhantomData,
        };
        let n = m.clone();
        assert_eq!(n.virt_base, m.virt_base);
        assert_eq!(n.phys_base.as_u64(), m.phys_base.as_u64());
        assert_eq!(n.size, m.size);
    }

    #[test]
    fn io_mem_sub_region_offsets() {
        let m = IoMem {
            virt_base: 0xffff_8000_0000_2000,
            phys_base: PhysAddr::new(0xfee0_2000),
            size: 0x1000,
            _not_send_pinned: PhantomData,
        };
        let s = m.sub_region(0x100, 0x200).unwrap();
        assert_eq!(s.virt_base, 0xffff_8000_0000_2100);
        assert_eq!(s.phys_base.as_u64(), 0xfee0_2100);
        assert_eq!(s.size, 0x200);
        assert!(m.sub_region(0x900, 0x800).is_none());
    }
}
