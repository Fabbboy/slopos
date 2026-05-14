//! Canonical-kernel-pointer safety wrappers.
//!
//! Kernel-side NMI/diagnostic code occasionally needs to follow raw
//! pointers (frame-pointer chains, IRET-frame snapshots) on paths that
//! must never fault recursively. Each helper here folds the
//! "is the address a canonical kernel-half pointer with adequate
//! alignment + headroom" check into one place, so the `unsafe { … }`
//! around `core::ptr::read_volatile` / `read_unaligned` stays inside
//! OSTD.
//!
//! The validation model is **best-effort**: we eliminate the obvious
//! fault classes (null, user-half, unaligned, end-of-canonical-space
//! wrap-around) but we cannot prove the address lies inside a mapped
//! kernel page — that would need a page-table walk, which is itself
//! fault-prone. Callers using these helpers from NMI / panic context
//! still have to accept that a read into an unmapped kernel page
//! faults; the validation just rules out the classes a tight bounds
//! check would otherwise miss.

/// High 17 bits of a canonical kernel-half x86-64 virtual address.
///
/// Both 0xFFFF_8000_0000_0000 and 0xFFFF_FFFF_FFFF_FFFF satisfy
/// `(addr >> 47) == 0x1FFFF`.
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

/// Read an aligned `u64` from a canonical kernel address with
/// `read_volatile`. Returns `None` if the address fails the canonical-
/// kernel + 8-byte-aligned + 8-byte-headroom check.
///
/// Designed for NMI-context frame-pointer chase loops where the
/// pre-validation cuts off the common fault classes (null /
/// user-half / unaligned / canonical wrap) before issuing the read.
///
/// Best-effort only — a read into an unmapped kernel page still
/// faults; callers in IST context accept that risk in exchange for
/// the diagnostic value.
#[inline]
pub fn read_volatile_canonical_kernel_u64(addr: u64) -> Option<u64> {
    if !is_canonical_kernel(addr, 8, 8) {
        return None;
    }
    let ptr = addr as *const u64;
    // SAFETY: address validated canonical-kernel + 8-byte-aligned +
    // has 8 bytes of canonical headroom. Caller accepts the residual
    // unmapped-page fault risk (panic/NMI diagnostic only).
    Some(unsafe { core::ptr::read_volatile(ptr) })
}

/// Read an unaligned `u64` from a raw pointer with `read_unaligned`.
///
/// The pointer must point to 8 readable bytes; callers in
/// IRET-corruption diagnostics typically validate the surrounding
/// region via [`is_canonical_kernel`] first. The unsafe `read_unaligned`
/// is centralised here.
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

/// Safe `fn(*mut InterruptFrame)` ↔ `*mut ()` round-trip.
///
/// fn-pointers and `*mut ()` are layout-compatible on x86_64; round-
/// tripping through an `AtomicPtr<()>` is a common pattern for slot-
/// based exception-handler tables. The unsafe `transmute` lives once,
/// here. Consumers wrap the registry with these helpers.
pub mod fn_ptr {
    /// Encode an `Option<F>` as `*mut ()`.  `None` → null.
    #[inline]
    pub fn encode<F: Copy>(f: Option<F>) -> *mut () {
        const {
            assert!(core::mem::size_of::<F>() == core::mem::size_of::<*mut ()>());
        }
        match f {
            Some(func) => {
                // SAFETY: const assert guarantees layout-equal pointer
                // size. fn-pointers are non-null and ABI-equivalent to
                // `*mut ()`.
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
/// symbol pointer. Returns the byte value. Used by boot's
/// `verify_memory_layout` to confirm the kernel image is mapped where
/// the linker said it would be — a fault here triple-faults during
/// boot, which is the intended outcome.
///
/// The unsafe `read_volatile` lives once, here.
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

/// Internal extern block holding the kernel entry symbol `_start`.
/// Consumed by [`verify_kernel_entry_alive`] below; kept private so
/// only the bundled probe can dereference it.
mod kernel_entry_sym {
    crate::extern_block! {
        pub(super) mod externs {
            static _start: u8;
        }
    }
}

/// Best-effort sanity check that the linker placed the kernel image
/// where the bootloader handoff promised. Reads one byte from the
/// linker-published `_start` symbol via `read_volatile` — a fault
/// here triple-faults during boot, which is the desired diagnostic.
/// The unsafe `read_volatile` stays inside OSTD; consumers stay safe.
#[inline]
pub fn verify_kernel_entry_alive() {
    let addr = kernel_entry_sym::externs::_start_addr();
    // SAFETY: `_start` is a linker-published byte inside the kernel
    // image; if the image is mapped where the linker said, this
    // single-byte probe is sound. If the image is *not* mapped, the
    // probe page-faults, which is exactly the diagnostic outcome
    // this sanity check exists for.
    let _ = unsafe { core::ptr::read_volatile(addr) };
}
