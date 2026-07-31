//! Typed syscall arguments.
//!
//! Each handler parameter declared in [`crate::define_syscall!`] must
//! implement [`SyscallArg`]. The macro threads a running register
//! cursor across the parameter list, slicing `ctx.regs()` into each
//! `from_raw` call.
//!
//! Some parameters consume more than one register slot — a
//! `UserSlice<T>` reads `(base, count)` from two consecutive register
//! positions. The associated const [`SyscallArg::ARITY`] declares how
//! many slots each type takes.

use slopos_abi::Errno;
use slopos_abi::fs::USER_PATH_MAX;
use slopos_abi::signal::NSIG;
use slopos_abi::task::{INVALID_PROCESS_ID, INVALID_TASK_ID};
use slopos_mm::user_ptr::{UserPtr as MmUserPtr, UserSlice as MmUserSlice};

use crate::syscall::common::syscall_copy_user_str;
use crate::syscall::context::SyscallContext;

/// One typed syscall parameter.
///
/// * `ARITY` — number of consecutive register slots the parameter
///   consumes. Plain integers and FDs are arity 1; [`UserSlice<T>`]
///   (and aliases) are arity 2 (base + count). The macro asserts at
///   expansion time that the sum of arities across a handler's
///   parameter list is `<= 6`.
/// * `from_raw` — given a slice of exactly `ARITY` raw register values
///   and the syscall context, decode the typed value or return an
///   [`Errno`] describing why the decode failed.
pub trait SyscallArg: Sized {
    const ARITY: usize;
    fn from_raw(regs: &[u64], ctx: &SyscallContext) -> Result<Self, Errno>;
}

// ─────────────────────────────────────────────────────────────────────
// Plain integer primitives — single-register, value-preserving cast
// ─────────────────────────────────────────────────────────────────────

macro_rules! impl_int_arg {
    ($($t:ty),+) => {
        $(impl SyscallArg for $t {
            const ARITY: usize = 1;
            #[inline]
            fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
                Ok(regs[0] as $t)
            }
        })+
    };
}
impl_int_arg!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize);

// ─────────────────────────────────────────────────────────────────────
// Typed identifiers
// ─────────────────────────────────────────────────────────────────────

/// File descriptor that **must** be non-negative.
///
/// Rejects negative values with `EBADF`. Use [`RawFd`] for syscalls
/// such as `mmap` where `-1` (anonymous mapping) is a valid argument.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Fd(i32);

impl Fd {
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }
}

impl SyscallArg for Fd {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let signed = regs[0] as i64;
        if !(0..=i32::MAX as i64).contains(&signed) {
            return Err(Errno::EBADF);
        }
        Ok(Fd(signed as i32))
    }
}

/// File descriptor that may be `-1` (anonymous-mapping marker for
/// `mmap`, "no fd" marker for a few other syscalls).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawFd(i32);

impl RawFd {
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }
    #[inline]
    pub const fn is_present(self) -> bool {
        self.0 >= 0
    }
}

impl SyscallArg for RawFd {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let signed = regs[0] as i64;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&signed) {
            return Err(Errno::EBADF);
        }
        Ok(RawFd(signed as i32))
    }
}

/// Process ID argument (validated to not equal [`INVALID_PROCESS_ID`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pid(u32);

impl Pid {
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl SyscallArg for Pid {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let v = regs[0] as u32;
        if v == INVALID_PROCESS_ID {
            return Err(Errno::ESRCH);
        }
        Ok(Pid(v))
    }
}

/// Task ID argument (validated to not equal [`INVALID_TASK_ID`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Tid(u32);

impl Tid {
    #[inline]
    pub const fn raw(self) -> u32 {
        self.0
    }
}

impl SyscallArg for Tid {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let v = regs[0] as u32;
        if v == INVALID_TASK_ID {
            return Err(Errno::ESRCH);
        }
        Ok(Tid(v))
    }
}

/// Signal-target PID for `kill(2)`. Preserves the signed semantics
/// (positive → single target, `0` → caller's group, `-1` → all,
/// `< -1` → group `-pid`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct SigPid(i32);

impl SigPid {
    #[inline]
    pub const fn raw(self) -> i32 {
        self.0
    }
}

impl SyscallArg for SigPid {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let signed = regs[0] as i64;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&signed) {
            return Err(Errno::ESRCH);
        }
        Ok(SigPid(signed as i32))
    }
}

/// Signal number, validated to fall inside `1..=NSIG`.
///
/// The bound is `NSIG` rather than a round number because the only
/// consumer, `rt_sigaction`, turns the value into `signum - 1` and
/// indexes `Task::signal_actions`, which is `[SignalActionCell; NSIG]`.
/// A looser bound here is an out-of-range index there, and with
/// `tests=on` `production_recovery_enabled()` is false, so that index
/// takes the whole run down rather than failing one syscall.
///
/// `crate::syscall::signal::parse_signum` applies the same bound for
/// `kill`, which takes a raw `u64` because signal 0 is meaningful there.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Signum(u8);

// `from_raw` narrows the validated value to `u8`. That is lossless only
// while `NSIG` fits in a `u8`; a larger signal space needs a wider
// newtype before it needs a wider bound.
const _: () = assert!(
    NSIG <= u8::MAX as usize,
    "Signum stores the validated signal number in a u8"
);

impl Signum {
    #[inline]
    pub const fn raw(self) -> u8 {
        self.0
    }
}

