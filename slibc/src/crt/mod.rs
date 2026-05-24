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

        // Capture the PT_TLS template so spawned threads can build valid TLS
        // blocks (the kernel only sets up the main thread's TLS image).
        crate::thread::tls::capture_tls_template_from_stack(stack_ptr as *const usize);
    }
}

/// Two-stage C runtime startup: parses the stack, initializes environ
/// and stdio, calls main, then performs a clean exit.
///
/// # Safety
/// `main`, `argc`, and `argv` must be valid. `envp` is derived from
/// `argv[argc+1]` per the System V ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __libc_start_main(
    main: MainFn,
    argc: isize,
    argv: *const *const c_char,
) -> ! {
    *MAIN_FN.get() = Some(main);
    *ARGC.get() = argc;
    (*ARGV.get()).0 = argv;

    let envp_ptr = argv.add(argc as usize + 1) as *const *const c_char;
    (*ENVP.get()).0 = envp_ptr;
    crate::env::environ = envp_ptr as *mut *mut u8;
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();

    let ret = main(argc, argv, envp_ptr);
    crate::process::exit(ret)
}

/// Canonical stack-based C-runtime entry. A naked `_start` passes the raw
/// initial stack pointer (`&argc`); this parses argc/argv/envp, captures the
/// program's `PT_TLS` template (via `AT_PHDR`), sets up the main thread's TLS,
/// initializes stdio, then calls `main` and exits. This is the standard
/// `_start -> __libc_start_main` shape; TLS is fully live before `main` runs.
///
/// # Safety
/// `stack_base` must point at the kernel-prepared entry stack (`argc` at
/// `[stack_base]`, then `argv`, `envp`, and the auxv).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __slibc_start(stack_base: *const usize) -> ! {
    unsafe extern "C" {
        fn main(argc: isize, argv: *const *const u8) -> isize;
    }

    let raw_argc = *stack_base as isize;
    let argc = if !(0..=1024).contains(&raw_argc) {
        0
    } else {
        raw_argc
    };
    let argv = stack_base.add(1) as *const *const c_char;
    let envp = stack_base.add(1 + (argc as usize) + 1) as *const *const c_char;

    *ARGC.get() = argc;
    (*ARGV.get()).0 = argv;
    (*ENVP.get()).0 = envp;
    crate::env::environ = envp as *mut *mut u8;

    // TLS before anything that touches a thread-local: capture the template
    // from AT_PHDR, then build + install the main thread's TLS block. `errno`
    // uses its static fallback until this completes.
    crate::thread::tls::capture_tls_template_from_stack(stack_base);
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();

    let ret = main(argc, argv as *const *const u8);
    crate::process::exit(ret as i32)
}

/// # Safety
/// Same RSP requirements as [`init_from_stack`].
pub unsafe fn crt0_start() -> ! {
    init_from_stack();

    let argc = *ARGC.get();
    let argv = (*ARGV.get()).0;
    let envp = (*ENVP.get()).0;

    crate::env::environ = envp as *mut *mut u8;
    crate::thread::tls::tls_init_main_thread();
    crate::stdio::streams::stdio_init();

    if let Some(main) = *MAIN_FN.get() {
        let ret = main(argc, argv, envp);
        crate::process::exit(ret);
    } else {
        crate::process::_exit(127);
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
