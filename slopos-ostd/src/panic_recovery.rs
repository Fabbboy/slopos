use core::sync::atomic::{AtomicPtr, AtomicU64, AtomicUsize, Ordering};

use crate::unwind::OopsInfo;

pub type PanicCleanupFn = fn();
pub type OopsTaskIdProvider = fn() -> u32;

const MAX_PANIC_CLEANUP_HANDLERS: usize = 8;
static PANIC_CLEANUP_COUNT: AtomicUsize = AtomicUsize::new(0);
static PANIC_CLEANUP_HANDLERS: [AtomicPtr<()>; MAX_PANIC_CLEANUP_HANDLERS] = [
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
    AtomicPtr::new(core::ptr::null_mut()),
];
static OOPS_TASK_ID_PROVIDER: AtomicPtr<()> = AtomicPtr::new(core::ptr::null_mut());

pub fn register_oops_task_id_provider(provider: OopsTaskIdProvider) {
    OOPS_TASK_ID_PROVIDER.store(provider as *mut (), Ordering::Release);
}

pub fn current_oops_task_id() -> u32 {
    let ptr = OOPS_TASK_ID_PROVIDER.load(Ordering::Acquire);
    if ptr.is_null() {
        return u32::MAX;
    }
    // SAFETY: stored only by `register_oops_task_id_provider` from a valid
    // `fn() -> u32` pointer; function pointers are never deallocated.
    let provider: OopsTaskIdProvider = unsafe { core::mem::transmute(ptr) };
    provider()
}

pub fn production_recovery_enabled() -> bool {
    !crate::boot_flags::has_flag(crate::boot_flags::BOOT_FLAG_TESTS_ENABLED)
        && !crate::boot_flags::has_flag(crate::boot_flags::BOOT_FLAG_PANIC_ON_OOPS)
}

/// Recovered panics per boot; the limit-crossing one is fatal. Each recovery
/// may leave non-RAII state skewed (counters, refcounts), so the degradation
/// per boot must be bounded. `0` disables the limit.
const OOPS_LIMIT_DEFAULT: u64 = 100;

/// A recovered-panic count and the budget it is judged against.
pub struct OopsLedger {
    count: AtomicU64,
    limit: AtomicU64,
}

impl OopsLedger {
    pub const fn new(limit: u64) -> Self {
        Self {
            count: AtomicU64::new(0),
            limit: AtomicU64::new(limit),
        }
    }

    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    pub fn limit(&self) -> u64 {
        self.limit.load(Ordering::Relaxed)
    }

    pub fn set_limit(&self, limit: u64) {
        self.limit.store(limit, Ordering::Relaxed);
    }

    pub fn record(&self) -> (u64, bool) {
        let count = self.count.fetch_add(1, Ordering::SeqCst).saturating_add(1);
        let limit = self.limit.load(Ordering::Relaxed);
        (count, limit != 0 && count >= limit)
    }

    #[doc(hidden)]
    pub fn restore(&self, count: u64, limit: u64) {
        self.count.store(count, Ordering::SeqCst);
        self.limit.store(limit, Ordering::SeqCst);
    }
}

/// Counts only panics caught at a production recovery boundary; test-harness
/// catches are expected control flow and never recorded.
static OOPS: OopsLedger = OopsLedger::new(OOPS_LIMIT_DEFAULT);

pub fn oops_count() -> u64 {
    OOPS.count()
}

pub fn oops_limit() -> u64 {
    OOPS.limit()
}

/// Set the recovered-panic budget (`panic.oops_limit=` boot knob); `0`
/// disables the limit.
pub fn set_oops_limit(limit: u64) {
    OOPS.set_limit(limit);
}

/// Record one production oops, returning the post-increment count and whether
/// the configured limit has been reached.
pub fn oops_record() -> (u64, bool) {
    OOPS.record()
}

/// Hermetic-test use only: production code never lowers the count.
#[doc(hidden)]
pub fn restore_oops_ledger(count: u64, limit: u64) {
    OOPS.restore(count, limit);
}

pub fn register_panic_cleanup(handler: PanicCleanupFn) {
    let idx = PANIC_CLEANUP_COUNT.fetch_add(1, Ordering::SeqCst);
    if idx < MAX_PANIC_CLEANUP_HANDLERS {
        PANIC_CLEANUP_HANDLERS[idx].store(handler as *mut (), Ordering::SeqCst);
    }
}

