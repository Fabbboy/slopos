//! User-mode CPU register context.
//!
//! [`UserContext`] is the OSTD-canonical snapshot of the user-mode
//! register file. It is the only public way to express "the state a
//! user-mode thread should resume with"; in particular,
//! [`UserContext::set_rflags`] is the only path that writes RFLAGS,
//! and it forcibly masks every flag that, if forged by user code,
//! would let userland step outside its sandbox (Inv. 2).
//!
//! `cs` / `ss` are populated by [`UserContext::new`] from the OSTD
//! GDT selectors and are not publicly settable.
//!
//! Argument-register validation happens through
//! [`UserContext::user_ptr_arg`] and friends — these are the only
//! public constructors of [`crate::user::ptr::UserPtr`] /
//! [`crate::user::ptr::UserSlice`], enforcing Inv. 5 (no
//! kernel-half address ever enters the kernel as a user pointer).

use crate::mm::Pod;
use crate::user::ptr::{UserBytes, UserPtr, UserPtrError, UserSlice};

const USER_CODE_SELECTOR: u16 = 0x23;
const USER_DATA_SELECTOR: u16 = 0x1B;

/// Bits the user is allowed to influence directly via
/// [`UserContext::set_rflags`].
///
/// - bit 0  CF
/// - bit 2  PF
/// - bit 4  AF
/// - bit 6  ZF
/// - bit 7  SF
/// - bit 8  TF       — single-step (debugger)
/// - bit 10 DF
/// - bit 11 OF
/// - bit 21 ID       — CPUID-allowed
pub const USER_RFLAGS_PERMITTED: u64 = (1 << 0)
    | (1 << 2)
    | (1 << 4)
    | (1 << 6)
    | (1 << 7)
    | (1 << 8)
    | (1 << 10)
    | (1 << 11)
    | (1 << 21);

/// Bits forced on regardless of caller value.
///
/// - bit 1  reserved-MBO
/// - bit 9  IF       — user must run with interrupts enabled
pub const USER_RFLAGS_FORCED: u64 = (1 << 1) | (1 << 9);

#[derive(Clone, Copy, Debug, Default)]
#[repr(C)]
pub struct UserRegs {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    pub rflags_user_subset: u64,
    pub fs_base: u64,
    pub gs_base: u64,
    pub cs: u16,
    pub ss: u16,
    pub _pad: [u16; 3],
}

// SAFETY (Inv. 2): `UserRegs` is consumed by `__ostd_user_return`'s
// inline asm at fixed byte offsets. These compile-time asserts pin the
// layout so a future field reorder fails to build rather than silently
// scrambling user state on the next user-mode round trip.
//
// Every offset `asm/user_return.s` names as a `UR_*` displacement is
// here, including the fourteen general-purpose registers between `rax`
// and `rip`: that file mirrors them by hand, so an unasserted one lets
// a field swap write user RBX into the RCX slot with every other razor
// still passing. `user_mode_round_trip_asm` feeds `offset_of!` straight
// into its operands and needs no razor of its own.
const _: () = {
    use core::mem::offset_of;
    assert!(offset_of!(UserRegs, rax) == 0);
    assert!(offset_of!(UserRegs, rbx) == 8);
    assert!(offset_of!(UserRegs, rcx) == 2 * 8);
    assert!(offset_of!(UserRegs, rdx) == 3 * 8);
    assert!(offset_of!(UserRegs, rsi) == 4 * 8);
    assert!(offset_of!(UserRegs, rdi) == 5 * 8);
    assert!(offset_of!(UserRegs, rbp) == 6 * 8);
    assert!(offset_of!(UserRegs, rsp) == 7 * 8);
    assert!(offset_of!(UserRegs, r8) == 8 * 8);
    assert!(offset_of!(UserRegs, r9) == 9 * 8);
    assert!(offset_of!(UserRegs, r10) == 10 * 8);
    assert!(offset_of!(UserRegs, r11) == 11 * 8);
    assert!(offset_of!(UserRegs, r12) == 12 * 8);
    assert!(offset_of!(UserRegs, r13) == 13 * 8);
    assert!(offset_of!(UserRegs, r14) == 14 * 8);
    assert!(offset_of!(UserRegs, r15) == 15 * 8);
    assert!(offset_of!(UserRegs, rip) == 16 * 8);
    assert!(offset_of!(UserRegs, rflags_user_subset) == 17 * 8);
    assert!(offset_of!(UserRegs, fs_base) == 18 * 8);
    assert!(offset_of!(UserRegs, gs_base) == 19 * 8);
    assert!(offset_of!(UserRegs, cs) == 20 * 8);
    assert!(offset_of!(UserRegs, ss) == 20 * 8 + 2);
    assert!(core::mem::size_of::<UserRegs>() == 176);
};

