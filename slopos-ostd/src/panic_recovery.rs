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

/// Default budget of recovered production panics per boot. Reaching the
/// limit makes the limit-crossing panic fatal: every recovered panic may
/// leave non-RAII kernel state skewed (counters, refcounts), so the total
/// degradation per boot must be bounded. `0` disables the limit.
const OOPS_LIMIT_DEFAULT: u64 = 100;

/// Recovered-production-panic ledger. Counts only panics caught at a
/// production recovery boundary; test-harness catches are expected control
/// flow and never recorded.
static OOPS_COUNT: AtomicU64 = AtomicU64::new(0);
static OOPS_LIMIT: AtomicU64 = AtomicU64::new(OOPS_LIMIT_DEFAULT);

pub fn oops_count() -> u64 {
    OOPS_COUNT.load(Ordering::Relaxed)
}

pub fn oops_limit() -> u64 {
    OOPS_LIMIT.load(Ordering::Relaxed)
}

/// Set the recovered-panic budget (`panic.oops_limit=` boot knob).
/// `0` disables the limit.
pub fn set_oops_limit(limit: u64) {
    OOPS_LIMIT.store(limit, Ordering::Relaxed);
}

/// Record one production oops. Returns the post-increment running count
/// and whether the configured limit has been reached.
pub fn oops_record() -> (u64, bool) {
    let count = OOPS_COUNT.fetch_add(1, Ordering::SeqCst).saturating_add(1);
    let limit = OOPS_LIMIT.load(Ordering::Relaxed);
    (count, limit != 0 && count >= limit)
}

/// Restore the ledger to a snapshot. Hermetic-test use only: production
/// code never lowers the count.
#[doc(hidden)]
pub fn restore_oops_ledger(count: u64, limit: u64) {
    OOPS_COUNT.store(count, Ordering::SeqCst);
    OOPS_LIMIT.store(limit, Ordering::SeqCst);
}

pub fn register_panic_cleanup(handler: PanicCleanupFn) {
    let idx = PANIC_CLEANUP_COUNT.fetch_add(1, Ordering::SeqCst);
    if idx < MAX_PANIC_CLEANUP_HANDLERS {
        PANIC_CLEANUP_HANDLERS[idx].store(handler as *mut (), Ordering::SeqCst);
    }
}

/// Number of cleanup handlers currently registered. Used by the
/// hermetic-state framework to snapshot the registration count at
/// scope-enter and truncate-to-snapshot on Drop, preventing the
/// fixed-size array from filling up across many test runs.
pub fn cleanup_handler_count() -> usize {
    PANIC_CLEANUP_COUNT
        .load(Ordering::SeqCst)
        .min(MAX_PANIC_CLEANUP_HANDLERS)
}

/// Truncate the cleanup-handler list to `count` entries. Slots from
/// `count` onward are zeroed so future registrations start fresh.
///
/// All operations are bounded-index atomic stores — sound regardless
/// of caller context. Hermetic-state framework calls this from
/// `KernelTestScope::Drop` with APs paused, which is the intended
/// use site, but a racing call would at worst clobber another caller's
/// truncation, not violate memory safety.
pub fn truncate_cleanup_handlers(count: usize) {
    let count = count.min(MAX_PANIC_CLEANUP_HANDLERS);
    PANIC_CLEANUP_COUNT.store(count, Ordering::SeqCst);
    for i in count..MAX_PANIC_CLEANUP_HANDLERS {
        PANIC_CLEANUP_HANDLERS[i].store(core::ptr::null_mut(), Ordering::SeqCst);
    }
}

pub fn call_panic_cleanup() {
    // SAFETY: invoked after catch_panic! catches a kernel-test unwind.
    // Rust Drops should already have released normal guards; poison-unlock
    // remains as a defensive cleanup for legacy paths and partially
    // constructed lock guards. Single-writer: the panicking CPU is the only
    // accessor.
    unsafe {
        crate::sync::lock_tracking::poison_unlock_all_held();
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

/// True when the current execution context is inside at least one
/// panic-recovery scope.
pub fn recovery_is_active() -> bool {
    crate::cpu::x86_64::pcr::recovery_depth() != 0
}

/// Current panic-recovery nesting depth for this execution context.
pub fn recovery_depth() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth()
}

/// Enter a panic-recovery scope in the current execution context.
pub fn recovery_enter() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth_enter()
}

/// Leave a panic-recovery scope in the current execution context.
pub fn recovery_exit() -> u32 {
    crate::cpu::x86_64::pcr::recovery_depth_exit()
}

#[doc(hidden)]
pub struct RecoveryGuard {
    active: bool,
}

impl RecoveryGuard {
    pub fn enter() -> Self {
        crate::cpu::x86_64::pcr::recovery_depth_enter();
        Self { active: true }
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
    let result = crate::unwind::catch_unwind(|| {
        f();
    });
    recovery_guard.exit();

    match result {
        Ok(()) => Ok(()),
        Err(payload) => {
            call_panic_cleanup();
            Err(payload.info)
        }
    }
}

#[macro_export]
macro_rules! catch_panic {
    ($code:block) => {{
        use $crate::panic_recovery::{RecoveryGuard, call_panic_cleanup};

        let recovery_guard = RecoveryGuard::enter();
        let result = $crate::unwind::catch_unwind(|| -> i32 { $code });
        recovery_guard.exit();

        match result {
            Ok(ret) => ret,
            Err(_) => {
                call_panic_cleanup();
                -1
            }
        }
    }};
}
