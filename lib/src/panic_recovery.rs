use core::arch::naked_asm;
use core::sync::atomic::{AtomicBool, Ordering};

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
static mut RECOVERY_BUF: JumpBuf = JumpBuf::zeroed();

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
    RECOVERY_ACTIVE.load(Ordering::SeqCst)
}

pub fn recovery_set_active(active: bool) {
    RECOVERY_ACTIVE.store(active, Ordering::SeqCst);
}

pub fn get_recovery_buf() -> *mut JumpBuf {
    &raw mut RECOVERY_BUF
}

#[macro_export]
macro_rules! catch_panic {
    ($code:block) => {{
        use $crate::panic_recovery::{get_recovery_buf, recovery_set_active, test_setjmp};

        let result = unsafe { test_setjmp(get_recovery_buf()) };

        if result == 0 {
            recovery_set_active(true);
            let ret = (|| -> i32 { $code })();
            recovery_set_active(false);
            ret
        } else {
            -1
        }
    }};
}
