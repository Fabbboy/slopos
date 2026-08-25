//! `DmaCoherent` / `DmaStream`: typed handles for IOMMU-mapped
//! device-visible memory.
//!
//! `DmaCoherent` is for buffers the device polls without explicit cache
//! management (descriptor rings, doorbell pages); `DmaStream` carries an
//! explicit [`DmaDirection`] plus a `sync_for_device` / `sync_for_cpu` API
//! that exists for future ARM64.
//!
//! All device-visible mappings flow through the registered [`IommuMapper`],
//! and with none registered every allocation fails with
//! [`DmaError::NotInitialised`]. That deny is type-level only: programming
//! the hardware IOMMU is the mapper's own responsibility, so a device the
//! bootloader left with an open DMA window keeps it until the mapper closes
//! it.
//!
//! Drop unmaps and releases the frames but issues **no DMA fence**: drivers
//! must quiesce the device before dropping a handle, or the unmap races
//! in-flight DMA and corrupts unrelated kernel memory.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::addr::PhysAddr;

use crate::mm::frame::{AnyFrameMeta, FrameAllocOptions};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::pod::Pod;
use crate::mm::uframe::{AnyUFrameMeta, UFrameError, USegment};
use crate::sync::BspToken;

/// Direction of a streaming DMA mapping. `DmaCoherent` is implicitly
/// bidirectional and so does not carry a direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaDirection {
    /// CPU writes, device reads.
    ToDevice,
    /// Device writes, CPU reads.
    FromDevice,
    /// Both directions; cache-flush semantics on each transition.
    Bidirectional,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    /// [`register_iommu_mapper`] has not been called yet.
    NotInitialised,
    /// Frame or page-table allocator is out of memory.
    Exhausted,
    /// IOMMU policy refuses this physical range (e.g. it covers
    /// kernel-sensitive memory).
    Forbidden,
    /// Mapper failed for another reason (page-walk failure, etc.).
    MappingFailed,
}

impl From<UFrameError> for DmaError {
    fn from(e: UFrameError) -> Self {
        match e {
            UFrameError::OutOfMemory => DmaError::Exhausted,
            // Misalignment / OOB / Truncated cannot occur at alloc time; the
            // byte-copy methods surface `UFrameError` directly instead.
            _ => DmaError::MappingFailed,
        }
    }
}

/// Per-frame metadata for `DmaCoherent` pages: a distinct ZST so
/// [`USegment`]'s `M` tracks coherent vs. streaming in the type system.
#[derive(Debug, Default)]
pub struct DmaCoherentMeta;

// SAFETY: ZST has no representation invariants. `returns_frame_on_last_drop`
// is `false` because a run's pages are owned by its `DmaRun`: the last drop
// resets the slot and `RunRelease` returns the pages.
unsafe impl AnyFrameMeta for DmaCoherentMeta {
    fn returns_frame_on_last_drop(&self) -> bool {
        false
    }
}

// SAFETY: DMA pages are peripheral-tampered, so only the byte-copy access
// `AnyUFrameMeta` permits (no `&T`) is sound.
unsafe impl AnyUFrameMeta for DmaCoherentMeta {}

#[derive(Debug, Default)]
pub struct DmaStreamMeta;

// SAFETY: as `DmaCoherentMeta` — the run owns the pages, so the per-frame
// lifecycle does not return them to the allocator.
unsafe impl AnyFrameMeta for DmaStreamMeta {
    fn returns_frame_on_last_drop(&self) -> bool {
        false
    }
}

// SAFETY: as `DmaCoherentMeta`.
unsafe impl AnyUFrameMeta for DmaStreamMeta {}

/// Pluggable IOMMU mapper; exactly one is registered per kernel lifetime.
pub trait IommuMapper: Send + Sync + 'static {
    /// Map `[phys, phys + size)` as a device-visible IOVA in the
    /// requested direction. Returns the IOVA base.
    fn map(&self, phys: PhysAddr, size: usize, direction: DmaDirection) -> Result<u64, DmaError>;

    /// Tear down a mapping previously returned by [`Self::map`].
    fn unmap(&self, iova: u64, size: usize);
}

struct MapperSlot {
    inner: AtomicPtr<()>,
}

static IOMMU_MAPPER: MapperSlot = MapperSlot {
    inner: AtomicPtr::new(core::ptr::null_mut()),
};