// SAFETY: every field is u64/u16/[u16; 3] — primitive integer types
// whose all-zero bit pattern is a valid value. No references,
// pointers, niche-constrained enums, or `bool` fields.
unsafe impl crate::mm::init::Zeroable for UserRegs {}

impl UserRegs {
    /// `const fn` zero/default constructor — usable in `const`
    /// contexts where `Default::default()` cannot be called.
    pub const fn const_zeroed() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rsi: 0,
            rdi: 0,
            rbp: 0,
            rsp: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rip: 0,
            rflags_user_subset: 0,
            fs_base: 0,
            gs_base: 0,
            cs: 0,
            ss: 0,
            _pad: [0; 3],
        }
    }
}

/// Borrowed handle to a task's XSAVE/FXSAVE buffer.
///
/// Opaque to consumers — the only way to produce one is through
/// the trusted side via [`FpuStateRef::from_raw`].
/// [`crate::user::mode::UserMode::execute`] is the only consumer;
/// it loads the buffer with `XRSTOR` on entry and persists it with
/// `XSAVE` on user-to-kernel transitions.
#[derive(Clone, Copy)]
pub struct FpuStateRef {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: `FpuStateRef` carries a raw pointer + length pair. The
// borrow tracking happens at the OSTD-internal call sites (the task
// owns the buffer, and `UserMode<'a>` holds a `&'a mut UserContext`
// which transitively pins the FpuStateRef for the duration of
// `execute`). Sending a `UserContext` between threads is fine because
// the buffer it points at is owned by the task it represents.
unsafe impl Send for FpuStateRef {}
unsafe impl Sync for FpuStateRef {}

// SAFETY: `FpuStateRef` is `(*mut u8, usize)`. Both `*mut u8`
// (Zeroable yields null) and `usize` (Zeroable yields 0) are valid
// when zero — the resulting `FpuStateRef` is the same as
// `FpuStateRef::empty()`.
unsafe impl crate::mm::init::Zeroable for FpuStateRef {}

impl FpuStateRef {
    /// # Safety
    ///
    /// `ptr` must point to `len` bytes of a 64-byte aligned XSAVE
    /// area owned by the task this context is built for, and the
    /// pointer must remain valid for as long as the resulting
    /// `UserContext` is live.
    pub unsafe fn from_raw(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    pub const fn empty() -> Self {
        Self {
            ptr: core::ptr::null_mut(),
            len: 0,
        }
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ptr.is_null() || self.len == 0
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut u8 {
        self.ptr
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.len
    }
}

impl core::fmt::Debug for FpuStateRef {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("FpuStateRef")
            .field("ptr", &self.ptr)
            .field("len", &self.len)
            .finish()
    }
}

#[derive(Clone, Debug)]
#[repr(C)]
pub struct UserContext {
    regs: UserRegs,
    fpu_state: FpuStateRef,
}

// `__ostd_user_return` loads a `*mut UserContext` out of the PCR and
// indexes it with the `UR_*` displacements above — it treats the whole
// context as its register file, so `regs` must sit at offset zero.
const _: () = assert!(core::mem::offset_of!(UserContext, regs) == 0);

// SAFETY: every field of `UserContext` is `Zeroable`
// (`UserRegs` and `FpuStateRef` impls above). The all-zero
// bit pattern is identical to what `UserContext::const_zeroed()`
// produces. `#[repr(C)]` pins the layout so the impl stays
// well-formed under field reorder.
unsafe impl crate::mm::init::Zeroable for UserContext {}

impl UserContext {
    /// `const fn` zero/uninitialised constructor.  All-zero regs +
    /// empty FPU state.  Suitable for slots inside larger structs
    /// (e.g. `Task::invalid()`) that need to be const-constructible
    /// before being filled in by an init path.  Production code
    /// should reach for [`Self::new`] / [`Self::set_regs`] before
    /// the context is consumed by [`crate::user::mode::UserMode`].
    pub const fn const_zeroed() -> Self {
        Self {
            regs: UserRegs::const_zeroed(),
            fpu_state: FpuStateRef::empty(),
        }
    }

