//! IRQ vector allocation and callback registration.
//!
//! [`IrqAllocator::alloc`] hands out a typed [`IrqLine`] over the
//! 32..224 vector range; [`IrqLine::register_callback`] installs a
//! `Fn(&IrqContext<'_>) + Send + Sync + 'static` closure for that
//! vector. [`dispatch`] is the public entrypoint the production IDT
//! stub jumps into — it loads the registered closure (if any) and
//! invokes it.
//!
//! # `IrqContext` keeps sensitive frame state private
//!
//! The context exposes only `vector` and `error_code`. RIP / RSP /
//! CS / RFLAGS are deliberately *not* reachable: those are sensitive
//! kernel-mode CPU state (Inv. 2) and must not flow to driver-supplied
//! callbacks.
//!
//! # Lifetime gating between [`IrqLine`] and [`CallbackHandle`]
//!
//! [`CallbackHandle`] borrows the [`IrqLine`] it was issued from
//! (`CallbackHandle<'a>` carries `PhantomData<&'a IrqLine>`). The
//! borrow checker therefore guarantees the line outlives the handle:
//! the line's `Drop` cannot run while a handle still holds a
//! pointer-to-vector mapping for the same vector. Forgotten handles
//! (`mem::forget`) leak the slot — by design; the leaked closure
//! lives forever and the vector remains un-re-registerable, which is
//! a soundness-preserving leak rather than a use-after-free.
//!
//! # Soundness
//!
//! Inv. 2 (kernel state untamperable by OSTD clients): `IrqContext`
//! shape; `register_callback` `Send + Sync + 'static` bound. Inv. 3
//! (peripheral untamperability): vectors only allocate from the
//! IOMMU-remap-friendly 32..224 range; reserved-vector list is
//! configured at boot.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use crate::KBox;
use crate::sync::BspToken;

// ---------------------------------------------------------------------------
// Vector range and allocator state.
// ---------------------------------------------------------------------------

/// First allocatable vector. Vectors 0..=31 are CPU exceptions.
pub const ALLOC_VECTOR_BASE: u8 = 32;

/// One past the last allocatable vector. Vectors 224..=255 are
/// reserved for system IPIs (LAPIC timer, reschedule IPI, etc.).
pub const ALLOC_VECTOR_END: u8 = 224;

const ALLOC_RANGE: usize = (ALLOC_VECTOR_END - ALLOC_VECTOR_BASE) as usize;
const BITMAP_WORDS: usize = ALLOC_RANGE.div_ceil(64);

/// Simple lock-free bitmap: each bit guarded by its own CAS loop on
/// the containing 64-bit word. `set` / `clear` / `test` / `alloc` /
/// `free` are the only operations the allocator needs.
struct AtomicBitmap {
    words: [AtomicU64; BITMAP_WORDS],
}

impl AtomicBitmap {
    const fn new() -> Self {
        Self {
            words: [const { AtomicU64::new(0) }; BITMAP_WORDS],
        }
    }

    #[inline]
    fn test(&self, bit: usize) -> bool {
        if bit >= ALLOC_RANGE {
            return false;
        }
        let w = bit >> 6;
        let mask = 1u64 << (bit & 63);
        self.words[w].load(Ordering::Acquire) & mask != 0
    }

    #[inline]
    fn set(&self, bit: usize) {
        if bit >= ALLOC_RANGE {
            return;
        }
        let w = bit >> 6;
        let mask = 1u64 << (bit & 63);
        self.words[w].fetch_or(mask, Ordering::AcqRel);
    }

    #[inline]
    fn clear(&self, bit: usize) {
        if bit >= ALLOC_RANGE {
            return;
        }
        let w = bit >> 6;
        let mask = 1u64 << (bit & 63);
        self.words[w].fetch_and(!mask, Ordering::AcqRel);
    }

