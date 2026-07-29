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

use core::cell::SyncUnsafeCell;

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

/// Drop every RFLAGS bit userland must not influence and force the
/// ones it must not clear. Every write of `rflags_user_subset` goes
/// through this.
#[inline]
const fn sanitize_user_rflags(value: u64) -> u64 {
    (value & USER_RFLAGS_PERMITTED) | USER_RFLAGS_FORCED
}

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

// SAFETY: `FpuStateRef` is a raw pointer + length pair naming a task's
// own XSAVE area, and `Send`/`Sync` claim only that the pair is
// meaningful on any CPU — the buffer is task memory, not CPU-local.
// Neither carries an aliasing promise; exclusivity over an FPU area is
// arranged by the `TaskExclusive` witness every `Task::fpu_*` accessor
// demands, so an `FpuStateRef` riding inside a `UserContext` grants no
// access its holder did not already have. The pair's only producer is
// the `unsafe` `from_raw`, whose contract names the buffer and requires
// it to outlive every `UserContext` built for that task. `from_raw` has
// no caller today, so every `FpuStateRef` in the kernel is `empty()`.
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

/// A task's user-mode register file plus a handle to its XSAVE area.
///
/// # Who may write it
///
/// The register file sits in a cell, so writes go through `&self` and
/// the type arranges no exclusivity — the round-trip protocol does.
/// `UserContext` is `Send + Sync` (a task's context travels with the
/// task across CPUs), so this is a contract rather than a bound.
///
/// There are exactly two writers, and they cannot overlap:
///
/// 1. `__ostd_user_return`, on the task's own CPU, between the SYSCALL
///    that left user mode and the jump back into the round trip. IRQs
///    are off for that whole window — `SFMASK` clears IF on entry and
///    the `sti` is the last instruction before the jump — and the task
///    cannot be running anywhere else, because it is running here.
/// 2. Kernel code acting for that task — the syscall dispatcher, signal
///    delivery, `exec`, the fork and clone builders reading a parent —
///    after the trampoline returned and before the next
///    [`crate::user::mode::UserMode::execute`] republishes
///    `pcr.user_ctx_ptr`.
///
/// There is no third: the only route to another task's context is
/// [`crate::task::Task::user_ctx`], which demands a `TaskExclusive`
/// witness, and that witness is `!Send`, `!Sync`, and names one task.
#[repr(C)]
pub struct UserContext {
    regs: SyncUnsafeCell<UserRegs>,
    fpu_state: FpuStateRef,
}

impl core::fmt::Debug for UserContext {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("UserContext")
            .field("regs", &self.regs())
            .field("fpu_state", &self.fpu_state)
            .finish()
    }
}

// `__ostd_user_return` loads a `*mut UserContext` out of the PCR and
// indexes it with the `UR_*` displacements above — it treats the whole
// context as its register file, so `regs` must sit at offset zero.
const _: () = assert!(core::mem::offset_of!(UserContext, regs) == 0);

