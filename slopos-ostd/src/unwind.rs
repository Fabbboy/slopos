//! Kernel-facing Rust unwind facade.
//!
//! This is the only public kernel surface over the `unwinding` crate. It keeps
//! panic payload allocation, catch boundaries, and fixed-buffer DWARF backtrace
//! capture inside OSTD.
//!
//! Unwinding is kernel-internal only. It must not cross user/kernel entry,
//! interrupt/trap stubs, scheduler context switches, or other non-Rust ABI
//! boundaries; those paths must catch before the boundary or abort.
//!
//! The `unwinding` crate is only linked for the bare-metal kernel target
//! (`target_os = "none"`); it provides the `eh_personality` lang item and the
//! `_Unwind_*` symbols a `no_std` build needs. On the host/Miri targets
//! (`cargo test` / `cargo miri test`) `std` already owns those, so pulling in
//! `unwinding` there collides on the `eh_personality` lang item (E0152). The
//! host shim below keeps this module's surface present off-target — where the
//! std test harness owns panic handling — so `catch_panic!` and any host test
//! that names this facade still compile.
//!
//! `unwinding` is built without its `dwarf-expr` feature: a call-frame
//! instruction that requires DWARF expression evaluation
//! (`DW_CFA_def_cfa_expression` and friends) fails the unwind,
//! `begin_panic` returns `Err`, and the panic falls through to the fatal
//! abort path. Fail-safe: an unsupported CFI rule can never resume with
//! wrong register state. Both `unwinding` and its DWARF reader `gimli`
//! are vendored TCB annexes pinned by `scripts/check_vendor_pin.sh`.

use core::fmt::{self, Write};
use core::panic::PanicInfo;

const OOPS_FILE_MAX: usize = 96;
const OOPS_REASON_MAX: usize = 160;

#[derive(Clone, Copy, Debug)]
pub struct FixedOopsStr<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedOopsStr<N> {
    pub const fn empty() -> Self {
        Self {
            bytes: [0; N],
            len: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("<invalid utf8>")
    }

    fn push_str_trunc(&mut self, s: &str) {
        let remaining = N.saturating_sub(self.len);
        let mut take = remaining.min(s.len());
        while take > 0 && !s.is_char_boundary(take) {
            take -= 1;
        }
        self.bytes[self.len..self.len + take].copy_from_slice(&s.as_bytes()[..take]);
        self.len += take;
    }
}

impl<const N: usize> Write for FixedOopsStr<N> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.push_str_trunc(s);
        Ok(())
    }
}

/// Panic metadata carried through the catch boundary to the recovery
/// consumer. Deliberately holds no backtrace: the panic handler prints the
/// symbolized trace exactly once on serial with interrupts disabled, and
/// this struct is `Copy` — a frames array would grow every copy to
/// duplicate what serial already carries.
#[derive(Clone, Copy, Debug)]
pub struct OopsInfo {
    /// Best-effort task id supplied by the scheduler registration hook.
    pub task_id: u32,
    pub file: FixedOopsStr<OOPS_FILE_MAX>,
    pub line: u32,
    pub column: u32,
    pub reason: FixedOopsStr<OOPS_REASON_MAX>,
}

impl OopsInfo {
    pub fn from_panic_info(info: &PanicInfo<'_>) -> Self {
        let mut file = FixedOopsStr::empty();
        let mut line = 0;
        let mut column = 0;
        if let Some(location) = info.location() {
            file.push_str_trunc(location.file());
            line = location.line();
            column = location.column();
        }

        let mut reason = FixedOopsStr::empty();
        let _ = write!(reason, "{}", info.message());

        Self {
            task_id: crate::panic_recovery::current_oops_task_id(),
            file,
            line,
            column,
            reason,
        }
    }
}

#[derive(Debug)]
pub struct KernelPanic {
    pub info: OopsInfo,
}

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

    #[cfg(target_os = "none")]
    fn push(&mut self, ip: u64) {
        if self.len < self.frames.len() {
            self.frames[self.len] = ip;
            self.len += 1;
        }
    }
}