    /// Atomically claim a specific bit. Returns `true` if the bit was
    /// clear and is now set; `false` if the bit was already set.
    fn try_set(&self, bit: usize) -> bool {
        if bit >= ALLOC_RANGE {
            return false;
        }
        let w = bit >> 6;
        let mask = 1u64 << (bit & 63);
        loop {
            let cur = self.words[w].load(Ordering::Acquire);
            if cur & mask != 0 {
                return false;
            }
            if self.words[w]
                .compare_exchange(cur, cur | mask, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                return true;
            }
        }
    }

    /// Atomically claim the first clear bit in `start_bit..ALLOC_RANGE`.
    /// Uses a CAS retry loop per word; returns the bit index or `None`
    /// if no clear bits exist in the requested range.
    ///
    /// `start_bit` lets `IrqAllocator::alloc()` skip the legacy
    /// IOAPIC-pinned vectors (32..48) so dynamic allocations only
    /// land in the MSI range (48..224). Vectors below `start_bit`
    /// remain claimable through `IrqAllocator::reserve_specific`.
    fn alloc_from(&self, start_bit: usize) -> Option<usize> {
        for (w_idx, word) in self.words.iter().enumerate() {
            // Mask out bits below `start_bit` so trailing_zeros never
            // picks one. For words entirely below start_bit, this
            // makes the inverted value zero so the loop falls through.
            let word_start = w_idx * 64;
            let local_skip = if start_bit > word_start {
                start_bit - word_start
            } else {
                0
            };
            if local_skip >= 64 {
                continue;
            }
            let skip_mask: u64 = if local_skip == 0 {
                0
            } else {
                (1u64 << local_skip) - 1
            };
            loop {
                let cur = word.load(Ordering::Acquire);
                let inv = !(cur | skip_mask);
                if inv == 0 {
                    break; // no clear bits in this word at or after start_bit
                }
                let bit_in_word = inv.trailing_zeros() as usize;
                let bit = word_start + bit_in_word;
                if bit >= ALLOC_RANGE {
                    break;
                }
                let mask = 1u64 << bit_in_word;
                if word
                    .compare_exchange(cur, cur | mask, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    return Some(bit);
                }
                // Lost race; retry this word.
            }
        }
        None
    }
}

/// Bit index corresponding to vector 48 (MSI_VECTOR_BASE) — the lowest
/// vector `IrqAllocator::alloc()` is allowed to hand out. Vectors below
/// this (32..48) are reserved for IOAPIC-pinned legacy IRQs and must be
/// claimed through `reserve_specific`.
const MSI_RANGE_FIRST_BIT: usize = 16;

struct AllocState {
    /// Bit set = vector currently held by some `IrqLine` *or* marked
    /// platform-reserved. The allocator scans for the first clear bit.
    allocated: AtomicBitmap,
    /// Bit set = platform-reserved (see [`register_irq_reserved`]).
    /// `free` consults this bitmap to avoid clearing a reserved bit
    /// when an `IrqLine` for an allocated vector drops.
    reserved: AtomicBitmap,
}

static ALLOC_STATE: AllocState = AllocState {
    allocated: AtomicBitmap::new(),
    reserved: AtomicBitmap::new(),
};

#[inline]
fn vector_to_idx(vector: u8) -> Option<usize> {
    if vector < ALLOC_VECTOR_BASE || vector >= ALLOC_VECTOR_END {
        return None;
    }
    Some((vector - ALLOC_VECTOR_BASE) as usize)
}