/// One-shot wiring point for the kernel's [`IommuMapper`]; the mapper must
/// be sound for concurrent `map` / `unmap` from any CPU.
pub fn register_iommu_mapper<'brand>(
    _token: &BspToken<'brand>,
    slot: &'static &'static dyn IommuMapper,
) {
    let raw = slot as *const &'static dyn IommuMapper as *mut ();
    let prev = IOMMU_MAPPER.inner.swap(raw, Ordering::AcqRel);
    assert!(
        prev.is_null(),
        "slopos_ostd::mm::dma::register_iommu_mapper called twice"
    );
}

fn current_iommu_mapper() -> Option<&'static dyn IommuMapper> {
    #[cfg(any(test, feature = "test-helpers"))]
    if HIDDEN_FROM_CPU.load(Ordering::Acquire) == crate::cpu::x86_64::pcr::get_current_cpu() {
        return None;
    }
    let raw = IOMMU_MAPPER.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: Inv. 6. `raw` came from `register_iommu_mapper`'s
    // `&'static &'static dyn IommuMapper`, so the storage is `'static`.
    let slot = unsafe { &*(raw as *const &'static dyn IommuMapper) };
    Some(*slot)
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    IOMMU_MAPPER
        .inner
        .store(core::ptr::null_mut(), Ordering::Release);
}

#[cfg(any(test, feature = "test-helpers"))]
static HIDDEN_FROM_CPU: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(usize::MAX);

/// Hides the registered mapper from the calling CPU alone, so a concurrent
/// driver allocation on another CPU is not denied over a test's shoulder.
#[cfg(any(test, feature = "test-helpers"))]
#[must_use = "the mapper reappears when the guard drops"]
pub struct MapperHiddenForTest {
    _pinned: crate::cpu::preempt::DisabledPreemptGuard,
}

#[cfg(any(test, feature = "test-helpers"))]
impl MapperHiddenForTest {
    pub fn for_current_cpu() -> Self {
        let pinned = crate::cpu::preempt::DisabledPreemptGuard::new();
        HIDDEN_FROM_CPU.store(
            crate::cpu::x86_64::pcr::get_current_cpu(),
            Ordering::Release,
        );
        Self { _pinned: pinned }
    }
}

#[cfg(any(test, feature = "test-helpers"))]
impl Drop for MapperHiddenForTest {
    fn drop(&mut self) {
        HIDDEN_FROM_CPU.store(usize::MAX, Ordering::Release);
    }
}

/// Passthrough mapper: IOVA == physical address, unmap is a no-op. Turns the
/// no-mapper hard deny into a passthrough policy on platforms with no IOMMU
/// to program (e.g. QEMU `q35` without VT-d).
struct IdentityMapper;

impl IommuMapper for IdentityMapper {
    fn map(&self, phys: PhysAddr, _size: usize, _direction: DmaDirection) -> Result<u64, DmaError> {
        Ok(phys.as_u64())
    }

    fn unmap(&self, _iova: u64, _size: usize) {}
}

static IDENTITY: IdentityMapper = IdentityMapper;
static IDENTITY_SLOT: &dyn IommuMapper = &IDENTITY;

/// Wire the passthrough [`IdentityMapper`] as the kernel's IOMMU mapper.
pub fn register_identity_dma_mapper<'brand>(token: &BspToken<'brand>) {
    register_iommu_mapper(token, &IDENTITY_SLOT);
}

/// Test-only identity-mapper registration that bypasses the `BspToken` and
/// the double-register assert, restoring the global mapper after
/// [`reset_for_test`] without re-minting the one-shot BSP token.
#[cfg(any(test, feature = "test-helpers"))]
pub fn register_identity_dma_mapper_for_test() {
    let raw = &IDENTITY_SLOT as *const &'static dyn IommuMapper as *mut ();
    IOMMU_MAPPER.inner.store(raw, Ordering::Release);
}

/// Returns a contiguous run of physical pages to the registered
/// [`FrameAlloc`](crate::mm::frame::FrameAlloc) on drop.
///
/// One `dealloc` at the head covers the whole run: the allocator recovers the
/// extent from its own descriptor, so `len_pages` is not the authority on how
/// much comes back.
struct RunRelease {
    head: PhysAddr,
    len_pages: usize,
}

impl Drop for RunRelease {
    fn drop(&mut self) {
        if let Some(allocator) = current_frame_allocator() {
            allocator.dealloc(self.head, self.len_pages);
            #[cfg(any(test, feature = "test-helpers"))]
            PAGES_RETURNED.fetch_add(self.len_pages as u64, Ordering::Relaxed);
        }
    }
}

