#![allow(unsafe_op_in_unsafe_fn)]

use core::ffi::c_char;

pub type MainFn =
    extern "C" fn(argc: isize, argv: *const *const c_char, envp: *const *const c_char) -> i32;

static mut MAIN_FN: Option<MainFn> = None;
static mut ARGC: isize = 0;
static mut ARGV: *const *const c_char = core::ptr::null();
static mut ENVP: *const *const c_char = core::ptr::null();

pub fn set_main(main: MainFn) {
    unsafe {
        MAIN_FN = Some(main);
    }
}

pub fn argc() -> isize {
    unsafe { ARGC }
}

pub fn argv() -> *const *const c_char {
    unsafe { ARGV }
}

pub fn envp() -> *const *const c_char {
    unsafe { ENVP }
}

pub fn crt0_start() -> ! {
    unsafe {
        use super::syscall::sys_exit;
        use core::arch::asm;

        let sp: u64;
        asm!("mov {}, rsp", out(reg) sp, options(nomem, nostack));

        let stack_ptr = sp as *const u64;

        ARGC = *stack_ptr as isize;
        ARGV = stack_ptr.add(1) as *const *const c_char;

        let envp_offset = 1 + (ARGC as usize) + 1;
        ENVP = stack_ptr.add(envp_offset) as *const *const c_char;

        if let Some(main) = MAIN_FN {
            let ret = main(ARGC, ARGV, ENVP);
            sys_exit(ret);
        } else {
            sys_exit(127);
        }
    }
}

#[macro_export]
macro_rules! entry {
    ($main:ident) => {
        #[unsafe(no_mangle)]
        pub extern "C" fn _start() -> ! {
            $crate::libslop::crt0::set_main($main);
            $crate::libslop::crt0::_start()
        }
    };
}

pub fn get_arg(index: usize) -> Option<&'static [u8]> {
    unsafe {
        if index >= ARGC as usize {
            return None;
        }
        let arg_ptr = *ARGV.add(index);
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
        if ENVP.is_null() {
            return None;
        }
        let mut i = 0;
        loop {
            let env_ptr = *ENVP.add(i);
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