/// Mark a set of vectors as platform-reserved. Subsequent
/// [`IrqAllocator::alloc`] calls will not return them; calling code
/// drops on a reserved vector are no-ops in the bitmap (so a
/// boot-time `IrqLine` constructed via internal API for a reserved
/// vector — should one ever exist — does not free it back to the
/// pool by mistake).
///
/// Additive: multiple calls accumulate. Vectors outside the
/// 32..224 range are silently ignored. Idempotent. The
/// `&BspToken<'brand>` witnesses BSP-only init; every vector in
/// `reserved` must be reserved by the platform's vector layout
/// (LAPIC timer, SYSCALL_VECTOR, IPIs, spurious vector, etc.) —
/// Inv. 3, a caller invariant.
pub fn register_irq_reserved<'brand>(_token: &BspToken<'brand>, reserved: &[u8]) {
    for &v in reserved {
        if let Some(idx) = vector_to_idx(v) {
            ALLOC_STATE.reserved.set(idx);
            ALLOC_STATE.allocated.set(idx);
        }
    }
}

/// Test-only reset hook.
#[cfg(any(test, feature = "test-helpers"))]
pub fn reset_for_test() {
    for word in ALLOC_STATE.allocated.words.iter() {
        word.store(0, Ordering::Release);
    }
    for word in ALLOC_STATE.reserved.words.iter() {
        word.store(0, Ordering::Release);
    }
    for slot in DISPATCH.iter() {
        let raw = slot.ptr.swap(ptr::null_mut(), Ordering::AcqRel);
        if !raw.is_null() {
            // SAFETY: we own raw via swap; nobody else reads it now.
            unsafe {
                drop(KBox::from_raw(raw));
            }
        }
    }
    SHUTDOWN.store(false, Ordering::Release);
}

// ---------------------------------------------------------------------------
// Errors.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    /// Allocator pool exhausted (or fully reserved).
    Exhausted,
    /// `register_callback` called twice on the same vector.
    AlreadyRegistered,
    /// Allocator gave out a vector that the dispatch table cannot
    /// hold a closure for — only ever returned for a runtime test of
    /// the allocator failing-`KBox::try_new` invariant.
    AllocFailed,
    /// `dispatch` is suppressed because [`shutdown`] has been called.
    ShuttingDown,
}

// ---------------------------------------------------------------------------
// IrqContext.
// ---------------------------------------------------------------------------

/// Read-only context handed to a dispatched callback.
///
/// Lifetime parameter ties the borrow to the dispatch invocation:
/// the closure may not retain the context. Sensitive frame state
/// (RIP / RSP / CS / RFLAGS / FS_BASE / GS_BASE) is *not* reachable.
pub struct IrqContext<'a> {
    vector: u8,
    error_code: u64,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> IrqContext<'a> {
    /// Vector that fired.
    #[inline]
    pub fn vector(&self) -> u8 {
        self.vector
    }

    /// Error code pushed by the CPU. Meaningful for vectors that
    /// push one (#DF=8, #TS=10, #NP=11, #SS=12, #GP=13, #PF=14,
    /// #AC=17, #VE=20, #CP=21, #SX=30); zero for the rest.
    #[inline]
    pub fn error_code(&self) -> u64 {
        self.error_code
    }
}

// ---------------------------------------------------------------------------
// Dispatch table.
// ---------------------------------------------------------------------------