#[cfg(any(test, feature = "test-helpers"))]
static PAGES_TAKEN: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
#[cfg(any(test, feature = "test-helpers"))]
static PAGES_RETURNED: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// `(taken, returned)` since boot, in pages. Both monotonic, so a test compares
/// deltas across its own window.
#[cfg(any(test, feature = "test-helpers"))]
pub fn run_page_account() -> (u64, u64) {
    (
        PAGES_TAKEN.load(Ordering::Relaxed),
        PAGES_RETURNED.load(Ordering::Relaxed),
    )
}

/// A contiguous run of DMA pages together with the release that hands them
/// back to the frame allocator.
///
/// [`DmaCoherentMeta`] and [`DmaStreamMeta`] declare
/// `returns_frame_on_last_drop() == false`, so the per-frame lifecycle resets
/// each `MetaSlot` but leaves the pages allocated; this type owns the
/// compensating `dealloc`.
///
/// **The field order is load-bearing.** Fields drop in declaration order, so
/// every `MetaSlot` reset completes before `release` makes the pages
/// claimable again — a free-listed paddr must read UNUSED, or the next
/// claimant's `from_unused` races a slot that is still TYPED.
struct DmaRun<M: AnyUFrameMeta> {
    segment: USegment<M>,
    #[allow(dead_code)]
    release: RunRelease,
}

impl<M: AnyUFrameMeta> DmaRun<M> {
    /// Allocate `npages` contiguous, zeroed pages and install `M` on each.
    ///
    /// The release is armed the instant the allocator hands over the run and
    /// is the last local dropped, so every error path from there on returns
    /// the pages.
    fn alloc(npages: usize) -> Result<Self, DmaError> {
        let allocator = current_frame_allocator().ok_or(DmaError::NotInitialised)?;
        let opts = FrameAllocOptions {
            size_pages: npages,
            zeroing: true,
            align_pages: 1,
            no_pcp: false,
            dma: false,
        };
        let head = allocator.alloc(opts).ok_or(DmaError::Exhausted)?;
        #[cfg(any(test, feature = "test-helpers"))]
        PAGES_TAKEN.fetch_add(npages as u64, Ordering::Relaxed);
        let release = RunRelease {
            head,
            len_pages: npages,
        };
        let segment = USegment::<M>::from_unused_run_inner(head, npages)?;
        Ok(Self { segment, release })
    }

    #[inline]
    fn head_paddr(&self) -> PhysAddr {
        self.segment.head_paddr()
    }

    #[inline]
    fn len_pages(&self) -> usize {
        self.segment.len_pages()
    }

    #[inline]
    fn len_bytes(&self) -> usize {
        self.segment.len_bytes()
    }
}

/// IOMMU-mapped, contiguous, coherent (cache-snooped) DMA buffer.
///
/// Drop releases the mapping and the frames without a DMA fence: quiesce the
/// device first.
pub struct DmaCoherent {
    run: DmaRun<DmaCoherentMeta>,
    iova: u64,
}

impl core::fmt::Debug for DmaCoherent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaCoherent")
            .field("iova", &format_args!("{:#x}", self.iova))
            .field("len_bytes", &self.run.len_bytes())
            .finish()
    }
}

impl DmaCoherent {
    /// Allocate `npages` contiguous physical pages, install
    /// [`DmaCoherentMeta`] on each, and IOMMU-map them bidirectionally.
    pub fn alloc(npages: usize) -> Result<Self, DmaError> {
        if npages == 0 {
            return Err(DmaError::Exhausted);
        }
        let mapper = current_iommu_mapper().ok_or(DmaError::NotInitialised)?;
        let run = DmaRun::<DmaCoherentMeta>::alloc(npages)?;
        let iova = mapper.map(
            run.head_paddr(),
            run.len_bytes(),
            DmaDirection::Bidirectional,
        )?;
        Ok(Self { run, iova })
    }

    #[inline]
    pub fn iova(&self) -> u64 {
        self.iova
    }

    #[inline]
    pub fn len_bytes(&self) -> usize {
        self.run.len_bytes()
    }

    #[inline]
    pub fn len_pages(&self) -> usize {
        self.run.len_pages()
    }

    #[inline]
    pub fn phys_base(&self) -> PhysAddr {
        self.run.head_paddr()
    }