impl SyscallArg for Signum {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let v = regs[0];
        if v == 0 || v as usize > NSIG {
            return Err(Errno::EINVAL);
        }
        Ok(Signum(v as u8))
    }
}

// ─────────────────────────────────────────────────────────────────────
// User-space pointer / slice wrappers
// ─────────────────────────────────────────────────────────────────────

/// Typed user-space pointer argument. One register: `addr`.
///
/// Wraps [`slopos_mm::user_ptr::UserPtr<T>`]; use [`UserPtr::inner`]
/// to pass to the kernel's user-copy primitives (which take a
/// `T: Copy` bound at the call site).
#[derive(Clone, Copy)]
pub struct UserPtr<T> {
    inner: MmUserPtr<T>,
}

impl<T> UserPtr<T> {
    #[inline]
    pub const fn inner(self) -> MmUserPtr<T> {
        self.inner
    }
    #[inline]
    pub fn as_u64(self) -> u64 {
        self.inner.as_u64()
    }
}

impl<T> SyscallArg for UserPtr<T> {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        match MmUserPtr::<T>::try_new(regs[0]) {
            Ok(p) => Ok(UserPtr { inner: p }),
            Err(_) => Err(Errno::EFAULT),
        }
    }
}

impl<T> SyscallArg for Option<UserPtr<T>> {
    const ARITY: usize = 1;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        if regs[0] == 0 {
            Ok(None)
        } else {
            match MmUserPtr::<T>::try_new(regs[0]) {
                Ok(p) => Ok(Some(UserPtr { inner: p })),
                Err(_) => Err(Errno::EFAULT),
            }
        }
    }
}

/// Typed user-space slice argument. Two registers: `base`, `count`.
///
/// Empty slices (`count == 0`) at a null base must be declared as
/// `Option<UserSlice<T>>` so the typed parser can map `base == 0`
/// to `None`.
#[derive(Clone, Copy)]
pub struct UserSlice<T> {
    inner: MmUserSlice<T>,
}

impl<T> UserSlice<T> {
    #[inline]
    pub const fn inner(&self) -> &MmUserSlice<T> {
        &self.inner
    }
    #[inline]
    pub fn base_u64(&self) -> u64 {
        self.inner.base().as_u64()
    }
    #[inline]
    pub fn len(&self) -> usize {
        self.inner.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
    #[inline]
    pub fn byte_len(&self) -> usize {
        self.inner.byte_len()
    }
}

impl<T> SyscallArg for UserSlice<T> {
    const ARITY: usize = 2;
    #[inline]
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        let count = regs[1] as usize;
        MmUserSlice::<T>::try_new(regs[0], count)
            .map(|inner| UserSlice { inner })
            .map_err(|_| Errno::EFAULT)
    }
}

impl<T> SyscallArg for Option<UserSlice<T>> {
    const ARITY: usize = 2;
    #[inline]
    fn from_raw(regs: &[u64], ctx: &SyscallContext) -> Result<Self, Errno> {
        if regs[0] == 0 {
            return Ok(None);
        }
        UserSlice::<T>::from_raw(regs, ctx).map(Some)
    }
}

/// Convenience alias for the byte-buffer case (e.g., `read`/`write`
/// payloads). Two registers: `base`, `count`.
pub type UserBytes = UserSlice<u8>;

// ─────────────────────────────────────────────────────────────────────
// Inline NUL-terminated user C-string buffer
// ─────────────────────────────────────────────────────────────────────

/// Inline NUL-terminated copy of a user-space C string.
///
/// `from_raw` allocates `N` bytes on the handler's stack and copies up
/// to `N - 1` payload bytes (the trailing NUL is always written).
/// Stays under the 2 KiB frame gate when `N <= 1024`; the
/// default `N = USER_PATH_MAX = 256` is the canonical choice.
#[derive(Clone, Copy)]
pub struct UserCStr<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> UserCStr<N> {
    /// Bytes up to but not including the terminating NUL.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }

    /// Full buffer, NUL-terminated.
    #[inline]
    pub fn as_nul_terminated(&self) -> &[u8] {
        &self.buf[..]
    }

    /// Length of the payload (excluding the trailing NUL).
    #[inline]
    pub const fn len(&self) -> usize {
        self.len
    }

    #[inline]
    pub const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

impl<const N: usize> SyscallArg for UserCStr<N> {
    const ARITY: usize = 1;
    fn from_raw(regs: &[u64], _ctx: &SyscallContext) -> Result<Self, Errno> {
        const {
            assert!(
                N >= 2,
                "UserCStr<N> requires room for at least one byte + NUL"
            );
        }
        if regs[0] == 0 {
            return Err(Errno::EFAULT);
        }
        let mut buf = [0u8; N];
        syscall_copy_user_str(&mut buf, regs[0]).map_err(|_| Errno::EFAULT)?;
        let len = buf.iter().position(|&b| b == 0).unwrap_or(N - 1);
        Ok(UserCStr { buf, len })
    }
}

impl<const N: usize> SyscallArg for Option<UserCStr<N>> {
    const ARITY: usize = 1;
    fn from_raw(regs: &[u64], ctx: &SyscallContext) -> Result<Self, Errno> {
        if regs[0] == 0 {
            return Ok(None);
        }
        UserCStr::<N>::from_raw(regs, ctx).map(Some)
    }
}

/// Re-export so handlers can spell `UserCStr<PATH_MAX>` without an
/// extra import.
pub const PATH_MAX: usize = USER_PATH_MAX;
