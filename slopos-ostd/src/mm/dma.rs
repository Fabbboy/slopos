//! `DmaCoherent` / `DmaStream`: typed handles for IOMMU-mapped
//! device-visible memory.
//!
//! `DmaCoherent` is for buffers the device polls without explicit
//! cache management (descriptor rings, doorbell pages); `DmaStream`
//! carries an explicit [`DmaDirection`] and a `sync_for_device` /
//! `sync_for_cpu` API for non-coherent architectures (no-op on
//! x86_64; the API is there for future ARM64).
//!
//! # IOMMU policy
//!
//! All device-visible mappings flow through the registered
//! [`IommuMapper`]. The default state is *no mapper registered* →
//! [`DmaError::NotInitialised`], i.e. type-level default-deny: a
//! driver cannot allocate DMA storage until the IOMMU is wired up.
//!
//! Caveat — only the type-level deny ships here; programming the
//! hardware IOMMU is the registered mapper's responsibility. A device
//! the bootloader left with an open DMA window will still be able to
//! issue arbitrary DMA until the mapper closes it.
//!
//! # Drop and in-flight DMA
//!
//! Dropping a `DmaCoherent` / `DmaStream` calls [`IommuMapper::unmap`]
//! and releases the underlying frames. **OSTD does not issue a DMA
//! fence**: drivers must quiesce the device (clear queues, mask
//! interrupts) *before* dropping the handle. A drop while the device
//! still issues DMA would race the unmap and corrupt unrelated
//! kernel memory.
//!
//! # Surface shape
//!
//! The current IOMMU surface is contiguous-only (`map(phys, size)`).
//! A per-page IOVA list — needed for scatter/gather and for VT-d's
//! non-contiguous IOVA layouts — is a future extension to the trait.

use core::marker::PhantomData;
use core::sync::atomic::{AtomicPtr, Ordering};

use slopos_abi::addr::PhysAddr;

use crate::mm::frame::{AnyFrameMeta, FrameAllocOptions};
use crate::mm::frame_alloc::current_frame_allocator;
use crate::mm::pod::Pod;
use crate::mm::uframe::{AnyUFrameMeta, UFrameError, USegment};
use crate::sync::BspToken;

// ---------------------------------------------------------------------------
// Direction + errors.
// ---------------------------------------------------------------------------

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
    /// [`register_iommu_mapper`] has not been called yet — type-level
    /// default-deny.
    NotInitialised,
    /// Frame or page-table allocator is out of memory.
    Exhausted,
    /// IOMMU policy refuses to map this physical range (e.g. it
    /// covers kernel-sensitive memory).
    Forbidden,
    /// Mapper failed for another reason (page-walk failure, etc.).
    MappingFailed,
}

impl From<UFrameError> for DmaError {
    fn from(e: UFrameError) -> Self {
        match e {
            UFrameError::OutOfMemory => DmaError::Exhausted,
            // Misalignment / OOB / Truncated should not occur at
            // alloc-time; treat as MappingFailed for surface-area
            // simplicity. Callers exercising the byte-copy methods
            // surface `UFrameError` directly.
            _ => DmaError::MappingFailed,
        }
    }
}

// ---------------------------------------------------------------------------
// DmaCoherentMeta / DmaStreamMeta.
// ---------------------------------------------------------------------------

/// Per-frame metadata for `DmaCoherent` pages. ZST: distinct type so
/// the `M` parameter on [`USegment`] tracks coherent vs. streaming at
/// the type system level.
#[derive(Debug, Default)]
pub struct DmaCoherentMeta;

// SAFETY: ZST has no representation invariants. A DMA run's pages are owned
// by its [`DmaRun`], not by the per-frame lifecycle, so
// `returns_frame_on_last_drop` is `false`: the last drop resets the slot and
// `RunRelease` returns the pages.
unsafe impl AnyFrameMeta for DmaCoherentMeta {
    fn returns_frame_on_last_drop(&self) -> bool {
        false
    }
}

// SAFETY: DMA pages are by definition peripheral-tampered, so the
// `AnyUFrameMeta` no-`&T` contract is the right home — only
// byte-copy access is sound.
unsafe impl AnyUFrameMeta for DmaCoherentMeta {}

/// Per-frame metadata for `DmaStream` pages.
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

// ---------------------------------------------------------------------------
// IommuMapper trait + registration.
// ---------------------------------------------------------------------------

/// Pluggable IOMMU mapper. Only one is registered per kernel
/// lifetime; `DmaCoherent::alloc` / `DmaStream::alloc` go through it
/// and `Drop` calls `unmap`.
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

/// One-shot wiring point for the kernel's [`IommuMapper`]. The
/// `&BspToken<'brand>` witnesses BSP-only init; the underlying
/// mapper must be sound for concurrent `map` / `unmap` from any CPU.
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
    let raw = IOMMU_MAPPER.inner.load(Ordering::Acquire);
    if raw.is_null() {
        return None;
    }
    // SAFETY: Inv. 6. `raw` was produced by `register_iommu_mapper`
    // from a `&'static &'static dyn IommuMapper`; the storage is
    // `'static` by contract.
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

// ---------------------------------------------------------------------------
// Identity (passthrough) mapper.
// ---------------------------------------------------------------------------