struct HandlerCell {
    callback: KBox<dyn Fn(&IrqContext<'_>) + Send + Sync + 'static>,
}

#[repr(transparent)]
struct DispatchSlot {
    ptr: AtomicPtr<HandlerCell>,
}

impl DispatchSlot {
    const fn new() -> Self {
        Self {
            ptr: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

static DISPATCH: [DispatchSlot; 256] = [const { DispatchSlot::new() }; 256];

/// Set after [`shutdown`] is called. Subsequent `dispatch` calls are
/// no-ops; this lets an orderly shutdown drain in-flight handlers
/// without racing teardown.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Mark the dispatch surface as shutting down — subsequent calls to
/// [`dispatch`] return immediately. Useful when the kernel is about
/// to tear down all `IrqLine` instances.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

/// Dispatch entrypoint. Called by the production IDT stub. Loads the
/// registered closure for `vector` (if any) and invokes it.
///
/// **Restriction:** registered closures must outlive any potential
/// dispatch. Production wiring registers at boot and only deregisters
/// at panic/shutdown; integration tests deregister handlers before
/// tearing down the test harness.
pub fn dispatch(vector: u8, error_code: u64) {
    if SHUTDOWN.load(Ordering::Acquire) {
        return;
    }
    let raw = DISPATCH[vector as usize].ptr.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: `register_callback` published `raw` with Release; we
    // observed it Acquire. The handler cell is alive until either
    // (a) the issuing `CallbackHandle` drops, which swaps the slot
    // back to null *before* dropping the `KBox<HandlerCell>`, or
    // (b) the issuing `IrqLine` drops, which the borrow checker
    // forbids while a handle exists. The module contract requires
    // that no dispatch is in flight during deregistration.
    let cell = unsafe { &*raw };
    let ctx = IrqContext {
        vector,
        error_code,
        _lifetime: PhantomData,
    };
    (cell.callback)(&ctx);
}

#[inline]
fn clear_dispatch(vector: u8) {
    let raw = DISPATCH[vector as usize]
        .ptr
        .swap(ptr::null_mut(), Ordering::AcqRel);
    if raw.is_null() {
        return;
    }
    // SAFETY: we extracted `raw` by atomic swap; the dispatch path
    // either observed null already (no-op) or observed `raw` and is
    // required by the module contract to have completed before this
    // point.
    unsafe {
        drop(KBox::from_raw(raw));
    }
}

// ---------------------------------------------------------------------------
// IrqAllocator + IrqLine + CallbackHandle.
// ---------------------------------------------------------------------------

/// ZST gateway over the allocator pool. Only callable surface is
/// [`IrqAllocator::alloc`].
pub struct IrqAllocator;

impl IrqAllocator {
    /// Allocate a free vector from the MSI pool (48..224, excluding
    /// platform-reserved vectors). Vectors 32..48 are reserved for
    /// legacy IOAPIC-pinned IRQs and only claimable via
    /// [`IrqAllocator::reserve_specific`] — virtio MSI/MSI-X programming
    /// requires vectors in 48+ because the lower range is bound to
    /// fixed hardware lines (PIT timer, PS/2 keyboard/mouse, COM1, …).
    ///
    /// Returns [`IrqError::Exhausted`] if no clear bits remain in the
    /// MSI range.
    pub fn alloc() -> Result<IrqLine, IrqError> {
        let idx = ALLOC_STATE
            .allocated
            .alloc_from(MSI_RANGE_FIRST_BIT)
            .ok_or(IrqError::Exhausted)?;
        Ok(IrqLine {
            vector: ALLOC_VECTOR_BASE + idx as u8,
        })
    }

    /// Claim a specific vector. Used for hardware-pinned IRQs whose
    /// IDT slot is fixed by the platform — typically legacy IOAPIC
    /// IRQs (PS/2 keyboard at 33, mouse at 44, COM1 at 36, …) where
    /// the IOAPIC redirection-table entry already points at this
    /// vector, so the dispatch slot must match.
    ///
    /// Returns:
    /// - [`IrqError::Exhausted`] if `vector` is outside the
    ///   `ALLOC_VECTOR_BASE..ALLOC_VECTOR_END` range.
    /// - [`IrqError::AlreadyRegistered`] if the bit is already claimed
    ///   (by a prior `alloc`, a prior `reserve_specific`, *or* by a
    ///   platform-reserved vector via [`register_irq_reserved`]).
    /// - `Ok(IrqLine)` on success. The line's `Drop` releases the
    ///   bitmap bit unless the vector was platform-reserved (in which
    ///   case the reservation persists, matching `alloc`'s drop path).
    pub fn reserve_specific(vector: u8) -> Result<IrqLine, IrqError> {
        let idx = vector_to_idx(vector).ok_or(IrqError::Exhausted)?;
        if ALLOC_STATE.allocated.try_set(idx) {
            Ok(IrqLine { vector })
        } else {
            Err(IrqError::AlreadyRegistered)
        }
    }
}

/// Owned IRQ vector handle. Frees the vector back to the allocator
/// on drop. Construction is gated by [`IrqAllocator::alloc`].
pub struct IrqLine {
    vector: u8,
}

impl IrqLine {
    /// Vector number this line owns (in 32..224).
    #[inline]
    pub fn vector(&self) -> u8 {
        self.vector
    }

    /// Publish `handler` into this line's dispatch slot. Shared body of
    /// [`register_callback`](Self::register_callback) and
    /// [`register_callback_owned`](Self::register_callback_owned); the
    /// only difference between those two is the receipt they return
    /// (borrowed handle vs. owned line), so the slot-population CAS lives
    /// here once.
    fn install<F>(&self, handler: F) -> Result<(), IrqError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        let inner = KBox::try_new(handler).map_err(|_| IrqError::AllocFailed)?;
        let cell =
            KBox::try_new(HandlerCell { callback: inner }).map_err(|_| IrqError::AllocFailed)?;
        let raw = KBox::into_raw(cell);
        match DISPATCH[self.vector as usize].ptr.compare_exchange(
            ptr::null_mut(),
            raw,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => Ok(()),
            Err(_) => {
                // SAFETY: we never published `raw`.
                unsafe {
                    drop(KBox::from_raw(raw));
                }
                Err(IrqError::AlreadyRegistered)
            }
        }
    }

    /// Install a callback for this line's vector. Returns a handle
    /// that, on drop, deregisters the callback. The handle borrows
    /// `self` so it cannot outlive the line.
    pub fn register_callback<'a, F>(&'a self, handler: F) -> Result<CallbackHandle<'a>, IrqError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        self.install(handler)?;
        Ok(CallbackHandle {
            vector: self.vector,
            _phantom: PhantomData,
        })
    }

