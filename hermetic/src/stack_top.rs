//! `KernelStackTop<'a>` — kernel-virt stack-top address, replacing `u64` in
//! the TSS-write APIs (`gdt_set_kernel_rsp0`, `gdt_set_ist`). The only
//! borrow-bound constructor is `from_slice`, which requires a real `&[u8]`,
//! so a fabricated address cannot reach those APIs with a lifetime tied to
//! anything real.
//!
//! Deliberately untagged by stack kind: one `IstStackRegion` slice serves
//! every IST slot from a single pool, and which function consumes the value
//! is what distinguishes the kinds.

use core::marker::PhantomData;

/// Kernel-virt stack-top address with a lifetime bound to the backing
/// allocation.
#[derive(Copy, Clone)]
pub struct KernelStackTop<'a> {
    addr: u64,
    _life: PhantomData<&'a ()>,
}

impl<'a> KernelStackTop<'a> {
    /// Construct from a raw kernel-virt address. `'a` is a caller claim that
    /// the backing region outlives the returned value; the debug-asserts
    /// only catch mis-aligned or non-kernel-virt addresses.
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

    /// Construct a `'static` stack top from a kernel-virt address the caller
    /// has established refers to a mapped region outliving the kernel image
    /// (an IST slot mapped at boot, a per-CPU kthread stack).
    pub fn from_kernel_va(addr: u64) -> KernelStackTop<'static> {
        KernelStackTop::<'static>::from_raw(addr)
    }

    /// Construct from a borrowed kernel-virt slice: the 16-byte-aligned top
    /// of the slice, borrowing the slice's lifetime.
    pub fn from_slice(slice: &'a [u8]) -> Self {
        // 16-byte alignment per the x86-64 ABI stack rule.
        let end = slice.as_ptr_range().end as u64;
        let aligned = end & !0xF;
        Self {
            addr: aligned,
            _life: PhantomData,
        }
    }

    /// Raw address for handoff to TSS / MSR-write code.
    pub fn as_u64(&self) -> u64 {
        self.addr
    }
}