/// Passthrough mapper: IOVA == physical address, unmap is a no-op.
///
/// On platforms with no IOMMU policy to enforce (e.g. QEMU `q35` without
/// VT-d), this turns "no mapper registered → hard deny" into "passthrough
/// policy", giving `DmaCoherent` / `DmaStream` their first live mapper. A
/// future VT-d mapper swaps in at the same single seam
/// ([`register_iommu_mapper`]).
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
///
/// Forwards to the single [`register_iommu_mapper`] seam, so the same
/// once-only / default-deny guarantees apply (its `assert!(prev.is_null())`
/// rejects a second registration). Default-deny is preserved: a driver that
/// never allocates DMA touches no mapping, and the slot can only be set once.
pub fn register_identity_dma_mapper<'brand>(token: &BspToken<'brand>) {
    register_iommu_mapper(token, &IDENTITY_SLOT);
}

/// Test-only identity-mapper registration that bypasses the `BspToken` and
/// the double-register assert, so a test can restore the global mapper after
/// [`reset_for_test`] without re-minting the one-shot BSP token.
#[cfg(any(test, feature = "test-helpers"))]
pub fn register_identity_dma_mapper_for_test() {
    let raw = &IDENTITY_SLOT as *const &'static dyn IommuMapper as *mut ();
    IOMMU_MAPPER.inner.store(raw, Ordering::Release);
}

// ---------------------------------------------------------------------------
// DmaRun: page ownership for a DMA segment.
// ---------------------------------------------------------------------------

/// Returns a contiguous run of physical pages to the registered
/// [`FrameAlloc`](crate::mm::frame::FrameAlloc) on drop.
///
/// One `dealloc` at the head covers the whole run: the allocator recovers a
/// run's extent from its own descriptor, so `len_pages` is a courtesy
/// argument rather than the authority on how much comes back.
struct RunRelease {
    head: PhysAddr,
    len_pages: usize,
}

impl Drop for RunRelease {
    fn drop(&mut self) {
        if let Some(allocator) = current_frame_allocator() {
            allocator.dealloc(self.head, self.len_pages);
        }
    }
}

/// A contiguous run of DMA pages together with the release that hands them
/// back to the frame allocator.
///
/// [`DmaCoherentMeta`] and [`DmaStreamMeta`] declare
/// `returns_frame_on_last_drop() == false`, so the per-frame lifecycle resets
/// each `MetaSlot` but leaves the pages allocated. This type owns the
/// compensating `dealloc`.
///
/// **The field order is load-bearing.** Fields drop in declaration order, so
/// `segment` — and with it every `MetaSlot` reset — completes before
/// `release` makes the pages claimable again. That is the ordering
/// [`Frame`](crate::mm::frame::Frame)'s own destructor establishes for metas
/// that do return their page: a free-listed paddr always has a slot reading
/// UNUSED, so the next claimant's `from_unused` cannot lose a race against a
/// slot that is still TYPED.
struct DmaRun<M: AnyUFrameMeta> {
    segment: USegment<M>,
    /// Reached only through its `Drop` glue, so tagged `#[allow(dead_code)]`.
    #[allow(dead_code)]
    release: RunRelease,
}

impl<M: AnyUFrameMeta> DmaRun<M> {
    /// Allocate `npages` contiguous, zeroed pages and install `M` on each.
    ///
    /// The release is armed the instant the allocator hands over the run and,
    /// being the first local declared, is the last local dropped. Every error
    /// path from there on therefore returns the pages, and so does every
    /// error path in the callers that take the run by value.
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

// ---------------------------------------------------------------------------
// DmaCoherent.
// ---------------------------------------------------------------------------

/// IOMMU-mapped, contiguous, coherent (cache-snooped) DMA buffer.
///
/// Constructed via [`Self::alloc`]. Drop releases the IOMMU mapping
/// and the underlying frames. See module-level docs for the in-flight
/// DMA caveat — drivers must quiesce the device before drop.
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
    /// [`DmaCoherentMeta`] on each, and IOMMU-map them
    /// bidirectionally. Requires a registered [`IommuMapper`] and
    /// [`crate::mm::frame::FrameAlloc`].
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

    /// Device-visible IOVA base.
    #[inline]
    pub fn iova(&self) -> u64 {
        self.iova
    }

    /// Total byte length.
    #[inline]
    pub fn len_bytes(&self) -> usize {
        self.run.len_bytes()
    }

    /// Number of pages.
    #[inline]
    pub fn len_pages(&self) -> usize {
        self.run.len_pages()
    }

    /// Physical base address of the underlying contiguous run.
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
        // Driver-side responsibility: device must be quiesced before
        // the handle drops. OSTD does not issue a DMA fence.
        if let Some(mapper) = current_iommu_mapper() {
            mapper.unmap(self.iova, self.run.len_bytes());
        }
        // `run` drops after this body: every `MetaSlot` resets, then the
        // pages go back to the frame allocator.
    }
}

// ---------------------------------------------------------------------------
// DmaStream.
// ---------------------------------------------------------------------------

/// IOMMU-mapped, contiguous, *streaming* DMA buffer carrying an
/// explicit [`DmaDirection`].
///
/// `sync_for_device` / `sync_for_cpu` are no-ops on x86_64 (the
/// architecture is cache-coherent for DMA); the API is preserved so
/// drivers can be written portably for future ARM64 support.
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
        // `run` drops after this body: every `MetaSlot` resets, then the
        // pages go back to the frame allocator.
    }
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

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