#[cfg(target_os = "none")]
pub use bare_metal::{begin_panic, capture_backtrace, catch_unwind};

/// Bare-metal implementation: real DWARF unwinding via the `unwinding` crate.
#[cfg(target_os = "none")]
mod bare_metal {
    use super::{KernelPanic, UNWIND_BACKTRACE_MAX, UnwindBacktrace};
    use alloc::alloc::AllocError;
    use alloc::boxed::Box;
    use core::convert::Infallible;
    use core::ffi::c_void;
    use core::mem::MaybeUninit;
    use core::panic::PanicInfo;
    use core::ptr::addr_of_mut;

    use unwinding::abi::{
        _Unwind_Backtrace, _Unwind_GetIP, _Unwind_RaiseException, UnwindContext, UnwindException,
        UnwindReasonCode,
    };
    use unwinding::panicking::{self, Exception};

    #[repr(C)]
    struct ExceptionWithPayload {
        exception: MaybeUninit<UnwindException>,
        payload: KernelPanic,
    }

    fn allocate_exception(payload: KernelPanic) -> Result<Box<ExceptionWithPayload>, AllocError> {
        let boxed: Box<MaybeUninit<ExceptionWithPayload>> = Box::try_new_uninit()?;
        // SAFETY: `boxed` is a fresh heap slot sized and aligned for
        // `ExceptionWithPayload`; both fields are written before converting
        // the allocation to `Box<ExceptionWithPayload>`.
        unsafe {
            let raw = Box::into_raw(boxed);
            let slot = (*raw).as_mut_ptr();
            addr_of_mut!((*slot).exception).write(MaybeUninit::uninit());
            addr_of_mut!((*slot).payload).write(payload);
            Ok(Box::from_raw(slot))
        }
    }

    unsafe impl Exception for KernelPanic {
        const CLASS: [u8; 8] = *b"SLOPKERN";

        fn wrap(this: Self) -> *mut UnwindException {
            match allocate_exception(this) {
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

    pub fn begin_panic(info: &PanicInfo) -> Result<Infallible, UnwindReasonCode> {
        unsafe extern "C" fn exception_cleanup(
            _unwind_code: UnwindReasonCode,
            exception: *mut UnwindException,
        ) {
            // SAFETY: this cleanup is installed only on exceptions allocated below.
            unsafe {
                let _ = KernelPanic::unwrap(exception);
            }
        }

        let mut exception = allocate_exception(KernelPanic {
            info: KernelPanic::oops_info(info),
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

    impl KernelPanic {
        fn oops_info(info: &PanicInfo<'_>) -> super::OopsInfo {
            super::OopsInfo::from_panic_info(info)
        }
    }

    pub fn catch_unwind<R, F>(f: F) -> Result<R, KernelPanic>
    where
        F: FnOnce() -> R,
    {
        match panicking::catch_unwind::<KernelPanic, R, F>(f) {
            Ok(value) => Ok(value),
            Err(Some(payload)) => {
                crate::panic::panic_in_flight_exit();
                Err(payload)
            }
            Err(None) => crate::panic::abort_now(),
        }
    }

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
}

#[cfg(not(target_os = "none"))]
pub use host::{capture_backtrace, catch_unwind};

/// Host/Miri shim (see module docs for why `unwinding` isn't linked here):
/// the std test harness already catches panics at the `#[test]` boundary, so
/// `catch_unwind` just runs the closure and `capture_backtrace` returns an
/// empty trace.
#[cfg(not(target_os = "none"))]
mod host {
    use super::{KernelPanic, UnwindBacktrace};

    pub fn catch_unwind<R, F>(f: F) -> Result<R, KernelPanic>
    where
        F: FnOnce() -> R,
    {
        Ok(f())
    }

    pub fn capture_backtrace() -> UnwindBacktrace {
        UnwindBacktrace::empty()
    }
}