    /// Install a callback and fold the line + its registration into a
    /// single owned [`OwnedIrq`]. Consumes `self`, so there is no
    /// outstanding borrow to juggle: the resource can be stored, moved,
    /// or attached to a [`crate::dev::Devres`] bag, and releases both the
    /// dispatch slot and the vector when dropped (in that order). This
    /// is the leak-free replacement for the `register_callback` +
    /// `mem::forget(handle); mem::forget(line)` pattern.
    pub fn register_callback_owned<F>(self, handler: F) -> Result<OwnedIrq, IrqError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        self.install(handler)?;
        Ok(OwnedIrq { line: self })
    }
}

/// A vector with an installed callback, owned as one RAII unit.
///
/// Holds the [`IrqLine`] by value; the dispatch slot is populated for
/// the lifetime of this value. Dropping it runs [`IrqLine`]'s `Drop`,
/// which clears the dispatch slot (releasing the boxed closure) *before*
/// freeing the vector bit — the correct handle-before-line teardown
/// order, with no `mem::forget` and no self-referential borrow.
pub struct OwnedIrq {
    line: IrqLine,
}

impl OwnedIrq {
    /// Vector number this binding owns (in 32..224).
    #[inline]
    pub fn vector(&self) -> u8 {
        self.line.vector()
    }
}

impl Drop for IrqLine {
    fn drop(&mut self) {
        clear_dispatch(self.vector);
        if let Some(idx) = vector_to_idx(self.vector) {
            if !ALLOC_STATE.reserved.test(idx) {
                ALLOC_STATE.allocated.clear(idx);
            }
        }
    }
}

/// Receipt for a registered callback. Borrows the [`IrqLine`] it was
/// issued from so the line cannot drop first. Dropping the handle
/// deregisters the callback.
#[must_use = "dropping the handle deregisters the callback"]
pub struct CallbackHandle<'a> {
    vector: u8,
    _phantom: PhantomData<&'a IrqLine>,
}

impl CallbackHandle<'_> {
    /// Vector number whose dispatch slot this handle controls.
    #[inline]
    pub fn vector(&self) -> u8 {
        self.vector
    }
}

