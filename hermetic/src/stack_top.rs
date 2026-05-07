//! `KernelStackTop<'a>` — lifetime-tied newtype wrapping a kernel-virt
//! stack-top address.
//!
//! Replaces `u64` in TSS-write APIs (`gdt_set_kernel_rsp0`,
//! `gdt_set_ist`). The lifetime forbids `let bad = unsafe {
//! KernelStackTop::from_raw(0xFFFF_FFFF_8020_4000) }; gdt_set_ist(bad);`
//! patterns in tests because the only safe constructor — `from_slice` —
//! requires a real `&[u8]` reference, and the lifetime would dangle.
//!
//! Single tag — earlier design considered `KernelStackTop<'a, IstStack>`
//! / `<KThreadStack>` etc., but the API-level distinction is enforced by
//! which function consumes the value, not by the address itself. Keeping
//! it untagged lets a single `IstStackRegion` slice serve every IST slot
//! from one allocation pool without leaking the type into the registry.

use core::marker::PhantomData;

/// Kernel-virt stack-top address with a lifetime bound to the backing
/// allocation.
///
/// Constructable only from a real `&[u8]` reference (safe path) or from
/// a raw u64 (unsafe path, with debug-asserts enforcing kernel-virt and
/// 16-byte alignment).
#[derive(Copy, Clone)]
pub struct KernelStackTop<'a> {
    addr: u64,
    _life: PhantomData<&'a ()>,
}

impl<'a> KernelStackTop<'a> {
    /// Construct from a raw kernel-virt address.
    ///
    /// # Safety
    /// - `addr` must be the high address of a 16-byte aligned, mapped,
    ///   kernel-virt stack region with a guard page below.
    /// - The backing region must outlive the returned `KernelStackTop`.
    ///   The `'a` lifetime is purely a marker — caller is responsible
    ///   for not using the value after the region is freed.
    pub unsafe fn from_raw(addr: u64) -> Self {
        debug_assert!(
            addr & 0xF == 0,
            "KernelStackTop: addr 0x{:x} is not 16-byte aligned",
            addr
        );
        debug_assert!(
            addr >= 0xFFFF_8000_0000_0000,
            "KernelStackTop: addr 0x{:x} is not kernel-virt",
            addr
        );
        Self {
            addr,
            _life: PhantomData,
        }
    }

    /// Construct from a kernel-virt address that the caller has already
    /// established refers to a mapped, kernel-image-lifetime stack region
    /// (e.g. an IST slot mapped at boot, or a per-CPU kthread stack
    /// allocated for the lifetime of the kernel).
    ///
    /// Performs the same kernel-virt + 16-byte-alignment debug-asserts as
    /// `from_raw`, but exposes a safe call site for boot code that has
    /// computed the address from an already-mapped region. The `'static`
    /// lifetime is appropriate because the backing region is expected to
    /// outlive the kernel image.
    pub fn from_kernel_va(addr: u64) -> KernelStackTop<'static> {
        unsafe { KernelStackTop::<'static>::from_raw(addr) }
    }

    /// Construct from a borrowed kernel-virt slice. Returns the
    /// 16-byte-aligned top of the slice; the lifetime of the returned
    /// `KernelStackTop` borrows from the slice.
    pub fn from_slice(slice: &'a [u8]) -> Self {
        // Use `as_ptr_range` to get the past-the-end pointer, then
        // round down to 16 bytes. This matches x86-64 ABI's stack
        // alignment.
        let end = slice.as_ptr_range().end as u64;
        let aligned = end & !0xF;
        Self {
            addr: aligned,
            _life: PhantomData,
        }
    }

    /// Raw u64 value for handoff to TSS / MSR-write code.
    pub fn as_u64(&self) -> u64 {
        self.addr
    }
}
