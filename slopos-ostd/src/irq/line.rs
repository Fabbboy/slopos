//! IRQ vector allocation and callback registration.
//!
//! [`IrqAllocator::alloc`] hands out a typed [`IrqLine`] over the 32..224
//! vector range; [`IrqLine::register_callback`] installs the closure that
//! [`dispatch`] invokes from the production IDT stub.
//!
//! [`CallbackHandle`] borrows its [`IrqLine`], so the line cannot drop while a
//! registration is live; `mem::forget`ing a handle leaks the slot by design
//! rather than risking a use-after-free.

use core::marker::PhantomData;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering};

use crate::KBox;
use crate::sync::BspToken;

/// First allocatable vector. Vectors 0..=31 are CPU exceptions.
pub const ALLOC_VECTOR_BASE: u8 = 32;

/// One past the last allocatable vector. Vectors 224..=255 are
/// reserved for system IPIs (LAPIC timer, reschedule IPI, etc.).
pub const ALLOC_VECTOR_END: u8 = 224;

const ALLOC_RANGE: usize = (ALLOC_VECTOR_END - ALLOC_VECTOR_BASE) as usize;
const BITMAP_WORDS: usize = ALLOC_RANGE.div_ceil(64);

/// Lock-free bitmap; each bit is guarded by a CAS loop on its 64-bit word.
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

    /// Atomically claim a bit; `false` if it was already set.
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

    /// Atomically claim the first clear bit at or after `start_bit`.
    ///
    /// `start_bit` lets `IrqAllocator::alloc()` skip the legacy IOAPIC-pinned
    /// vectors (32..48) so dynamic allocations only land in the MSI range;
    /// lower vectors stay claimable through `IrqAllocator::reserve_specific`.
    fn alloc_from(&self, start_bit: usize) -> Option<usize> {
        for (w_idx, word) in self.words.iter().enumerate() {
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
                    break;
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
            }
        }
        None
    }
}

/// Bit index of vector 48 (MSI_VECTOR_BASE) — the lowest vector
/// `IrqAllocator::alloc()` may hand out; 32..48 are IOAPIC-pinned legacy IRQs,
/// claimable only through `reserve_specific`.
const MSI_RANGE_FIRST_BIT: usize = 16;

struct AllocState {
    /// Bit set = vector held by some `IrqLine` *or* marked platform-reserved.
    allocated: AtomicBitmap,
    /// Bit set = platform-reserved; consulted on `IrqLine` drop so a reserved
    /// bit is never cleared.
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

/// Mark vectors as platform-reserved: [`IrqAllocator::alloc`] will not hand
/// them out, and an `IrqLine` drop will not release them back to the pool.
///
/// Additive and idempotent; vectors outside 32..224 are ignored. The
/// `BspToken` witnesses BSP-only init. Caller invariant (Inv. 3): every listed
/// vector is reserved by the platform's vector layout.
pub fn register_irq_reserved<'brand>(_token: &BspToken<'brand>, reserved: &[u8]) {
    for &v in reserved {
        if let Some(idx) = vector_to_idx(v) {
            ALLOC_STATE.reserved.set(idx);
            ALLOC_STATE.allocated.set(idx);
        }
    }
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrqError {
    /// Allocator pool exhausted (or fully reserved).
    Exhausted,
    /// `register_callback` called twice on the same vector.
    AlreadyRegistered,
    /// Allocator gave out a vector that the dispatch table cannot hold a
    /// closure for.
    AllocFailed,
    /// `dispatch` is suppressed because [`shutdown`] has been called.
    ShuttingDown,
}

/// Read-only context handed to a dispatched callback.
///
/// The lifetime stops the closure retaining it. Sensitive frame state
/// (RIP / RSP / CS / RFLAGS / FS_BASE / GS_BASE) is deliberately unreachable
/// (Inv. 2).
pub struct IrqContext<'a> {
    vector: u8,
    error_code: u64,
    _lifetime: PhantomData<&'a ()>,
}