// SAFETY: every field of `UserContext` is `Zeroable`
// (`UserRegs` and `FpuStateRef` impls above). `SyncUnsafeCell<T>` is
// `#[repr(transparent)]` over `UnsafeCell<T>` over `T`, so wrapping
// `UserRegs` in one leaves its bit patterns untouched and the all-zero
// one is still what `UserContext::const_zeroed()` produces.
// `#[repr(C)]` pins the layout so the impl stays well-formed under
// field reorder.
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
            regs: SyncUnsafeCell::new(UserRegs::const_zeroed()),
            fpu_state: FpuStateRef::empty(),
        }
    }

    /// Build a fresh `UserContext` with the given GPR snapshot. `cs`
    /// and `ss` are forced to the OSTD user selectors; `rflags` is
    /// passed through [`Self::set_rflags`].
    pub fn new(mut regs: UserRegs, fpu_state: FpuStateRef) -> Self {
        regs.cs = USER_CODE_SELECTOR;
        regs.ss = USER_DATA_SELECTOR;
        regs._pad = [0; 3];
        regs.rflags_user_subset = sanitize_user_rflags(regs.rflags_user_subset);
        Self {
            regs: SyncUnsafeCell::new(regs),
            fpu_state,
        }
    }

    /// Snapshot the register file.
    ///
    /// By value, not by reference: the file is written through `&self`,
    /// so a `&UserRegs` handed out here would be a shared borrow of
    /// memory the next setter mutates.
    #[inline]
    pub fn regs(&self) -> UserRegs {
        // SAFETY: the cell's contents are a plain `UserRegs`, always
        // initialised, and the write contract on this type keeps the
        // two writers from overlapping this read.
        unsafe { *self.regs.get() }
    }

    /// Raw pointer to the embedded `UserRegs`.  Used by the kernel→user
    /// round-trip asm helper (see
    /// [`crate::user::mode::user_mode_round_trip_asm`]); ordinary
    /// callers should reach for [`Self::regs`] / [`Self::set_regs`]
    /// instead.
    #[inline]
    pub fn regs_ptr(&self) -> *const UserRegs {
        self.regs.get().cast_const()
    }

    /// Direct mutable view of the embedded GPR snapshot, proven by
    /// `&mut self` rather than by the write contract.
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
        self.regs.get_mut()
    }

    /// Replace the entire register snapshot. `cs` / `ss` from `regs`
    /// are ignored — the OSTD selectors are re-applied. `rflags` is
    /// masked so a caller cannot bypass the sensitive-bits filter by
    /// writing the snapshot directly. Sanitised before the store, so an
    /// unmasked value is never briefly live in the cell.
    pub fn set_regs(&self, mut regs: UserRegs) {
        regs.cs = USER_CODE_SELECTOR;
        regs.ss = USER_DATA_SELECTOR;
        regs._pad = [0; 3];
        regs.rflags_user_subset = sanitize_user_rflags(regs.rflags_user_subset);
        // SAFETY: see the write contract on `UserContext`.
        unsafe { *self.regs.get() = regs };
    }

    #[inline]
    pub fn fpu_state(&self) -> FpuStateRef {
        self.fpu_state
    }

    #[inline]
    pub fn rax(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`. A raw place
        // read, so no reference into the cell is formed.
        unsafe { (*self.regs.get()).rax }
    }

    #[inline]
    pub fn set_rax(&self, rax: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rax = rax };
    }

    /// The six syscall argument registers in ABI order:
    /// `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9`.
    #[inline]
    pub fn syscall_args(&self) -> [u64; 6] {
        let regs = self.regs.get();
        // SAFETY: see the write contract on `UserContext`.
        unsafe {
            [
                (*regs).rdi,
                (*regs).rsi,
                (*regs).rdx,
                (*regs).r10,
                (*regs).r8,
                (*regs).r9,
            ]
        }
    }

    #[inline]
    pub fn rip(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rip }
    }

    #[inline]
    pub fn set_rip(&self, rip: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rip = rip };
    }

    #[inline]
    pub fn rsp(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rsp }
    }

    #[inline]
    pub fn set_rsp(&self, rsp: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rsp = rsp };
    }

    #[inline]
    pub fn fs_base(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).fs_base }
    }

    #[inline]
    pub fn set_fs_base(&self, fs_base: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).fs_base = fs_base };
    }

    #[inline]
    pub fn gs_base(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).gs_base }
    }

    #[inline]
    pub fn set_gs_base(&self, gs_base: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).gs_base = gs_base };
    }

    /// Write user-mode RFLAGS, masking every sensitive bit.
    ///
    /// SAFETY note (Inv. 2): the kernel never copies user-supplied
    /// RFLAGS into hardware verbatim. IOPL=0 forbids user port I/O,
    /// IF=1 keeps the user preemptible, AC=0 keeps SMAP enforcement
    /// active for kernel-mode code that runs after the next IRETQ,
    /// VM/NT/RF/VIF/VIP cleared closes the corresponding x86 escape
    /// hatches.
    pub fn set_rflags(&self, value: u64) {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rflags_user_subset = sanitize_user_rflags(value) };
    }

    /// Read the masked RFLAGS value as it will be loaded by IRETQ.
    #[inline]
    pub fn rflags(&self) -> u64 {
        // SAFETY: see the write contract on `UserContext`.
        unsafe { (*self.regs.get()).rflags_user_subset }
    }

    /// Read the raw value of a syscall argument register. Indices map
    /// to the Linux x86_64 syscall ABI: `0 → rdi`, `1 → rsi`,
    /// `2 → rdx`, `3 → r10`, `4 → r8`, `5 → r9`.
    pub fn syscall_arg(&self, reg_index: usize) -> u64 {
        match reg_index {
            0..=5 => self.syscall_args()[reg_index],
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
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags(0);
        let f = ctx.rflags();
        assert!(f & (1 << 9) != 0, "IF must be forced on");
        assert!(f & (1 << 1) != 0, "MBO bit 1 must be forced on");
    }

    #[test]
    fn set_rflags_clears_iopl() {
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags(0x3000); // IOPL = 3
        assert_eq!(ctx.rflags() & 0x3000, 0);
    }

    #[test]
    fn set_rflags_clears_ac() {
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags(1 << 18);
        assert_eq!(ctx.rflags() & (1 << 18), 0);
    }

    #[test]
    fn set_rflags_clears_nt_and_vm() {
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 14) | (1 << 17));
        assert_eq!(ctx.rflags() & ((1 << 14) | (1 << 17)), 0);
    }

    #[test]
    fn set_rflags_clears_vif_vip_and_rf() {
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 16) | (1 << 19) | (1 << 20));
        assert_eq!(ctx.rflags() & ((1 << 16) | (1 << 19) | (1 << 20)), 0);
    }

    #[test]
    fn set_rflags_preserves_id_and_user_arith_flags() {
        let ctx = ctx_with_rflags(0);
        ctx.set_rflags((1 << 21) | (1 << 0) | (1 << 6) | (1 << 11));
        let f = ctx.rflags();
        assert!(f & (1 << 21) != 0, "ID preserved");
        assert!(f & (1 << 0) != 0, "CF preserved");
        assert!(f & (1 << 6) != 0, "ZF preserved");
        assert!(f & (1 << 11) != 0, "OF preserved");
    }

    #[test]
    fn set_rflags_drops_all_other_bits() {
        let ctx = ctx_with_rflags(0);
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
