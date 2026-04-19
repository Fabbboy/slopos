//! Typed `CR3` primitives: `Pcid`, `MmContextId`, `Cr3Value`.
//!
//! `CR3` on x86-64 is a 64-bit register whose layout, once `CR4.PCIDE`
//! is enabled, is:
//!
//! ```text
//!   63             52 51                         12 11         0
//!  +---------+------+-----------------------------+-------------+
//!  | NOFLUSH | rsv0 |       PML4 phys (52-bit)    |   PCID (12) |
//!  +---------+------+-----------------------------+-------------+
//! ```
//!
//! - Bit 63 (`NOFLUSH`): when set on a `mov CR3, reg`, the CPU preserves
//!   the PCID's existing TLB entries across the load. Used for KPTI
//!   user↔kernel CR3 swaps and ASID-reuse context switches.
//! - Bits 11..0 (`PCID`): address-space identifier. Zero when PCID is
//!   disabled. Kernel-only PCID is `0` by convention; per-process PCIDs
//!   are allocated per-CPU by `mm::mmu::asid`.
//!
//! The `Cr3Value` newtype is the only thing the kernel writes to CR3 —
//! raw `u64`s never cross module boundaries. Callers build a value with
//! [`Cr3Value::kernel`] or, once PCID is live, [`Cr3Value::new`].

use core::sync::atomic::{AtomicU64, Ordering};

use slopos_abi::addr::PhysAddr;
use slopos_arch::cpu;

/// 12-bit hardware process-context identifier.
///
/// The CPU tags TLB entries with the PCID in CR3[11:0] so entries from
/// different address spaces can coexist. A `Pcid` is always bound to a
/// particular CPU's slot pool (see `mm::mmu::asid`); a raw PCID value
/// alone is not enough to reconstruct a CR3 — you also need the owning
/// `MmContext` generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(transparent)]
pub struct Pcid(u16);

impl Pcid {
    /// Kernel / bootstrap PCID. Kernel mappings use this whenever PCID
    /// is enabled; they are also marked `GLOBAL` so they outlive CR3
    /// reloads.
    pub const KERNEL: Pcid = Pcid(0);

    /// Construct a `Pcid` from a raw value.
    ///
    /// Returns `None` when the value does not fit in the architectural
    /// 12-bit field.
    #[inline]
    pub const fn new(raw: u16) -> Option<Self> {
        if raw <= 0x0FFF { Some(Self(raw)) } else { None }
    }

    /// Construct without bounds checking. Caller guarantees `raw < 4096`.
    #[inline]
    pub const fn new_unchecked(raw: u16) -> Self {
        debug_assert!(raw <= 0x0FFF);
        Self(raw)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn bits(self) -> u64 {
        self.0 as u64
    }
}

/// A stable, monotonically-increasing identifier for a single
/// `MmContext` (i.e. a process address space).
///
/// Unlike `Pcid`, this is **never reused**. The 64-bit space makes
/// rollover inconceivable over the lifetime of any real machine. The
/// hardware PCID is an entirely separate resource — when a context
/// migrates between CPUs or is evicted from a PCID slot, its
/// `MmContextId` stays the same; a fresh `Pcid` is assigned by the
/// per-CPU ASID pool.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct MmContextId(u64);

impl MmContextId {
    /// Sentinel for "no context" — used by per-CPU ASID slots that are
    /// empty, and by page-fault handlers that need to report an
    /// unloaded address space.
    pub const INVALID: MmContextId = MmContextId(0);

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn is_valid(self) -> bool {
        self.0 != 0
    }

    /// Build a context ID from a raw 64-bit value. Used where a
    /// context ID is derived from the legacy `process_id` surface
    /// rather than freshly allocated by `alloc_mm_context_id`.
    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }
}

/// Global monotonic allocator for `MmContextId`. Never wraps.
static NEXT_MM_CONTEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Allocate a fresh, never-before-used `MmContextId`.
#[inline]
pub fn alloc_mm_context_id() -> MmContextId {
    MmContextId(NEXT_MM_CONTEXT_ID.fetch_add(1, Ordering::Relaxed))
}

