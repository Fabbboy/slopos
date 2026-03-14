use core::cell::SyncUnsafeCell;
use core::ffi::c_char;

pub type MainFn =
    extern "C" fn(argc: isize, argv: *const *const c_char, envp: *const *const c_char) -> i32;

#[repr(transparent)]
struct SyncCharPtrPtr(*const *const c_char);
unsafe impl Sync for SyncCharPtrPtr {}

static MAIN_FN: SyncUnsafeCell<Option<MainFn>> = SyncUnsafeCell::new(None);
static ARGC: SyncUnsafeCell<isize> = SyncUnsafeCell::new(0);
static ARGV: SyncUnsafeCell<SyncCharPtrPtr> =
    SyncUnsafeCell::new(SyncCharPtrPtr(core::ptr::null()));
static ENVP: SyncUnsafeCell<SyncCharPtrPtr> =
    SyncUnsafeCell::new(SyncCharPtrPtr(core::ptr::null()));

pub fn set_main(main: MainFn) {
    unsafe {
        *MAIN_FN.get() = Some(main);
    }
}

pub fn argc() -> isize {
    unsafe { *ARGC.get() }
}

pub fn argv() -> *const *const c_char {
    unsafe { (*ARGV.get()).0 }
}

pub fn envp() -> *const *const c_char {
    unsafe { (*ENVP.get()).0 }
}

/// # Safety
/// Must be called exactly once from a context where RSP points at the
/// kernel-prepared stack layout (argc at [rsp], argv at [rsp+8], ...).
pub unsafe fn init_from_stack() {
    unsafe {
        use core::arch::asm;

        let sp: u64;
        asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack));

        let stack_ptr = sp as *const u64;

        let raw_argc = *stack_ptr as isize;
        if raw_argc < 0 || raw_argc > 1024 {
            *ARGC.get() = 0;
            (*ARGV.get()).0 = core::ptr::null();
            (*ENVP.get()).0 = core::ptr::null();
            return;
        }

        *ARGC.get() = raw_argc;
        (*ARGV.get()).0 = stack_ptr.add(1) as *const *const c_char;

        let envp_offset = 1 + (raw_argc as usize) + 1;
        (*ENVP.get()).0 = stack_ptr.add(envp_offset) as *const *const c_char;
    }
}

/// # Safety
/// Same RSP requirements as [`init_from_stack`].
pub unsafe fn crt0_start() -> ! {
    use crate::pal::syscall::sys_exit;

    init_from_stack();

    if let Some(main) = *MAIN_FN.get() {
        let ret = main(*ARGC.get(), (*ARGV.get()).0, (*ENVP.get()).0);
        sys_exit(ret);
    } else {
        sys_exit(127);
    }
}

pub fn get_arg(index: usize) -> Option<&'static [u8]> {
    unsafe {
        if index >= (*ARGC.get()) as usize {
            return None;
        }
        let arg_ptr = *(*ARGV.get()).0.add(index);
        if arg_ptr.is_null() {
            return None;
        }
        let mut len = 0;
        while *arg_ptr.add(len) != 0 {
            len += 1;
        }
        Some(core::slice::from_raw_parts(arg_ptr as *const u8, len))
    }
}

pub fn get_env(name: &[u8]) -> Option<&'static [u8]> {
    unsafe {
        if (*ENVP.get()).0.is_null() {
            return None;
        }
        let mut i = 0;
        loop {
            let env_ptr = *(*ENVP.get()).0.add(i);
            if env_ptr.is_null() {
                break;
            }
            let mut len = 0;
            while *env_ptr.add(len) != 0 {
                len += 1;
            }
            let env = core::slice::from_raw_parts(env_ptr as *const u8, len);

            if env.len() > name.len() && env[name.len()] == b'=' {
                if &env[..name.len()] == name {
                    return Some(&env[name.len() + 1..]);
                }
            }
            i += 1;
        }
        None
    }
}
