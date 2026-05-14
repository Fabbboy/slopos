use core::arch::naked_asm;
use core::cell::SyncUnsafeCell;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use crate::cpu::x86_64::pcr::get_current_cpu;

#[repr(C, align(16))]
pub struct JumpBuf {
    pub rbx: u64,
    pub rbp: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rsp: u64,
    pub rip: u64,
}

impl JumpBuf {
    pub const fn zeroed() -> Self {
        Self {
            rbx: 0,
            rbp: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rsp: 0,
            rip: 0,
        }
    }
}

static RECOVERY_ACTIVE: AtomicBool = AtomicBool::new(false);
static RECOVERY_CPU: AtomicUsize = AtomicUsize::new(0);
static RECOVERY_BUF: SyncUnsafeCell<JumpBuf> = SyncUnsafeCell::new(JumpBuf::zeroed());

pub type PanicCleanupFn = fn();

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
    // SAFETY: invoked from catch_panic!'s longjmp tail. The longjmp
    // invalidated every SpinLockGuard the panicking test body held;
    // poison-unlock each tracked entry so registered handlers — and
    // the surrounding KernelTestScope::Drop chain that re-acquires
    // TASK_MANAGER / KERNEL_HEAP / etc. — don't deadlock on a stale
    // ticket. Single-writer: the panicking CPU is the only accessor.
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

#[unsafe(naked)]
pub unsafe extern "C" fn test_setjmp(buf: *mut JumpBuf) -> i32 {
    naked_asm!(
        "mov [rdi], rbx",
        "mov [rdi + 8], rbp",
        "mov [rdi + 16], r12",
        "mov [rdi + 24], r13",
        "mov [rdi + 32], r14",
        "mov [rdi + 40], r15",
        "lea rax, [rsp + 8]",
        "mov [rdi + 48], rax",
        "mov rax, [rsp]",
        "mov [rdi + 56], rax",
        "xor eax, eax",
        "ret",
    )
}

#[unsafe(naked)]
pub unsafe extern "C" fn test_longjmp(buf: *const JumpBuf, val: i32) -> ! {
    naked_asm!(
        "mov eax, esi",
        "test eax, eax",
        "jnz 2f",
        "mov eax, 1",
        "2:",
        "mov rbx, [rdi]",
        "mov rbp, [rdi + 8]",
        "mov r12, [rdi + 16]",
        "mov r13, [rdi + 24]",
        "mov r14, [rdi + 32]",
        "mov r15, [rdi + 40]",
        "mov rsp, [rdi + 48]",
        "jmp [rdi + 56]",
    )
}

pub fn recovery_is_active() -> bool {
    if !RECOVERY_ACTIVE.load(Ordering::SeqCst) {
        return false;
    }
    get_current_cpu() == RECOVERY_CPU.load(Ordering::SeqCst)
}

pub fn recovery_set_active(active: bool) {
    if active {
        RECOVERY_CPU.store(get_current_cpu(), Ordering::SeqCst);
    }
    RECOVERY_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn get_recovery_buf() -> *mut JumpBuf {
    RECOVERY_BUF.get()
}

/// Safe surface around [`test_longjmp`].
///
/// `recovery_is_active()` must have returned `true` before reaching
/// this call — that guarantees `RECOVERY_BUF` was populated by a
/// prior [`test_setjmp`] frame still on the stack. The unsafe
/// `test_longjmp` asm and its raw-pointer dance live inside OSTD;
/// consumers (boot's `#[panic_handler]`) call this safe entry point.
pub fn longjmp_to_recovery(val: i32) -> ! {
    // SAFETY: `RECOVERY_BUF` is a `'static` `SyncUnsafeCell<JumpBuf>`
    // populated by the matching `test_setjmp` in `catch_panic!`. The
    // longjmp restores callee-saved registers and resumes at the
    // setjmp call site; the contract is the same as standard libc
    // setjmp/longjmp and is verified by the calling order
    // `catch_panic!` enforces.
    unsafe {
        test_longjmp(get_recovery_buf(), val);
    }
}

#[macro_export]
macro_rules! catch_panic {
    ($code:block) => {{
        use $crate::panic_recovery::{
            call_panic_cleanup, get_recovery_buf, recovery_set_active, test_setjmp,
        };

        let result = unsafe { test_setjmp(get_recovery_buf()) };

        if result == 0 {
            recovery_set_active(true);
            let ret = (|| -> i32 { $code })();
            recovery_set_active(false);
            ret
        } else {
            call_panic_cleanup();
            -1
        }
    }};
}