impl Drop for CallbackHandle<'_> {
    fn drop(&mut self) {
        clear_dispatch(self.vector);
    }
}

// ---------------------------------------------------------------------------
// Lib unit tests (host-side).
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrd};

    fn isolate<R>(f: impl FnOnce() -> R) -> R {
        // The allocator bitmap and the BSP mint guard (via
        // `run_bsp_init_for_test` in some bodies) are process-global;
        // serialise against the other global-state test modules.
        let _g = crate::test_support::global_lock::lock_global_test_state();
        reset_for_test();
        let r = f();
        reset_for_test();
        r
    }

    #[test]
    fn alloc_returns_vector_in_range() {
        isolate(|| {
            let l = IrqAllocator::alloc().expect("alloc");
            assert!(l.vector() >= ALLOC_VECTOR_BASE);
            assert!(l.vector() < ALLOC_VECTOR_END);
        });
    }

    #[test]
    fn alloc_distinct_vectors() {
        isolate(|| {
            let a = IrqAllocator::alloc().expect("a");
            let b = IrqAllocator::alloc().expect("b");
            assert_ne!(a.vector(), b.vector());
        });
    }

    #[test]
    fn drop_returns_vector_to_pool() {
        isolate(|| {
            let v = {
                let l = IrqAllocator::alloc().expect("alloc");
                l.vector()
            };
            // Drain everything else and check `v` is reachable.
            let mut others = std::vec::Vec::new();
            loop {
                match IrqAllocator::alloc() {
                    Ok(line) => {
                        if line.vector() == v {
                            return;
                        }
                        others.push(line);
                    }
                    Err(IrqError::Exhausted) => panic!("never saw freed vector"),
                    Err(other) => panic!("unexpected {:?}", other),
                }
            }
        });
    }

    #[test]
    fn reserved_vector_is_skipped() {
        isolate(|| {
            crate::sync::run_bsp_init_for_test(|t| {
                register_irq_reserved(t, &[ALLOC_VECTOR_BASE]);
            });
            for _ in 0..3 {
                let l = IrqAllocator::alloc().expect("alloc");
                assert_ne!(l.vector(), ALLOC_VECTOR_BASE);
            }
        });
    }

    #[test]
    fn register_callback_then_dispatch() {
        isolate(|| {
            let line = IrqAllocator::alloc().expect("alloc");
            let v = line.vector();
            let counter = Arc::new(AtomicUsize::new(0));
            let c2 = counter.clone();
            let _h = line
                .register_callback(move |ctx: &IrqContext<'_>| {
                    assert_eq!(ctx.vector(), v);
                    assert_eq!(ctx.error_code(), 0x42);
                    c2.fetch_add(1, StdOrd::Relaxed);
                })
                .expect("register");
            dispatch(v, 0x42);
            dispatch(v, 0x42);
            assert_eq!(counter.load(StdOrd::Relaxed), 2);
        });
    }

    #[test]
    fn double_register_errors() {
        isolate(|| {
            let line = IrqAllocator::alloc().expect("alloc");
            let _h = line.register_callback(|_| {}).expect("first");
            let r = line.register_callback(|_| {});
            assert_eq!(r.err(), Some(IrqError::AlreadyRegistered));
        });
    }

    #[test]
    fn dropping_handle_clears_dispatch() {
        isolate(|| {
            let line = IrqAllocator::alloc().expect("alloc");
            let v = line.vector();
            let counter = Arc::new(AtomicUsize::new(0));
            {
                let c2 = counter.clone();
                let _h = line
                    .register_callback(move |_ctx| {
                        c2.fetch_add(1, StdOrd::Relaxed);
                    })
                    .expect("register");
            } // handle drops here
            dispatch(v, 0);
            assert_eq!(counter.load(StdOrd::Relaxed), 0);
        });
    }

    #[test]
    fn shutdown_suppresses_dispatch() {
        isolate(|| {
            let line = IrqAllocator::alloc().expect("alloc");
            let v = line.vector();
            let counter = Arc::new(AtomicUsize::new(0));
            let c2 = counter.clone();
            let _h = line
                .register_callback(move |_| {
                    c2.fetch_add(1, StdOrd::Relaxed);
                })
                .expect("register");
            shutdown();
            dispatch(v, 0);
            assert_eq!(counter.load(StdOrd::Relaxed), 0);
        });
    }

    #[test]
    fn irq_context_does_not_expose_frame_fields() {
        // Compile-time check via field count: `IrqContext` has only
        // vector + error_code + lifetime. Anyone adding RIP / RSP /
        // CS would have to bump this assertion, which serves as a
        // tripwire for Inv. 2 leakage.
        let ctx = IrqContext {
            vector: 13,
            error_code: 0,
            _lifetime: PhantomData,
        };
        assert_eq!(ctx.vector(), 13);
        assert_eq!(ctx.error_code(), 0);
    }

    #[test]
    fn irq_error_eq() {
        assert_eq!(IrqError::Exhausted, IrqError::Exhausted);
        assert_ne!(IrqError::Exhausted, IrqError::AlreadyRegistered);
    }

    #[test]
    fn vector_to_idx_bounds() {
        assert_eq!(vector_to_idx(0), None);
        assert_eq!(vector_to_idx(31), None);
        assert_eq!(vector_to_idx(32), Some(0));
        assert_eq!(vector_to_idx(223), Some(191));
        assert_eq!(vector_to_idx(224), None);
        assert_eq!(vector_to_idx(0xFF), None);
    }

    #[test]
    fn reserve_specific_claims_legacy_irq_vector() {
        isolate(|| {
            let line = IrqAllocator::reserve_specific(33).expect("legacy IRQ1");
            assert_eq!(line.vector(), 33);
        });
    }

    #[test]
    fn reserve_specific_double_claim_refused() {
        isolate(|| {
            let _line = IrqAllocator::reserve_specific(44).expect("first claim");
            let r = IrqAllocator::reserve_specific(44);
            assert_eq!(r.err(), Some(IrqError::AlreadyRegistered));
        });
    }

    #[test]
    fn reserve_specific_out_of_range_refused() {
        isolate(|| {
            assert_eq!(
                IrqAllocator::reserve_specific(0).err(),
                Some(IrqError::Exhausted)
            );
            assert_eq!(
                IrqAllocator::reserve_specific(31).err(),
                Some(IrqError::Exhausted)
            );
            assert_eq!(
                IrqAllocator::reserve_specific(224).err(),
                Some(IrqError::Exhausted)
            );
            assert_eq!(
                IrqAllocator::reserve_specific(0xFF).err(),
                Some(IrqError::Exhausted)
            );
        });
    }

    #[test]
    fn reserve_specific_drop_releases_vector() {
        isolate(|| {
            {
                let _line = IrqAllocator::reserve_specific(36).expect("first");
            }
            // Should be claimable again after drop.
            let line = IrqAllocator::reserve_specific(36).expect("after drop");
            assert_eq!(line.vector(), 36);
        });
    }

    #[test]
    fn reserve_specific_refuses_platform_reserved_vector() {
        isolate(|| {
            // Mark vector 0x80 (SYSCALL_VECTOR) as platform-reserved.
            // It's the one reserved vector that lives inside the
            // allocator's 32..224 pool — the IPI/timer/spurious vectors
            // sit at 0xEC..0xFF, outside the pool entirely, so reserving
            // them is a no-op in the bitmap.
            crate::sync::run_bsp_init_for_test(|t| {
                register_irq_reserved(t, &[0x80]);
            });
            let r = IrqAllocator::reserve_specific(0x80);
            assert_eq!(r.err(), Some(IrqError::AlreadyRegistered));
        });
    }
}
