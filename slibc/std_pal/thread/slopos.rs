use crate::ffi::CStr;
use crate::io;
use crate::num::NonZero;
use crate::ptr;
use crate::thread::ThreadInit;
use crate::time::Duration;

unsafe extern "C" {
    fn pthread_create(
        thread: *mut u64,
        attr: *const u8,
        start: unsafe extern "C" fn(*mut u8) -> *mut u8,
        arg: *mut u8,
    ) -> i32;
    fn pthread_join(thread: u64, retval: *mut *mut u8) -> i32;
    fn slopos_sleep_ms(ms: u64);
    fn slopos_yield();
}

pub struct Thread {
    tid: u64,
}

unsafe impl Send for Thread {}
unsafe impl Sync for Thread {}

pub const DEFAULT_MIN_STACK_SIZE: usize = 2 * 1024 * 1024;

impl Thread {
    pub unsafe fn new(stack: usize, init: Box<ThreadInit>) -> io::Result<Thread> {
        let data = Box::into_raw(init);
        let mut tid: u64 = 0;
        let ret = unsafe { pthread_create(&mut tid, ptr::null(), thread_start, data as *mut u8) };

        if ret != 0 {
            unsafe { drop(Box::from_raw(data)) };
            return Err(io::Error::from_raw_os_error(ret));
        }

        return Ok(Thread { tid });

        unsafe extern "C" fn thread_start(data: *mut u8) -> *mut u8 {
            let init = unsafe { Box::from_raw(data as *mut ThreadInit) };
            let rust_start = init.init();
            rust_start();
            unsafe { crate::sys::thread_local::destructors::run() };
            crate::rt::thread_cleanup();
            ptr::null_mut()
        }
    }

    pub fn join(self) {
        unsafe {
            pthread_join(self.tid, ptr::null_mut());
        }
    }
}

pub fn available_parallelism() -> io::Result<NonZero<usize>> {
    // SlopOS is SMP-capable; return 1 as conservative default
    Ok(unsafe { NonZero::new_unchecked(1) })
}

pub fn current_os_id() -> Option<u64> {
    None
}

pub fn yield_now() {
    unsafe { slopos_yield() }
}

pub fn set_name(_name: &CStr) {
    // SlopOS doesn't support thread naming yet
}

pub fn sleep(dur: Duration) {
    let ms = dur.as_millis();
    let ms = if ms > u64::MAX as u128 {
        u64::MAX
    } else if ms == 0 && !dur.is_zero() {
        1u64
    } else {
        ms as u64
    };
    unsafe { slopos_sleep_ms(ms) }
}