    pub fn read_pod<T: Pod>(&self, offset: usize) -> Result<T, UFrameError> {
        self.run.segment.read_pod(offset)
    }

    pub fn write_pod<T: Pod>(&self, offset: usize, value: T) -> Result<(), UFrameError> {
        self.run.segment.write_pod(offset, value)
    }

    pub fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        self.run.segment.read_bytes(offset, dst)
    }

    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> Result<(), UFrameError> {
        self.run.segment.write_bytes(offset, src)
    }
}

impl Drop for DmaCoherent {
    fn drop(&mut self) {
        if let Some(mapper) = current_iommu_mapper() {
            mapper.unmap(self.iova, self.run.len_bytes());
        }
    }
}

/// IOMMU-mapped, contiguous, *streaming* DMA buffer carrying an
/// explicit [`DmaDirection`].
pub struct DmaStream {
    run: DmaRun<DmaStreamMeta>,
    iova: u64,
    direction: DmaDirection,
    _marker: PhantomData<()>,
}

impl core::fmt::Debug for DmaStream {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("DmaStream")
            .field("iova", &format_args!("{:#x}", self.iova))
            .field("len_bytes", &self.run.len_bytes())
            .field("direction", &self.direction)
            .finish()
    }
}

impl DmaStream {
    /// Allocate `npages` contiguous physical pages and IOMMU-map them
    /// in the requested direction.
    pub fn alloc(npages: usize, direction: DmaDirection) -> Result<Self, DmaError> {
        if npages == 0 {
            return Err(DmaError::Exhausted);
        }
        let mapper = current_iommu_mapper().ok_or(DmaError::NotInitialised)?;
        let run = DmaRun::<DmaStreamMeta>::alloc(npages)?;
        let iova = mapper.map(run.head_paddr(), run.len_bytes(), direction)?;
        Ok(Self {
            run,
            iova,
            direction,
            _marker: PhantomData,
        })
    }

    #[inline]
    pub fn iova(&self) -> u64 {
        self.iova
    }

    #[inline]
    pub fn direction(&self) -> DmaDirection {
        self.direction
    }

    #[inline]
    pub fn len_bytes(&self) -> usize {
        self.run.len_bytes()
    }

    #[inline]
    pub fn len_pages(&self) -> usize {
        self.run.len_pages()
    }

    #[inline]
    pub fn phys_base(&self) -> PhysAddr {
        self.run.head_paddr()
    }

    /// Flush CPU writes so the device sees them. No-op on x86_64.
    #[inline]
    pub fn sync_for_device(&self) {}

    /// Invalidate CPU caches so the next read sees device writes.
    /// No-op on x86_64.
    #[inline]
    pub fn sync_for_cpu(&self) {}

    pub fn read_pod<T: Pod>(&self, offset: usize) -> Result<T, UFrameError> {
        self.run.segment.read_pod(offset)
    }

    pub fn write_pod<T: Pod>(&self, offset: usize, value: T) -> Result<(), UFrameError> {
        self.run.segment.write_pod(offset, value)
    }

    pub fn read_bytes(&self, offset: usize, dst: &mut [u8]) -> Result<(), UFrameError> {
        self.run.segment.read_bytes(offset, dst)
    }

    pub fn write_bytes(&self, offset: usize, src: &[u8]) -> Result<(), UFrameError> {
        self.run.segment.write_bytes(offset, src)
    }
}

impl Drop for DmaStream {
    fn drop(&mut self) {
        if let Some(mapper) = current_iommu_mapper() {
            mapper.unmap(self.iova, self.run.len_bytes());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dma_direction_eq() {
        assert_eq!(DmaDirection::ToDevice, DmaDirection::ToDevice);
        assert_ne!(DmaDirection::ToDevice, DmaDirection::FromDevice);
    }

    #[test]
    fn dma_error_eq() {
        assert_eq!(DmaError::NotInitialised, DmaError::NotInitialised);
        assert_ne!(DmaError::NotInitialised, DmaError::Exhausted);
    }

    #[test]
    fn dma_meta_is_default() {
        let _coherent: DmaCoherentMeta = Default::default();
        let _stream: DmaStreamMeta = Default::default();
    }

    #[test]
    fn uframe_error_to_dma_error_oom() {
        let e: DmaError = UFrameError::OutOfMemory.into();
        assert_eq!(e, DmaError::Exhausted);
    }
}