/// Snapshotted at hermetic scope-enter and truncated back to on scope Drop, so
/// the fixed-size array does not fill up across many test runs.
pub fn cleanup_handler_count() -> usize {
    PANIC_CLEANUP_COUNT
        .load(Ordering::SeqCst)
        .min(MAX_PANIC_CLEANUP_HANDLERS)
}

/// Truncate the cleanup-handler list to `count` entries, zeroing the slots
/// above so future registrations start fresh. Bounded-index atomic stores
/// throughout: a racing call clobbers another truncation, nothing more.
pub fn truncate_cleanup_handlers(count: usize) {
    let count = count.min(MAX_PANIC_CLEANUP_HANDLERS);
    PANIC_CLEANUP_COUNT.store(count, Ordering::SeqCst);
    for i in count..MAX_PANIC_CLEANUP_HANDLERS {
        PANIC_CLEANUP_HANDLERS[i].store(core::ptr::null_mut(), Ordering::SeqCst);
    }
}

pub fn call_panic_cleanup() {
    call_panic_cleanup_above(0);
}

/// [`call_panic_cleanup`] scoped to the locks acquired above `held_mark`.
///
/// The *recovered* path, so it deliberately does not enter the fatal bypass:
/// latching there would leave every later acquisition on every CPU
/// unvalidated for the rest of the boot.
pub fn call_panic_cleanup_above(held_mark: u32) {
    // SAFETY: single-writer — the panicking CPU is the only accessor. Rust
    // drops have already released normal guards; the poison-unlock covers
    // legacy paths and partially constructed guards.
    unsafe {
        crate::sync::lock_tracking::poison_unlock_held_above(held_mark);
    }

    let count = PANIC_CLEANUP_COUNT
        .load(Ordering::SeqCst)
        .min(MAX_PANIC_CLEANUP_HANDLERS);
    for i in 0..count {
        let handler = PANIC_CLEANUP_HANDLERS[i].load(Ordering::SeqCst);
        if !handler.is_null() {
            let func: PanicCleanupFn = unsafe { core::mem::transmute(handler) };
            func();
        }
    }
}

pub fn recovery_is_active() -> bool {
    crate::cpu::x86_64::pcr::recovery_depth() != 0
}

pub fn recovery_depth() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth()
}

pub fn recovery_enter() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth_enter()
}

pub fn recovery_exit() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth_exit()
}

#[doc(hidden)]
pub struct RecoveryGuard {
    active: bool,
    /// Held-lock depth on entry, so the unwind releases only what this scope
    /// acquired: draining the whole stack would poison-release locks the outer
    /// frame still holds, and its `Drop` would release them again — on a
    /// ticket lock, two holders.
    held_mark: u32,
}

impl RecoveryGuard {
    pub fn enter() -> Self {
        let held_mark = crate::sync::lock_tracking::held_depth_mark();
        crate::cpu::x86_64::pcr::recovery_depth_enter();
        Self {
            active: true,
            held_mark,
        }
    }

    pub fn held_mark(&self) -> u32 {
        self.held_mark
    }

    pub fn exit(mut self) {
        self.exit_inner();
    }

    fn exit_inner(&mut self) {
        if self.active {
            crate::cpu::x86_64::pcr::recovery_depth_exit();
            self.active = false;
        }
    }
}

impl Drop for RecoveryGuard {
    fn drop(&mut self) {
        self.exit_inner();
    }
}

pub fn run_recoverable<F>(f: F) -> Result<(), OopsInfo>
where
    F: FnOnce(),
{
    let recovery_guard = RecoveryGuard::enter();
    let held_mark = recovery_guard.held_mark();
    let result = crate::unwind::catch_unwind(|| {
        f();
    });
    recovery_guard.exit();

    match result {
        Ok(()) => Ok(()),
        Err(payload) => {
            call_panic_cleanup_above(held_mark);
            Err(payload.info)
        }
    }
}

#[macro_export]
macro_rules! catch_panic {
    ($code:block) => {{
        use $crate::panic_recovery::{RecoveryGuard, call_panic_cleanup_above};

        let recovery_guard = RecoveryGuard::enter();
        let held_mark = recovery_guard.held_mark();
        let result = $crate::unwind::catch_unwind(|| -> i32 { $code });
        recovery_guard.exit();

        match result {
            Ok(ret) => ret,
            Err(_) => {
                call_panic_cleanup_above(held_mark);
                -1
            }
        }
    }};
}
