//! Safe reads at kernel-virtual integer addresses.
//!
//! Every helper here is **safe to call** but the interior `unsafe` is
//! sound only when the caller has confirmed that:
//!
//! - `addr` is a valid kernel-virtual byte address mapped read+write
//!   (for the mutating helpers) and points at a properly aligned `T`,
//! - the underlying region is owned by the caller for the duration of
//!   the call so no concurrent mutation tears the read,
//! - if `T: !Pod`, the bytes at `addr` are a valid representation of
//!   `T`.

/// Read a naturally-aligned `T: Copy` at the kernel-virtual byte
/// address `addr`. The caller must ensure `addr` is non-null, aligned
/// for `T`, and points at an initialised `T` whose memory is exclusively
/// owned for the duration of the read.
#[inline]
pub fn read_kernel<T: Copy>(addr: u64) -> T {
    let p = addr as *const T;
    debug_assert!(!p.is_null(), "read_kernel: addr must be non-null");
    debug_assert_eq!(
        addr as usize % core::mem::align_of::<T>(),
        0,
        "read_kernel: addr must be aligned for T"
    );
    // SAFETY: caller upholds the module-level contract.
    unsafe { *p }
}

/// Read a naturally-aligned `T: Copy` at byte offset `offset` past the
/// kernel-virtual base address `addr`. Same contract as
/// [`read_kernel`], with the offset taken in `T`-sized strides.
#[inline]
pub fn read_kernel_at<T: Copy>(addr: u64, offset: usize) -> T {
    let p = addr as *const T;
    debug_assert!(!p.is_null(), "read_kernel_at: addr must be non-null");
    // SAFETY: caller upholds the module-level contract; the offset is
    // expressed in `T`-strides matching the underlying layout.
    unsafe { *p.add(offset) }
}
/// High 17 bits of a canonical kernel-half x86-64 virtual address: both
/// 0xFFFF_8000_0000_0000 and 0xFFFF_FFFF_FFFF_FFFF satisfy `(addr >> 47) == 0x1FFFF`.
const CANONICAL_KERNEL_HIGH_BITS: u64 = 0x1FFFF;

/// Returns `true` when `addr` is a canonical kernel-half virtual
/// address with `align`-byte alignment and `extra` extra bytes of
/// canonical-kernel headroom past `addr`. `align` and `extra` are
/// caller-supplied; both must be powers of two for the cheap mask.
#[inline]
pub const fn is_canonical_kernel(addr: u64, align: u64, extra: u64) -> bool {
    if addr == 0 {
        return false;
    }
    if (addr >> 47) != CANONICAL_KERNEL_HIGH_BITS {
        return false;
    }
    if (addr & (align - 1)) != 0 {
        return false;
    }
    let Some(end) = addr.checked_add(extra) else {
        return false;
    };
    (end >> 47) == CANONICAL_KERNEL_HIGH_BITS
}

// The probe routes its load through a known RIP range: the page-fault
// handler recognises a kernel-mode fault inside
// `__ostd_probe_read_start..end` and redirects RIP to
// `__ostd_probe_read_fault`, so a diagnostic walker can read an address
// it cannot prove is mapped without escalating into a second panic.

core::arch::global_asm!(
    ".global __ostd_probe_read_u64",
    ".global __ostd_probe_read_start",
    ".global __ostd_probe_read_end",
    ".global __ostd_probe_read_fault",
    "__ostd_probe_read_u64:",
    "__ostd_probe_read_start:",
    "    mov rax, [rdi]",
    "__ostd_probe_read_end:",
    "    mov [rsi], rax",
    "    mov eax, 1",
    "    ret",
    "__ostd_probe_read_fault:",
    "    xor eax, eax",
    "    ret",
);

unsafe extern "C" {
    fn __ostd_probe_read_u64(addr: *const u64, out: *mut u64) -> u64;
    fn __ostd_probe_read_start();
    fn __ostd_probe_read_end();
    fn __ostd_probe_read_fault();
}

/// Returns `true` if `rip` falls within the probe-read load region. The
/// page-fault handler queries this on kernel-mode faults and redirects
/// RIP to [`probe_read_fault_ip`] on a match.
#[inline]
pub fn is_probe_read_ip(rip: u64) -> bool {
    let start = __ostd_probe_read_start as *const () as u64;
    let end = __ostd_probe_read_end as *const () as u64;
    rip >= start && rip < end
}

/// RIP the page-fault handler rewrites to when [`is_probe_read_ip`]
/// matches — the probe's failure tail.
#[inline]
pub fn probe_read_fault_ip() -> u64 {
    __ostd_probe_read_fault as *const () as u64
}

/// Read an aligned `u64` from a canonical kernel address, tolerating
/// unmapped pages. Returns `None` if the address fails the canonical-
/// kernel + 8-byte-aligned + 8-byte-headroom check, **or** if the read
/// page-faults (the handler redirects the probe to its failure tail).
#[inline]
pub fn read_volatile_canonical_kernel_u64(addr: u64) -> Option<u64> {
    if !is_canonical_kernel(addr, 8, 8) {
        return None;
    }
    let mut out = 0u64;
    // SAFETY: the probe load is fault-recoverable by construction;
    // `out` is a live stack slot.
    let ok = unsafe { __ostd_probe_read_u64(addr as *const u64, &mut out) };
    if ok != 0 { Some(out) } else { None }
}