impl<'a> IrqContext<'a> {
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

static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Mark the dispatch surface as shutting down: [`dispatch`] returns
/// immediately, so teardown does not race in-flight handlers.
pub fn shutdown() {
    SHUTDOWN.store(true, Ordering::Release);
}

/// Dispatch entrypoint for the production IDT stub: invokes the closure
/// registered for `vector`, if any.
///
/// **Restriction:** registered closures must outlive any potential dispatch,
/// and no dispatch may be in flight during deregistration.
pub fn dispatch(vector: u8, error_code: u64) {
    if SHUTDOWN.load(Ordering::Acquire) {
        return;
    }
    let raw = DISPATCH[vector as usize].ptr.load(Ordering::Acquire);
    if raw.is_null() {
        return;
    }
    // SAFETY: `register_callback` published `raw` with Release; we observed
    // it Acquire. The cell lives until either the issuing `CallbackHandle`
    // drops, which nulls the slot before freeing the `KBox`, or the issuing
    // `IrqLine` drops, which the borrow checker forbids while a handle
    // exists. No dispatch may be in flight during deregistration.
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
    // SAFETY: `raw` was extracted by atomic swap; the dispatch path either
    // observed null or, per the module contract, completed before this point.
    unsafe {
        drop(KBox::from_raw(raw));
    }
}

/// ZST gateway over the allocator pool.
pub struct IrqAllocator;

impl IrqAllocator {
    /// Allocate a free vector from the MSI pool (48..224, minus
    /// platform-reserved vectors). Vectors 32..48 stay bound to fixed hardware
    /// lines (PIT timer, PS/2 keyboard/mouse, COM1, …) and are claimable only
    /// via [`IrqAllocator::reserve_specific`].
    pub fn alloc() -> Result<IrqLine, IrqError> {
        let idx = ALLOC_STATE
            .allocated
            .alloc_from(MSI_RANGE_FIRST_BIT)
            .ok_or(IrqError::Exhausted)?;
        Ok(IrqLine {
            vector: ALLOC_VECTOR_BASE + idx as u8,
        })
    }

    /// Claim a specific vector, for hardware-pinned IRQs whose IOAPIC
    /// redirection entry already names it (PS/2 keyboard 33, mouse 44,
    /// COM1 36, …), so the dispatch slot must match.
    ///
    /// [`IrqError::Exhausted`] if `vector` is outside
    /// `ALLOC_VECTOR_BASE..ALLOC_VECTOR_END`;
    /// [`IrqError::AlreadyRegistered`] if the bit is already claimed, including
    /// by [`register_irq_reserved`]. The line's `Drop` releases the bit unless
    /// the vector was platform-reserved.
    pub fn reserve_specific(vector: u8) -> Result<IrqLine, IrqError> {
        let idx = vector_to_idx(vector).ok_or(IrqError::Exhausted)?;
        if ALLOC_STATE.allocated.try_set(idx) {
            Ok(IrqLine { vector })
        } else {
            Err(IrqError::AlreadyRegistered)
        }
    }
}

/// Owned IRQ vector handle; frees the vector back to the allocator on drop.
pub struct IrqLine {
    vector: u8,
}

impl IrqLine {
    /// Vector number this line owns (in 32..224).
    #[inline]
    pub fn vector(&self) -> u8 {
        self.vector
    }

    /// Publish `handler` into this line's dispatch slot; shared body of the
    /// two `register_callback*` entry points, which differ only in the receipt
    /// they return.
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

    /// Install a callback for this line's vector. The returned handle borrows
    /// `self` and deregisters the callback on drop.
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

    /// Install a callback and fold the line plus its registration into one
    /// owned [`OwnedIrq`], with no outstanding borrow to juggle: it can be
    /// stored, moved, or attached to a [`crate::dev::Devres`] bag, and on drop
    /// releases the dispatch slot and then the vector.
    pub fn register_callback_owned<F>(self, handler: F) -> Result<OwnedIrq, IrqError>
    where
        F: Fn(&IrqContext<'_>) + Send + Sync + 'static,
    {
        self.install(handler)?;
        Ok(OwnedIrq { line: self })
    }
}

/// A vector with an installed callback, owned as one RAII unit. Dropping it
/// clears the dispatch slot before freeing the vector bit.
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

/// Receipt for a registered callback. Borrows the issuing [`IrqLine`] so the
/// line cannot drop first; dropping the handle deregisters the callback.
#[must_use = "dropping the handle deregisters the callback"]
pub struct CallbackHandle<'a> {
    vector: u8,
    _phantom: PhantomData<&'a IrqLine>,
}

impl CallbackHandle<'_> {
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

#[cfg(test)]
mod tests {
    use super::*;
    extern crate std;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering as StdOrd};

    fn isolate<R>(f: impl FnOnce() -> R) -> R {
        // Allocator bitmap and BSP mint guard are process-global; serialise
        // against the other global-state test modules.
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
            }
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
        // The exhaustive struct literal is the tripwire: adding RIP / RSP / CS
        // to `IrqContext` breaks it (Inv. 2 leakage).
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
            let line = IrqAllocator::reserve_specific(36).expect("after drop");
            assert_eq!(line.vector(), 36);
        });
    }

    #[test]
    fn reserve_specific_refuses_platform_reserved_vector() {
        isolate(|| {
            // 0x80 (SYSCALL_VECTOR) is the only platform-reserved vector inside
            // the 32..224 pool; the IPI/timer/spurious vectors at 0xEC..0xFF
            // sit outside it, so reserving those is a no-op in the bitmap.
            crate::sync::run_bsp_init_for_test(|t| {
                register_irq_reserved(t, &[0x80]);
            });
            let r = IrqAllocator::reserve_specific(0x80);
            assert_eq!(r.err(), Some(IrqError::AlreadyRegistered));
        });
    }
}
