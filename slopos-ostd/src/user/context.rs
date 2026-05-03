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

    pub fn empty() -> Self {
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
pub struct UserContext {
    regs: UserRegs,
    fpu_state: FpuStateRef,
}

impl UserContext {
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