/// A complete `CR3` value: 52-bit PML4 physical frame + 12-bit PCID +
/// optional no-flush bit.
///
/// This is the only type the kernel writes to CR3. `Debug` on it shows
/// the decomposed fields, so tracing a stale TLB bug doesn't require
/// decoding a raw hex value by hand.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct Cr3Value(u64);

impl Cr3Value {
    /// `CR3` bit mask that isolates the 52-bit physical frame. This is
    /// `0x000F_FFFF_FFFF_F000` — matches `PageFlags::ADDRESS_MASK`.
    pub const PHYS_MASK: u64 = 0x000F_FFFF_FFFF_F000;
    /// PCID field, bits `[11:0]`.
    pub const PCID_MASK: u64 = 0x0000_0000_0000_0FFF;
    /// Bit 63: suppress the automatic TLB flush on `mov CR3, reg`.
    pub const NOFLUSH_BIT: u64 = 1 << 63;

    /// Build a CR3 value from a page-aligned PML4 physical address, a
    /// PCID, and a no-flush selector.
    ///
    /// Panics in debug builds if `pml4_phys` is not 4 KiB aligned.
    #[inline]
    pub fn new(pml4_phys: PhysAddr, pcid: Pcid, no_flush: bool) -> Self {
        let phys = pml4_phys.as_u64();
        debug_assert!(
            phys & !Self::PHYS_MASK == 0,
            "CR3 PML4 must be 4KiB aligned"
        );
        let mut bits = (phys & Self::PHYS_MASK) | (pcid.bits() & Self::PCID_MASK);
        if no_flush {
            bits |= Self::NOFLUSH_BIT;
        }
        Self(bits)
    }

    /// Build a CR3 for kernel PCID (0) with the flush bit cleared.
    /// This is the pre-PCID equivalent of `cpu::write_cr3(phys)`; every
    /// legacy call site has been migrated onto it.
    #[inline]
    pub fn kernel(pml4_phys: PhysAddr) -> Self {
        Self::new(pml4_phys, Pcid::KERNEL, false)
    }

    /// Return a new value with the no-flush bit set.
    #[inline]
    pub const fn with_noflush(self) -> Self {
        Self(self.0 | Self::NOFLUSH_BIT)
    }

    /// Return a new value with the no-flush bit cleared.
    #[inline]
    pub const fn without_noflush(self) -> Self {
        Self(self.0 & !Self::NOFLUSH_BIT)
    }

    /// The PML4 physical address, page-aligned.
    #[inline]
    pub fn pml4_phys(self) -> PhysAddr {
        PhysAddr::new(self.0 & Self::PHYS_MASK)
    }

    #[inline]
    pub const fn pcid(self) -> Pcid {
        Pcid((self.0 & Self::PCID_MASK) as u16)
    }

    #[inline]
    pub const fn no_flush(self) -> bool {
        (self.0 & Self::NOFLUSH_BIT) != 0
    }

    /// The raw 64-bit value, ready for `mov CR3, reg`.
    #[inline]
    pub const fn bits(self) -> u64 {
        self.0
    }

    /// Reinterpret a raw CR3 read (e.g. from assembly or hardware) as a
    /// `Cr3Value`. Used by the scheduler on entry to compare an
    /// incoming hardware CR3 against the value we intend to load.
    #[inline]
    pub const fn from_raw(bits: u64) -> Self {
        Self(bits)
    }
}

impl core::fmt::Debug for Cr3Value {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Cr3Value")
            .field(
                "pml4_phys",
                &format_args!("{:#x}", self.pml4_phys().as_u64()),
            )
            .field("pcid", &self.pcid().raw())
            .field("no_flush", &self.no_flush())
            .finish()
    }
}

/// Read `CR3` through the typed primitive.
#[inline]
pub fn read_cr3_value() -> Cr3Value {
    Cr3Value::from_raw(cpu::read_cr3())
}

/// Write a typed `CR3` value.
///
/// Single choke point for every CR3 write in the kernel. ASID
/// bookkeeping lives in `mm::mmu::asid`; future KPTI activation adds
/// the user/kernel CR3 swap on ring transition through the same
/// primitive.
#[inline]
pub fn write_cr3_value(value: Cr3Value) {
    cpu::write_cr3(value.bits());
}