    /// Null-checked reborrow of a raw `*mut UserContext`.
    ///
    /// Callers in syscall handlers receive `*mut UserContext` from the
    /// `__ostd_user_return` trampoline (or the legacy `int 0x80`
    /// adapter); this helper centralises the null-check + lifetime
    /// reborrow so handlers do not write `unsafe { &mut *ptr }` at
    /// every entry.
    ///
    /// SAFETY (Inv. 5): every per-task `UserContext` lives at a fixed
    /// position inside the per-task `Task` struct (`Task::user_ctx`)
    /// for the task's lifetime; the syscall path holds a kernel-mode
    /// borrow that cannot race with the user-mode round trip per the
    /// `__ostd_user_return` contract (which only re-enters with
    /// `pcr.user_ctx_ptr` after kernel-side return). The unsafe
    /// `&mut *ptr` is therefore sound iff the caller did not fabricate
    /// a non-task pointer.
    /// **Prefer a form whose lifetime is anchored.** This one lets the caller
    /// pick, and two picks is two `&mut UserContext` to one task. It survives
    /// because `SyscallContext::user_ctx_mut` takes `&self` and returns the
    /// borrow out, so the honest anchor is `&mut self` — which means threading
    /// `&mut SyscallContext` through every handler. Worth doing; not worth
    /// doing as a side effect of a lifetime cleanup.
    #[inline]
    pub fn from_ptr_mut<'a>(ptr: *mut UserContext) -> Option<&'a mut UserContext> {
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { &mut *ptr })
    }

    /// Read-only sibling of [`Self::from_ptr_mut`].
    #[inline]
    pub fn from_ptr<'a>(ptr: *const UserContext) -> Option<&'a UserContext> {
        if ptr.is_null() {
            return None;
        }
        Some(unsafe { &*ptr })
    }

    /// Build a fresh `UserContext` with the given GPR snapshot. `cs`
    /// and `ss` are forced to the OSTD user selectors; `rflags` is
    /// passed through [`Self::set_rflags`].
    pub fn new(mut regs: UserRegs, fpu_state: FpuStateRef) -> Self {
        regs.cs = USER_CODE_SELECTOR;
        regs.ss = USER_DATA_SELECTOR;
        regs._pad = [0; 3];
        let mut ctx = Self { regs, fpu_state };
        let raw_rflags = ctx.regs.rflags_user_subset;
        ctx.set_rflags(raw_rflags);
        ctx
    }

    #[inline]
    pub fn regs(&self) -> &UserRegs {
        &self.regs
    }

    /// Raw pointer to the embedded `UserRegs`.  Used by the kernel→user
    /// round-trip asm helper (see
    /// [`crate::user::mode::user_mode_round_trip_asm`]); ordinary
    /// callers should reach for [`Self::regs`] / [`Self::set_regs`]
    /// instead.
    #[inline]
    pub fn regs_ptr(&self) -> *const UserRegs {
        &self.regs as *const UserRegs
    }

    /// Mutable raw pointer to the embedded `UserRegs`.  Same caveat as
    /// [`Self::regs_ptr`].
    #[inline]
    pub fn regs_mut_ptr(&mut self) -> *mut UserRegs {
        &mut self.regs as *mut UserRegs
    }

    /// Direct mutable view of the embedded GPR snapshot.
    ///
    /// **Mask discipline is on the caller**: writes to
    /// `rflags_user_subset` through this reference bypass
    /// [`Self::set_rflags`]'s sensitive-bit filter, and writes to
    /// `cs` / `ss` bypass the user-selector reapplication that
    /// [`Self::new`] / [`Self::set_regs`] guarantee.  Prefer
    /// [`Self::set_regs`] for any path that is (or could be) driven by
    /// user-supplied register values; reach for this only when the
    /// caller is OSTD-internal kernel state-management (e.g. seeding a
    /// fresh context from a known-good kernel-supplied snapshot, or
    /// kernel-mode test scaffolding constructing inputs to a syscall
    /// handler).
    #[inline]
    pub fn regs_mut(&mut self) -> &mut UserRegs {
        &mut self.regs
    }

    /// Replace the entire register snapshot. `cs` / `ss` from `regs`
    /// are ignored — the OSTD selectors are re-applied. `rflags` is
    /// re-masked through [`Self::set_rflags`] so a caller cannot
    /// bypass the sensitive-bits filter by writing the snapshot
    /// directly.
    pub fn set_regs(&mut self, mut regs: UserRegs) {
        regs.cs = USER_CODE_SELECTOR;
        regs.ss = USER_DATA_SELECTOR;
        regs._pad = [0; 3];
        let raw_rflags = regs.rflags_user_subset;
        self.regs = regs;
        self.set_rflags(raw_rflags);
    }

