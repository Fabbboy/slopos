use core::arch::naked_asm;
use core::sync::atomic::{AtomicBool, AtomicPtr, AtomicUsize, Ordering};

use crate::percpu::get_current_cpu;

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
static mut RECOVERY_BUF: JumpBuf = JumpBuf::zeroed();

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

pub fn call_panic_cleanup() {
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
    &raw mut RECOVERY_BUF
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