/// Read an unaligned `u64` from a raw pointer.
///
/// # Safety
///
/// `ptr` must point to 8 readable bytes inside the kernel address
/// space. The lone callers (`handle_corrupt_iret_frame` and the panic
/// vicinity dump) bound the read region either via the current task's
/// kernel-stack extents or via a ±128-byte window around a probed
/// pointer.
#[inline]
pub unsafe fn read_unaligned_u64(ptr: *const u64) -> u64 {
    // SAFETY: contract delegated to caller (see fn-level docs).
    unsafe { core::ptr::read_unaligned(ptr) }
}

/// Read the five-word IRETQ frame `[RIP, CS, RFLAGS, RSP, SS]` from a
/// kernel-space pointer published by an ISR asm stub.
///
/// Validates canonical-kernel + 8-byte-aligned + 40-byte headroom and
/// returns `None` on failure; the residual unmapped-page risk is
/// accepted for the panic-time forensic value.
#[inline]
pub fn read_iret_frame(ptr: *const u64) -> Option<[u64; 5]> {
    let addr = ptr as u64;
    if !is_canonical_kernel(addr, 8, 40) {
        return None;
    }
    let mut out = [0u64; 5];
    // SAFETY: the reads stay inside the 5×u64 window just validated
    // above, which is the window the ISR asm stub set up.
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = unsafe { core::ptr::read_unaligned(ptr.add(i)) };
    }
    Some(out)
}

/// Read an unaligned `u64` at `ptr` only if the byte window
/// `[ptr, ptr + 8)` lies inside `[lo, hi)`. Returns `None` when the
/// window escapes the supplied bounds.
#[inline]
pub fn read_unaligned_u64_in_range(ptr: *const u64, lo: usize, hi: usize) -> Option<u64> {
    let addr = ptr as usize;
    let end = addr.checked_add(8)?;
    if addr < lo || end > hi {
        return None;
    }
    // SAFETY: caller-supplied `[lo, hi)` bounds a live mapped region,
    // and the 8-byte read was just checked to lie entirely inside it.
    Some(unsafe { core::ptr::read_unaligned(ptr) })
}

/// Safe `fn(*mut InterruptFrame)` ↔ `*mut ()` round-trip: fn-pointers
/// and `*mut ()` are layout-compatible on x86_64.
pub mod fn_ptr {
    /// Encode an `Option<F>` as `*mut ()`.  `None` → null.
    #[inline]
    pub fn encode<F: Copy>(f: Option<F>) -> *mut () {
        const {
            assert!(core::mem::size_of::<F>() == core::mem::size_of::<*mut ()>());
        }
        match f {
            Some(func) => {
                // SAFETY: the const assert guarantees pointer-sized `F`;
                // fn-pointers are non-null and ABI-equivalent to `*mut ()`.
                unsafe { core::mem::transmute_copy::<F, *mut ()>(&func) }
            }
            None => core::ptr::null_mut(),
        }
    }

    /// Decode a slot value back into `Option<F>`. Null → `None`.
    #[inline]
    pub fn decode<F: Copy>(raw: *mut ()) -> Option<F> {
        const {
            assert!(core::mem::size_of::<F>() == core::mem::size_of::<*mut ()>());
        }
        if raw.is_null() {
            None
        } else {
            // SAFETY: every non-null value was stored via `encode::<F>`
            // above, so the round-trip preserves the original fn ptr.
            Some(unsafe { core::mem::transmute_copy::<*mut (), F>(&raw) })
        }
    }
}

/// Sanity-check probe: read one byte from a linker-published kernel
/// symbol pointer.
///
/// # Safety
/// `addr` must point to a readable byte inside the kernel image (or
/// the call sites the kernel pretends so) — callers obtain it from
/// the linker via `slopos_ostd::extern_block!`. A misaddressed probe
/// faults rather than silently returning garbage; this is the desired
/// boot-time diagnostic.
#[inline]
pub unsafe fn probe_kernel_byte(addr: *const u8) -> u8 {
    // SAFETY: contract delegated to caller (see fn-level docs).
    unsafe { core::ptr::read_volatile(addr) }
}

/// Kernel entry symbol `_start`; kept private so only
/// [`verify_kernel_entry_alive`] can dereference it.
mod kernel_entry_sym {
    crate::extern_block! {
        pub(super) mod externs {
            static _start: u8;
        }
    }
}

/// Best-effort sanity check that the linker placed the kernel image
/// where the bootloader handoff promised: a fault on the one-byte
/// `_start` probe triple-faults during boot, which is the diagnostic.
#[inline]
pub fn verify_kernel_entry_alive() {
    let addr = kernel_entry_sym::externs::_start_addr();
    // SAFETY: `_start` is a linker-published byte inside the kernel
    // image; an unmapped image faults, which is the intended outcome.
    let _ = unsafe { core::ptr::read_volatile(addr) };
}