    #[inline]
    pub fn fpu_state(&self) -> FpuStateRef {
        self.fpu_state
    }

    #[inline]
    pub fn rip(&self) -> u64 {
        self.regs.rip
    }

    #[inline]
    pub fn set_rip(&mut self, rip: u64) {
        self.regs.rip = rip;
    }

    #[inline]
    pub fn set_rax(&mut self, rax: u64) {
        self.regs.rax = rax;
    }

    #[inline]
    pub fn rsp(&self) -> u64 {
        self.regs.rsp
    }

    #[inline]
    pub fn set_rsp(&mut self, rsp: u64) {
        self.regs.rsp = rsp;
    }

    #[inline]
    pub fn fs_base(&self) -> u64 {
        self.regs.fs_base
    }

    #[inline]
    pub fn set_fs_base(&mut self, fs_base: u64) {
        self.regs.fs_base = fs_base;
    }

    #[inline]
    pub fn gs_base(&self) -> u64 {
        self.regs.gs_base
    }

    #[inline]
    pub fn set_gs_base(&mut self, gs_base: u64) {
        self.regs.gs_base = gs_base;
    }

    /// Write user-mode RFLAGS, masking every sensitive bit.
    ///
    /// SAFETY note (Inv. 2): the kernel never copies user-supplied
    /// RFLAGS into hardware verbatim. IOPL=0 forbids user port I/O,
    /// IF=1 keeps the user preemptible, AC=0 keeps SMAP enforcement
    /// active for kernel-mode code that runs after the next IRETQ,
    /// VM/NT/RF/VIF/VIP cleared closes the corresponding x86 escape
    /// hatches.
    pub fn set_rflags(&mut self, value: u64) {
        self.regs.rflags_user_subset = (value & USER_RFLAGS_PERMITTED) | USER_RFLAGS_FORCED;
    }

    /// Read the masked RFLAGS value as it will be loaded by IRETQ.
    #[inline]
    pub fn rflags(&self) -> u64 {
        self.regs.rflags_user_subset
    }

    /// Read the raw value of a syscall argument register. Indices map
    /// to the Linux x86_64 syscall ABI: `0 → rdi`, `1 → rsi`,
    /// `2 → rdx`, `3 → r10`, `4 → r8`, `5 → r9`.
    pub fn syscall_arg(&self, reg_index: usize) -> u64 {
        match reg_index {
            0 => self.regs.rdi,
            1 => self.regs.rsi,
            2 => self.regs.rdx,
            3 => self.regs.r10,
            4 => self.regs.r8,
            5 => self.regs.r9,
            _ => panic!("UserContext::syscall_arg: reg_index {reg_index} out of range"),
        }
    }

    /// Build a validated typed [`UserPtr<T>`] from syscall argument
    /// register `reg_index`. This is the only public construction
    /// path for `UserPtr`, which is what enforces Inv. 5 — every
    /// user pointer that crosses into the kernel must come from a
    /// register that was loaded by a user-mode IRETQ exit or
    /// SYSCALL entry.
    pub fn user_ptr_arg<T: Pod>(&self, reg_index: usize) -> Result<UserPtr<T>, UserPtrError> {
        UserPtr::<T>::try_new(self.syscall_arg(reg_index))
    }

    /// Build a validated [`UserSlice<T>`] from a (base, count) pair
    /// of syscall argument registers.
    pub fn user_slice_arg<T: Pod>(
        &self,
        base_idx: usize,
        count_idx: usize,
    ) -> Result<UserSlice<T>, UserPtrError> {
        let base = self.syscall_arg(base_idx);
        let count = self.syscall_arg(count_idx) as usize;
        UserSlice::<T>::try_new(base, count)
    }

