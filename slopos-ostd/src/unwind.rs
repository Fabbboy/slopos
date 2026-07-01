//! Kernel-facing Rust unwind facade.
//!
//! This is the only public kernel surface over the `unwinding` crate. It keeps
//! panic payload allocation, catch boundaries, and fixed-buffer DWARF backtrace
//! capture inside OSTD.
//!
//! Unwinding is kernel-internal only. It must not cross user/kernel entry,
//! interrupt/trap stubs, scheduler context switches, or other non-Rust ABI
//! boundaries; those paths must catch before the boundary or abort.

use core::convert::Infallible;
use core::panic::PanicInfo;

#[cfg(feature = "kernel-unwind")]
use alloc::boxed::Box;
#[cfg(feature = "kernel-unwind")]
use core::ffi::c_void;
#[cfg(feature = "kernel-unwind")]
use core::mem::MaybeUninit;

#[cfg(feature = "kernel-unwind")]
use unwinding::abi::{
    _Unwind_Backtrace, _Unwind_GetIP, _Unwind_RaiseException, UnwindContext, UnwindException,
    UnwindReasonCode,
};
#[cfg(feature = "kernel-unwind")]
use unwinding::panicking::{self, Exception};

#[derive(Debug)]
pub struct KernelPanic;

pub const UNWIND_BACKTRACE_MAX: usize = 16;

#[derive(Clone, Copy)]
pub struct UnwindBacktrace {
    frames: [u64; UNWIND_BACKTRACE_MAX],
    len: usize,
}

impl UnwindBacktrace {
    pub const fn empty() -> Self {
        Self {
            frames: [0; UNWIND_BACKTRACE_MAX],
            len: 0,
        }
    }

    pub fn as_slice(&self) -> &[u64] {
        &self.frames[..self.len]
    }

    #[cfg(feature = "kernel-unwind")]
    fn push(&mut self, ip: u64) {
        if self.len < self.frames.len() {
            self.frames[self.len] = ip;
            self.len += 1;
        }
    }
}

#[cfg(feature = "kernel-unwind")]
#[repr(C)]
struct ExceptionWithPayload {
    exception: MaybeUninit<UnwindException>,
    payload: KernelPanic,
}

#[cfg(feature = "kernel-unwind")]
unsafe impl Exception for KernelPanic {
    const CLASS: [u8; 8] = *b"SLOPKERN";

    fn wrap(this: Self) -> *mut UnwindException {
        match Box::try_new(ExceptionWithPayload {
            exception: MaybeUninit::uninit(),
            payload: this,
        }) {
            Ok(exception) => Box::into_raw(exception) as *mut UnwindException,
            Err(_) => core::ptr::null_mut(),
        }
    }

    unsafe fn unwrap(ex: *mut UnwindException) -> Self {
        let ex = ex as *mut ExceptionWithPayload;
        // SAFETY: `ex` was allocated by `wrap` for this exception class and
        // is consumed exactly once by the unwinder's catch/delete path.
        let ex = unsafe { Box::from_raw(ex) };
        ex.payload
    }
}

#[cfg(feature = "kernel-unwind")]
pub fn begin_panic(_info: &PanicInfo) -> Result<Infallible, UnwindReasonCode> {
    unsafe extern "C" fn exception_cleanup(
        _unwind_code: UnwindReasonCode,
        exception: *mut UnwindException,
    ) {
        // SAFETY: this cleanup is installed only on exceptions allocated below.
        unsafe {
            let _ = KernelPanic::unwrap(exception);
        }
    }

    let mut exception = Box::try_new(ExceptionWithPayload {
        exception: MaybeUninit::uninit(),
        payload: KernelPanic,
    })
    .map_err(|_| UnwindReasonCode::FATAL_PHASE1_ERROR)?;

    let ex = exception.exception.as_mut_ptr();
    // SAFETY: `ex` points into the boxed exception object and is initialized
    // before ownership is handed to the unwinder.
    unsafe {
        (*ex).exception_class = u64::from_ne_bytes(KernelPanic::CLASS);
        (*ex).exception_cleanup = Some(exception_cleanup);
        let ex = Box::into_raw(exception) as *mut UnwindException;
        Err(_Unwind_RaiseException(ex))
    }
}

#[cfg(not(feature = "kernel-unwind"))]
pub fn begin_panic(_info: &PanicInfo) -> Result<Infallible, ()> {
    Err(())
}

#[cfg(feature = "kernel-unwind")]
pub fn catch_unwind<R, F>(f: F) -> Result<R, KernelPanic>
where
    F: FnOnce() -> R,
{
    match panicking::catch_unwind::<KernelPanic, R, F>(f) {
        Ok(value) => Ok(value),
        Err(Some(payload)) => Err(payload),
        Err(None) => crate::panic::abort_now(),
    }
}

#[cfg(all(feature = "test-helpers", not(feature = "kernel-unwind")))]
pub fn catch_unwind<R, F>(f: F) -> Result<R, KernelPanic>
where
    F: FnOnce() -> R,
{
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(_) => Err(KernelPanic),
    }
}

#[cfg(not(any(feature = "kernel-unwind", feature = "test-helpers")))]
pub fn catch_unwind<R, F>(f: F) -> Result<R, KernelPanic>
where
    F: FnOnce() -> R,
{
    Ok(f())
}

#[cfg(feature = "kernel-unwind")]
pub fn capture_backtrace() -> UnwindBacktrace {
    extern "C" fn trace(ctx: &UnwindContext<'_>, arg: *mut c_void) -> UnwindReasonCode {
        let out = arg.cast::<UnwindBacktrace>();
        if out.is_null() {
            return UnwindReasonCode::NORMAL_STOP;
        }
        let ip = _Unwind_GetIP(ctx) as u64;
        // SAFETY: `_Unwind_Backtrace` calls this synchronously with the
        // `UnwindBacktrace` pointer passed below.
        unsafe {
            (*out).push(ip);
            if (*out).len >= UNWIND_BACKTRACE_MAX {
                return UnwindReasonCode::NORMAL_STOP;
            }
        }
        UnwindReasonCode::NO_REASON
    }

    let mut out = UnwindBacktrace::empty();
    let _ = _Unwind_Backtrace(trace, (&mut out as *mut UnwindBacktrace).cast());
    out
}

#[cfg(not(feature = "kernel-unwind"))]
pub fn capture_backtrace() -> UnwindBacktrace {
    UnwindBacktrace::empty()
}
