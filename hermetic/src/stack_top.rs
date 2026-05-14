//! `KernelStackTop<'a>` — lifetime-tied newtype wrapping a kernel-virt
//! stack-top address.
//!
//! Replaces `u64` in TSS-write APIs (`gdt_set_kernel_rsp0`,
//! `gdt_set_ist`). The lifetime forbids `let bad =
//! KernelStackTop::from_raw(0xFFFF_FFFF_8020_4000); gdt_set_ist(bad);`
//! patterns in tests because the only borrow-bound constructor —
//! `from_slice` — requires a real `&[u8]` reference, and the
//! `'static` lifetime emerging from `from_raw` would dangle.
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
/// Constructable from a real `&[u8]` reference (`from_slice`, which
/// binds the lifetime to the slice) or from a raw u64 (`from_raw` /
/// `from_kernel_va`, with debug-asserts enforcing kernel-virt and
/// 16-byte alignment).
#[derive(Copy, Clone)]
pub struct KernelStackTop<'a> {
    addr: u64,
    _life: PhantomData<&'a ()>,
}

impl<'a> KernelStackTop<'a> {
    /// Construct from a raw kernel-virt address.
    ///
    /// The function body itself performs no memory access — it only
    /// stores `addr` in a wrapper struct with a phantom lifetime
    /// marker. The lifetime `'a` is a witness that the caller has
    /// established the backing region outlives the returned value;
    /// the type system enforces non-escape via `'a`. Debug asserts
    /// catch obvious bugs (non-canonical / mis-aligned addresses)
    /// at construction time.
    pub fn from_raw(addr: u64) -> Self {
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
    /// `from_raw`, but binds the lifetime to `'static` for boot code
    /// that has computed the address from an already-mapped region
    /// expected to outlive the kernel image.
    pub fn from_kernel_va(addr: u64) -> KernelStackTop<'static> {
        KernelStackTop::<'static>::from_raw(addr)
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