    /// Convenience for byte buffers (`UserSlice<u8>`).
    pub fn user_bytes_arg(
        &self,
        base_idx: usize,
        len_idx: usize,
    ) -> Result<UserBytes, UserPtrError> {
        let base = self.syscall_arg(base_idx);
        let len = self.syscall_arg(len_idx) as usize;
        UserSlice::<u8>::try_new(base, len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx_with_rflags(rflags: u64) -> UserContext {
        let mut regs = UserRegs::default();
        regs.rflags_user_subset = rflags;
        UserContext::new(regs, FpuStateRef::empty())
    }

    #[test]
    fn cs_ss_forced_to_user_selectors() {
        let ctx = ctx_with_rflags(0);
        assert_eq!(ctx.regs().cs, USER_CODE_SELECTOR);
        assert_eq!(ctx.regs().ss, USER_DATA_SELECTOR);
    }

    #[test]
    fn set_rflags_forces_if_and_mbo() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags(0);
        let f = ctx.rflags();
        assert!(f & (1 << 9) != 0, "IF must be forced on");
        assert!(f & (1 << 1) != 0, "MBO bit 1 must be forced on");
    }

    #[test]
    fn set_rflags_clears_iopl() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags(0x3000); // IOPL = 3
        assert_eq!(ctx.rflags() & 0x3000, 0);
    }

    #[test]
    fn set_rflags_clears_ac() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags(1 << 18);
        assert_eq!(ctx.rflags() & (1 << 18), 0);
    }

    #[test]
    fn set_rflags_clears_nt_and_vm() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 14) | (1 << 17));
        assert_eq!(ctx.rflags() & ((1 << 14) | (1 << 17)), 0);
    }

    #[test]
    fn set_rflags_clears_vif_vip_and_rf() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 16) | (1 << 19) | (1 << 20));
        assert_eq!(ctx.rflags() & ((1 << 16) | (1 << 19) | (1 << 20)), 0);
    }

    #[test]
    fn set_rflags_preserves_id_and_user_arith_flags() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 21) | (1 << 0) | (1 << 6) | (1 << 11));
        let f = ctx.rflags();
        assert!(f & (1 << 21) != 0, "ID preserved");
        assert!(f & (1 << 0) != 0, "CF preserved");
        assert!(f & (1 << 6) != 0, "ZF preserved");
        assert!(f & (1 << 11) != 0, "OF preserved");
    }

    #[test]
    fn set_rflags_drops_all_other_bits() {
        let mut ctx = ctx_with_rflags(0);
        ctx.set_rflags(u64::MAX);
        let f = ctx.rflags();
        // No bits beyond 21 may be set.
        assert_eq!(f & !((1u64 << 22) - 1), 0, "high bits leaked: {f:#x}");
        // IOPL must be zero.
        assert_eq!(f & 0x3000, 0);
    }

    #[test]
    fn syscall_arg_register_mapping() {
        let mut regs = UserRegs::default();
        regs.rdi = 0x10;
        regs.rsi = 0x20;
        regs.rdx = 0x30;
        regs.r10 = 0x40;
        regs.r8 = 0x50;
        regs.r9 = 0x60;
        let ctx = UserContext::new(regs, FpuStateRef::empty());
        assert_eq!(ctx.syscall_arg(0), 0x10);
        assert_eq!(ctx.syscall_arg(1), 0x20);
        assert_eq!(ctx.syscall_arg(2), 0x30);
        assert_eq!(ctx.syscall_arg(3), 0x40);
        assert_eq!(ctx.syscall_arg(4), 0x50);
        assert_eq!(ctx.syscall_arg(5), 0x60);
    }

    #[test]
    #[should_panic(expected = "out of range")]
    fn syscall_arg_out_of_range_panics() {
        let ctx = ctx_with_rflags(0);
        let _ = ctx.syscall_arg(6);
    }

    #[test]
    fn user_ptr_arg_validates_user_range() {
        let mut regs = UserRegs::default();
        regs.rdi = 0x0000_4000_0000_1000;
        let ctx = UserContext::new(regs, FpuStateRef::empty());
        let p = ctx.user_ptr_arg::<u32>(0).unwrap();
        assert_eq!(p.as_u64(), 0x0000_4000_0000_1000);
    }

    #[test]
    fn user_ptr_arg_rejects_kernel_address() {
        let mut regs = UserRegs::default();
        regs.rdi = 0xffff_8000_0000_1000;
        let ctx = UserContext::new(regs, FpuStateRef::empty());
        let r = ctx.user_ptr_arg::<u32>(0);
        assert!(matches!(r, Err(UserPtrError::OutOfUserRange)));
    }

    #[test]
    fn user_ptr_arg_rejects_null() {
        let regs = UserRegs::default();
        let ctx = UserContext::new(regs, FpuStateRef::empty());
        let r = ctx.user_ptr_arg::<u32>(0);
        assert!(matches!(r, Err(UserPtrError::Null)));
    }
}
