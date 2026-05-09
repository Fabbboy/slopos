//! Syscall Validation Tests
//!
//! Targets: invalid/null pointer handling, boundary conditions,
//! permission checks, resource exhaustion, and dispatch edge cases.

use core::ffi::c_char;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::scheduler::task_struct::Task;
use crate::syscall::fs::syscall_ioctl;
use crate::syscall::handlers::{
    syscall_arch_prctl, syscall_futex, syscall_getpgid, syscall_setpgid, syscall_setsid,
};
use crate::syscall::signal::{
    deliver_pending_signal, syscall_kill, syscall_rt_sigaction, syscall_rt_sigprocmask,
    syscall_rt_sigreturn,
};
use slopos_abi::addr::PhysAddr;
use slopos_abi::fs::O_RDONLY;
use slopos_abi::signal::{
    SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGUSR1, SigSet, SignalFrame, UserSigaction, sig_bit,
};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, CLONE_SETTLS, CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, ERRNO_EAGAIN,
    F_GETFL, FUTEX_WAIT, FUTEX_WAKE, MAP_ANONYMOUS, MAP_PRIVATE, O_NOCTTY, O_NONBLOCK, POLLIN,
    SYSCALL_ARCH_PRCTL, SYSCALL_CLONE, SYSCALL_FUTEX, SYSCALL_GETPGID, SYSCALL_IOCTL, SYSCALL_KILL,
    SYSCALL_NET_SCAN, SYSCALL_PIPE, SYSCALL_PIPE2, SYSCALL_POLL, SYSCALL_RT_SIGACTION,
    SYSCALL_RT_SIGPROCMASK, SYSCALL_RT_SIGRETURN, SYSCALL_SELECT, SYSCALL_SETPGID, SYSCALL_SETSID,
    SYSCALL_TABLE_SIZE, SYSCALL_VHANGUP, TIOCSCTTY, TtyIndex,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE, TaskStatus};
use slopos_mm::page_alloc::alloc_page_frame;
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::user::context::UserContext;
use slopos_testing::{TestResult, assert_eq_test, assert_not_null, assert_test};
use slopos_utils::klog_info;

use crate::scheduler::scheduler::unblock_task;
use crate::scheduler::task;
use crate::scheduler::task::{
    task_clone, task_create, task_find_by_id, task_fork, task_set_state,
    task_set_state_from_with_reason, task_terminate, task_try_transition_from,
};
use crate::syscall::handlers::syscall_lookup;
use slopos_abi::io::{KernelIoBuf, KernelIoBufRef};
use slopos_abi::task::BlockReason;
use slopos_fs::fileio::{
    file_close_fd, file_fcntl_fd, file_open_for_process, file_pipe_create, file_poll_fd,
    file_read_fd, file_write_fd, fileio_clone_table_for_process, fileio_destroy_table_for_process,
};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;

// =============================================================================
// Test Helpers
// =============================================================================

/// Wrapper around the hermetic `KernelTestScope` so existing call sites
/// (`let _f = SyscallFixture::new();`) keep working without churn.
/// The previous hand-rolled fixture leaked PCR pointers and per-CPU
/// `enabled` bits; the hermetic scope's registry walk handles every
/// such leak through the per-subsystem `HermeticState` impls in
/// `crate::scheduler::test_hermetic`.
type SyscallFixture = crate::scheduler::test_fixture::KernelTestScope;

/// Park PCR's `current_task` on the BSP bootstrap stub. Used by tests
/// that mutate the running-task pointer. The hermetic
/// `BspCurrentTask` impl restores the original value on scope drop.
fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::set_current_task(
        crate::scheduler::safestack_rt::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

fn make_task_current(task_ptr: *mut Task) {
    assert!(!task_ptr.is_null(), "make_task_current: null task_ptr");
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    crate::scheduler::scheduler::dispatch(cpu_id, task_ptr);
}

use crate::tests::helpers::dummy_task_entry;

fn create_test_kernel_task() -> u32 {
    task_create(
        b"KernelTest\0".as_ptr() as *const c_char,
        dummy_task_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_KERNEL_MODE,
    )
}

fn create_test_user_task() -> u32 {
    let user_entry = unsafe { core::mem::transmute(PROCESS_CODE_START_VA as usize) };
    let id = task_create(
        b"UserTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
    // Block the task immediately so the scheduler on other CPUs never picks
    // it up.  These tests only inspect task structures — they never need the
    // task to actually run user-mode code.
    if id != INVALID_TASK_ID {
        task_set_state(id, TaskStatus::Blocked);
    }
    id
}

/// Build a zero-initialised `UserContext` for tests. Direct GPR
/// mutations through `regs_mut()` are how tests load syscall arg
/// registers (rdi, rsi, …) before dispatching, mirroring the legacy
/// `frame.regs_mut().rdi = …` pattern.
fn zero_frame() -> UserContext {
    UserContext::const_zeroed()
}

fn pts_path_for(number: u32) -> Option<[u8; 11]> {
    if number > 9 {
        return None;
    }
    let mut path = *b"/dev/pts/0\0";
    path[9] = b'0' + number as u8;
    Some(path)
}

fn with_user_process_context<R>(pid: u32, f: impl FnOnce() -> R) -> Option<R> {
    if slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(pid) == 0 {
        return None;
    }
    // SAFETY: test scaffolding — irqs are masked by the caller's
    // test harness, kernel-half invariant always holds for an OSTD
    // VmSpace built via `VmSpace::new`.
    if !unsafe { slopos_mm::process_vm::process_vm_activate(pid) } {
        return None;
    }
    set_test_process_id(pid);
    let out = f();
    set_test_process_id(slopos_abi::task::INVALID_PROCESS_ID);
    // SAFETY: same as above; kernel master always satisfies activate's
    // invariant.
    unsafe {
        slopos_kernel_services::kernel_vm_space::kernel_vm_space()
            .lock()
            .activate();
    }
    Some(out)
}

fn user_copy_out<T: Copy>(pid: u32, addr: u64, value: &T) -> bool {
    with_user_process_context(pid, || {
        let ptr = match UserPtr::<T>::try_new(addr) {
            Ok(p) => p,
            Err(_) => return false,
        };
        copy_to_user(ptr, value).is_ok()
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(pid: u32, addr: u64) -> Option<T> {
    with_user_process_context(pid, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_page(pid: u32) -> Option<u64> {
    let base = process_vm_alloc(pid, 4096, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }

    let phys: PhysAddr = alloc_page_frame(0);
    if phys.is_null() {
        return None;
    }

    let mapped = slopos_mm::process_vm::process_vm_with_dual_paging(pid, |_pd, vs| {
        slopos_mm::dual_paging::ostd_map_4kb_user(
            vs,
            slopos_abi::addr::VirtAddr::new(base),
            phys,
            PageFlags::USER_RW.bits(),
        )
        .is_ok()
    });
    if !matches!(mapped, Some(true)) {
        return None;
    }

    Some(base)
}

// =============================================================================
// Syscall Dispatch Tests
// =============================================================================

pub fn test_syscall_lookup_invalid_number() -> TestResult {
    assert_test!(
        syscall_lookup(0xFFFF).is_null(),
        "should reject out-of-bounds"
    );
    assert_test!(
        syscall_lookup(SYSCALL_TABLE_SIZE as u64).is_null(),
        "should reject boundary"
    );
    assert_test!(syscall_lookup(u64::MAX).is_null(), "should reject u64::MAX");
    TestResult::Pass
}

pub fn test_syscall_lookup_empty_slot() -> TestResult {
    let entry = syscall_lookup(9);
    assert_test!(entry.is_null(), "unimplemented slot should return null");
    TestResult::Pass
}

pub fn test_syscall_lookup_valid() -> TestResult {
    // SYSCALL_EXIT = 1
    let entry = syscall_lookup(1);
    assert_not_null!(entry, "SYSCALL_EXIT lookup returned null");
    let entry_ref = unsafe { &*entry };
    assert_test!(entry_ref.handler.is_some(), "SYSCALL_EXIT has no handler");
    TestResult::Pass
}

pub fn test_process_syscall_lookup_valid() -> TestResult {
    let required = [
        SYSCALL_CLONE,
        SYSCALL_ARCH_PRCTL,
        SYSCALL_FUTEX,
        SYSCALL_RT_SIGACTION,
        SYSCALL_RT_SIGPROCMASK,
        SYSCALL_KILL,
        SYSCALL_RT_SIGRETURN,
    ];

    for sysno in required {
        let entry = syscall_lookup(sysno);
        assert_not_null!(entry, "required syscall missing from table");
        assert_test!(
            unsafe { (*entry).handler.is_some() },
            "required syscall has no handler"
        );
    }

    TestResult::Pass
}

pub fn test_io_syscall_lookup_valid() -> TestResult {
    let required = [
        SYSCALL_POLL,
        SYSCALL_SELECT,
        SYSCALL_PIPE,
        SYSCALL_PIPE2,
        SYSCALL_IOCTL,
        SYSCALL_SETPGID,
        SYSCALL_GETPGID,
        SYSCALL_SETSID,
    ];

    for sysno in required {
        let entry = syscall_lookup(sysno);
        assert_not_null!(entry, "required syscall missing from dispatch table");
        assert_test!(
            unsafe { (*entry).handler.is_some() },
            "required syscall has no handler in dispatch table"
        );
    }

    TestResult::Pass
}

pub fn test_net_scan_syscall_lookup_valid() -> TestResult {
    let entry = syscall_lookup(SYSCALL_NET_SCAN);
    assert_not_null!(entry, "net_scan syscall missing from table");
    assert_test!(
        unsafe { (*entry).handler.is_some() },
        "net_scan syscall has no handler"
    );
    TestResult::Pass
}

pub fn test_pipe_poll_eof_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe creation failed"
    );

    let payload = b"wheel";
    let written = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "pipe write failed");

    let revents = file_poll_fd(pid, read_fd, POLLIN);
    assert_test!((revents & POLLIN) != 0, "pipe read fd should be readable");

    let mut out = [0u8; 8];
    let read = file_read_fd(
        pid,
        read_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(read as usize, payload.len(), "pipe read length mismatch");
    assert_test!(&out[..payload.len()] == payload, "pipe payload mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write fd failed");
    let eof_read = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(eof_read, 0, "pipe EOF read should return 0");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read fd failed");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_process_group_session_syscalls_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID);
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr);

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr);

    let mut frame = zero_frame();
    let _ = syscall_getpgid(parent_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(
        frame.regs().rax as u32,
        unsafe { (*parent_ptr).pgid },
        "getpgid self mismatch"
    );

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = child_id as u64;
    setpgid_frame.regs_mut().rsi = parent_id as u64;
    let _ = syscall_setpgid(parent_ptr, &mut setpgid_frame as *mut UserContext);
    assert_eq_test!(
        setpgid_frame.regs().rax,
        0,
        "setpgid should succeed for child"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).pgid },
        parent_id,
        "child pgid mismatch after setpgid"
    );

    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame as *mut UserContext);
    assert_eq_test!(
        setsid_frame.regs().rax as u32,
        child_id,
        "setsid should return child sid"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).sid },
        child_id,
        "child sid mismatch after setsid"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).pgid },
        child_id,
        "child pgid mismatch after setsid"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_kill_process_group_semantics() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_test_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "failed to create leader task");
    let leader_ptr = task_find_by_id(leader_id);
    assert_not_null!(leader_ptr, "leader lookup failed");

    let member_id = task_fork(leader_ptr, core::ptr::null());
    assert_test!(member_id != INVALID_TASK_ID, "failed to fork member task");
    task_set_state(member_id, TaskStatus::Blocked);
    let member_ptr = task_find_by_id(member_id);
    assert_not_null!(member_ptr, "member lookup failed");

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = member_id as u64;
    setpgid_frame.regs_mut().rsi = leader_id as u64;
    let _ = syscall_setpgid(leader_ptr, &mut setpgid_frame as *mut UserContext);
    assert_eq_test!(
        setpgid_frame.regs().rax,
        0,
        "setpgid should succeed for member"
    );

    let leader_pid = unsafe { (*leader_ptr).process_id };
    let member_pid = unsafe { (*member_ptr).process_id };

    let mut probe_frame = zero_frame();
    probe_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    probe_frame.regs_mut().rsi = 0;
    let _ = with_user_process_context(leader_pid, || {
        syscall_kill(leader_ptr, &mut probe_frame as *mut UserContext)
    });
    assert_eq_test!(
        probe_frame.regs().rax,
        0,
        "kill(group, 0) probe should succeed"
    );

    unsafe {
        (*leader_ptr).signal_pending.store(0, Ordering::Release);
        (*member_ptr).signal_pending.store(0, Ordering::Release);
    }

    let mut negative_group_frame = zero_frame();
    negative_group_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    negative_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(leader_pid, || {
        syscall_kill(leader_ptr, &mut negative_group_frame as *mut UserContext)
    });
    assert_eq_test!(
        negative_group_frame.regs().rax,
        0,
        "kill(-pgid, SIGUSR1) failed"
    );

    let pending_bit = sig_bit(SIGUSR1);
    let leader_pending = unsafe { (*leader_ptr).signal_pending.load(Ordering::Acquire) };
    let member_pending = unsafe { (*member_ptr).signal_pending.load(Ordering::Acquire) };
    assert_test!(
        (leader_pending & pending_bit) != 0,
        "leader did not receive group signal"
    );
    assert_test!(
        (member_pending & pending_bit) != 0,
        "member did not receive group signal"
    );

    unsafe {
        (*leader_ptr).signal_pending.store(0, Ordering::Release);
        (*member_ptr).signal_pending.store(0, Ordering::Release);
    }

    let mut caller_group_frame = zero_frame();
    caller_group_frame.regs_mut().rdi = 0;
    caller_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(member_pid, || {
        syscall_kill(member_ptr, &mut caller_group_frame as *mut UserContext)
    });
    assert_eq_test!(caller_group_frame.regs().rax, 0, "kill(0, SIGUSR1) failed");

    let leader_pending_after = unsafe { (*leader_ptr).signal_pending.load(Ordering::Acquire) };
    let member_pending_after = unsafe { (*member_ptr).signal_pending.load(Ordering::Acquire) };
    assert_test!(
        (leader_pending_after & pending_bit) != 0,
        "leader missing kill(0) group signal"
    );
    assert_test!(
        (member_pending_after & pending_bit) != 0,
        "member missing kill(0) group signal"
    );

    task_terminate(member_id);
    task_terminate(leader_id);
    TestResult::Pass
}

pub fn test_tiocsctty_session_leader_acquires_ctty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(
        frame.regs().rax,
        0,
        "TIOCSCTTY should succeed for session leader"
    );

    let sid = unsafe { (*task_ptr).sid };
    let ctty = unsafe { (*task_ptr).controlling_tty };
    assert_eq_test!(ctty, Some(TtyIndex(0)), "controlling_tty should be tty0");

    let tty_sid =
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    assert_eq_test!(tty_sid, sid, "tty session should match caller sid");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_tiocsctty_non_leader_rejected() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(child_ptr, &mut frame as *mut UserContext);
    assert_test!(
        frame.regs().rax != 0,
        "TIOCSCTTY should fail for non-session leader"
    );

    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child ctty should remain None"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_open_dev_tty_with_o_noctty_preserves_flag() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(
        frame.regs().rax,
        0,
        "TIOCSCTTY should succeed before /dev/tty open"
    );

    let pid = unsafe { (*task_ptr).process_id };
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, b"/dev/tty", O_RDONLY | O_NOCTTY as u32);
    park_bootstrap_on_current_cpu();
    assert_test!(fd >= 0, "open(/dev/tty, O_NOCTTY) failed");

    let flags = file_fcntl_fd(pid, fd, F_GETFL, 0);
    let close_rc = file_close_fd(pid, fd);
    task_terminate(task_id);

    assert_eq_test!(close_rc, 0, "close /dev/tty fd failed");
    assert_test!(flags >= 0, "F_GETFL failed for /dev/tty fd");
    assert_test!(
        (flags as u64 & O_NOCTTY) != 0,
        "F_GETFL should preserve O_NOCTTY on /dev/tty fd"
    );

    TestResult::Pass
}

pub fn test_setsid_child_preserves_parent_controlling_tty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.regs_mut().rdi = 0;
    ioctl_frame.regs_mut().rsi = TIOCSCTTY;
    ioctl_frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut ioctl_frame as *mut UserContext);
    assert_eq_test!(ioctl_frame.regs().rax, 0, "parent TIOCSCTTY should succeed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame as *mut UserContext);
    assert_eq_test!(
        setsid_frame.regs().rax as u32,
        child_id,
        "setsid should succeed for child"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child should drop inherited ctty"
    );
    assert_eq_test!(
        unsafe { (*parent_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "parent should retain controlling tty"
    );

    let tty_sid =
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    assert_eq_test!(
        tty_sid,
        parent_id,
        "tty session should stay attached to parent session"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_hangup_clears_all_session_controlling_ttys() -> TestResult {
    let _fixture = SyscallFixture::new();

    let leader_id = create_test_user_task();
    assert_test!(leader_id != INVALID_TASK_ID, "failed to create leader task");
    let leader_ptr = task_find_by_id(leader_id);
    assert_not_null!(leader_ptr, "leader lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.regs_mut().rdi = 0;
    ioctl_frame.regs_mut().rsi = TIOCSCTTY;
    ioctl_frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(leader_ptr, &mut ioctl_frame as *mut UserContext);
    assert_eq_test!(ioctl_frame.regs().rax, 0, "leader TIOCSCTTY should succeed");

    let child_id = task_fork(leader_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    slopos_kernel_services::syscall_services::tty::hangup(TtyIndex(0));

    assert_eq_test!(
        unsafe { (*leader_ptr).controlling_tty },
        None,
        "leader ctty should clear on hangup"
    );
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "child ctty should clear on hangup"
    );
    assert_eq_test!(
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(0)).unwrap_or(0),
        0,
        "tty session should detach on hangup"
    );

    task_terminate(child_id);
    task_terminate(leader_id);
    TestResult::Pass
}

pub fn test_pts_open_acquires_controlling_tty_without_o_noctty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let master_idx = match slopos_kernel_services::syscall_services::tty::alloc_pty() {
        Ok(idx) => idx,
        Err(_) => return TestResult::Fail,
    };
    let slave_number =
        match slopos_kernel_services::syscall_services::tty::get_pty_number(master_idx) {
            Ok(n) => n,
            Err(_) => return TestResult::Fail,
        };
    let path = match pts_path_for(slave_number) {
        Some(path) => path,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Unlock slave so /dev/pts/N open succeeds.
    let _ = slopos_kernel_services::syscall_services::tty::set_pty_lock(master_idx, false);

    let pid = unsafe { (*task_ptr).process_id };
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N) failed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        Some(TtyIndex(slave_number as u8)),
        "PTY slave open should acquire controlling tty"
    );
    assert_eq_test!(
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(slave_number as u8))
            .unwrap_or(0),
        task_id,
        "PTY slave session should match task session"
    );

    let _ = file_close_fd(pid, fd);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_pts_open_with_o_noctty_skips_controlling_tty_acquire() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let master_idx = match slopos_kernel_services::syscall_services::tty::alloc_pty() {
        Ok(idx) => idx,
        Err(_) => return TestResult::Fail,
    };
    let slave_number =
        match slopos_kernel_services::syscall_services::tty::get_pty_number(master_idx) {
            Ok(n) => n,
            Err(_) => return TestResult::Fail,
        };
    let path = match pts_path_for(slave_number) {
        Some(path) => path,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Unlock slave so /dev/pts/N open succeeds.
    let _ = slopos_kernel_services::syscall_services::tty::set_pty_lock(master_idx, false);

    let pid = unsafe { (*task_ptr).process_id };
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N, O_NOCTTY) failed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        None,
        "O_NOCTTY should prevent ctty acquire"
    );
    assert_eq_test!(
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(slave_number as u8))
            .unwrap_or(0),
        0,
        "O_NOCTTY open should leave PTY session unattached"
    );

    let _ = file_close_fd(pid, fd);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_vm_mmap_munmap_stress_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = unsafe { (*task_ptr).process_id };

    for _ in 0..128 {
        let addr = slopos_mm::process_vm::process_vm_mmap(
            pid,
            0,
            4096,
            slopos_abi::syscall::PROT_READ | slopos_abi::syscall::PROT_WRITE,
            MAP_PRIVATE | MAP_ANONYMOUS,
            -1,
            0,
        );
        if addr == 0 {
            task_terminate(task_id);
            return TestResult::Fail;
        }
        if slopos_mm::process_vm::process_vm_munmap(pid, addr, 4096) != 0 {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// Fork Edge Case Tests
// =============================================================================

pub fn test_fork_null_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(ptr::null_mut(), core::ptr::null());
    assert_test!(
        child_id == INVALID_TASK_ID,
        "fork with null parent should fail"
    );
    TestResult::Pass
}

pub fn test_fork_kernel_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let kernel_task_id = create_test_kernel_task();
    assert_test!(kernel_task_id != INVALID_TASK_ID);

    let kernel_task = task_find_by_id(kernel_task_id);
    assert_not_null!(kernel_task);

    use crate::scheduler::task::task_fork;
    let child_id = task_fork(kernel_task, core::ptr::null());
    assert_test!(
        child_id == INVALID_TASK_ID,
        "kernel tasks should not be forkable"
    );

    task_terminate(kernel_task_id);
    TestResult::Pass
}

pub fn test_fork_terminated_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::task_fork;

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_terminate(task_id);

    let task_ptr_after = task_find_by_id(task_id);
    if !task_ptr_after.is_null() {
        let state = unsafe { (*task_ptr_after).status() };
        if state == TaskStatus::Terminated {
            let child_id = task_fork(task_ptr_after, core::ptr::null());
            assert_test!(
                child_id == INVALID_TASK_ID,
                "fork terminated parent should fail"
            );
        }
    }

    TestResult::Pass
}

pub fn test_fork_blocked_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    use crate::scheduler::task::{task_fork, task_set_state};

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_set_state(task_id, TaskStatus::Blocked);

    let child_id = task_fork(task_ptr, core::ptr::null());

    task_terminate(task_id);
    if child_id != INVALID_TASK_ID {
        task_terminate(child_id);
    }

    TestResult::Pass
}

pub fn test_fork_cleanup_on_failure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..5 {
        let _ = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
    }

    for _ in 0..3 {
        let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);
        if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
            slopos_mm::process_vm::destroy_process_vm(child_pid);
        }
    }

    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 64, "memory leak after fork cleanup: {} pages", leak);

    TestResult::Pass
}

// =============================================================================
// Pointer Validation Tests
// =============================================================================

pub fn test_user_ptr_null() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    assert_test!(
        UserPtr::<u64>::try_new(0).is_err(),
        "null address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_kernel_address() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let kernel_addr: u64 = 0xFFFF_8000_0000_0000;
    assert_test!(
        UserPtr::<u64>::try_new(kernel_addr).is_err(),
        "kernel address should be rejected"
    );
    TestResult::Pass
}

pub fn test_user_ptr_misaligned() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let _result = UserPtr::<u64>::try_new(0x1001);
    // Just verify it doesn't crash; alignment policy is implementation-defined
    TestResult::Pass
}

pub fn test_user_ptr_overflow_boundary() -> TestResult {
    use slopos_mm::user_ptr::UserPtr;
    let near_max: u64 = u64::MAX - 4;
    assert_test!(
        UserPtr::<u64>::try_new(near_max).is_err(),
        "overflow-prone address should be rejected"
    );
    TestResult::Pass
}

// =============================================================================
// Syscall Argument Boundary Tests
// =============================================================================

pub fn test_brk_extreme_values() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();
    let pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(pid != slopos_abi::task::INVALID_PROCESS_ID);

    let current_brk = slopos_mm::process_vm::process_vm_brk(pid, 0);
    if current_brk == 0 {
        klog_info!("SYSCALL_TEST: Initial brk returned 0 (might be a bug)");
    }

    let max_brk = slopos_mm::process_vm::process_vm_brk(pid, u64::MAX);
    assert_test!(max_brk != u64::MAX, "brk accepted u64::MAX");

    let kernel_brk = slopos_mm::process_vm::process_vm_brk(pid, 0xFFFF_8000_0000_0000);
    assert_test!(
        kernel_brk != 0xFFFF_8000_0000_0000,
        "brk accepted kernel address"
    );

    slopos_mm::process_vm::destroy_process_vm(pid);
    TestResult::Pass
}

pub fn test_memfd_create_boundaries() -> TestResult {
    // Test memfd_create + ftruncate basics
    let result = slopos_mm::memfd::memfd_create(0);
    assert_test!(result.is_some(), "memfd_create should succeed");
    if let Some((handle, _ops)) = result {
        // ftruncate with zero size should fail
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 0);
        assert_test!(rc < 0, "ftruncate(0) should fail");

        // ftruncate with valid size should succeed
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 4096);
        assert_eq_test!(rc, 0, "ftruncate(4096) should succeed");

        // ftruncate again should fail (one-shot)
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 8192);
        assert_test!(rc < 0, "ftruncate twice should fail");

        // Cleanup
        slopos_mm::memfd::memfd_release(handle);
    }
    TestResult::Pass
}

// =============================================================================
// Task State Corruption Tests
// =============================================================================

pub fn test_terminate_already_terminated() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    assert_eq_test!(task_terminate(task_id), 0, "first termination failed");

    // Second termination should not crash
    let _r2 = task_terminate(task_id);

    let task_ptr = task_find_by_id(task_id);
    if !task_ptr.is_null() {
        let state = unsafe { (*task_ptr).status() };
        assert_test!(state != TaskStatus::Ready, "terminated task in READY state");
    }

    TestResult::Pass
}

pub fn test_operations_on_terminated_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);

    use crate::scheduler::task::task_get_info;
    let mut task_ptr: *mut Task = ptr::null_mut();
    let _info_result = task_get_info(task_id, &mut task_ptr);

    use crate::scheduler::task::task_set_state;
    let state_result = task_set_state(task_id, TaskStatus::Ready);
    if state_result == 0 {
        let task = task_find_by_id(task_id);
        if !task.is_null() {
            let current_state = unsafe { (*task).status() };
            assert_test!(
                current_state != TaskStatus::Ready,
                "revived terminated task"
            );
        }
    }

    TestResult::Pass
}

// =============================================================================
// Memory Pressure During Syscall Tests
// =============================================================================

pub fn test_fork_memory_pressure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..10 {
        let addr = slopos_mm::process_vm::process_vm_alloc(
            parent_pid,
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
        if addr == 0 {
            break;
        }
    }

    use slopos_abi::addr::PhysAddr;
    use slopos_mm::page_alloc::{ALLOC_FLAG_NO_PCP, alloc_page_frame, free_page_frame};

    let mut stress_pages: [PhysAddr; 128] = [PhysAddr::NULL; 128];
    let mut stress_count = 0usize;

    for _ in 0..128 {
        let phys = alloc_page_frame(ALLOC_FLAG_NO_PCP);
        if phys.is_null() {
            break;
        }
        stress_pages[stress_count] = phys;
        stress_count += 1;
    }

    let child_pid = slopos_mm::process_vm::process_vm_clone_cow(parent_pid);

    let mut free_before = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_before,
        ptr::null_mut(),
    );

    if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
        slopos_mm::process_vm::destroy_process_vm(child_pid);
    }
    slopos_mm::process_vm::destroy_process_vm(parent_pid);

    for i in 0..stress_count {
        free_page_frame(stress_pages[i]);
    }

    let mut free_after = 0u32;
    slopos_mm::page_alloc::get_page_allocator_stats(
        ptr::null_mut(),
        &mut free_after,
        ptr::null_mut(),
    );

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 32, "memory leak under pressure: {} pages", leak);

    TestResult::Pass
}

pub fn test_task_id_wraparound() -> TestResult {
    let _fixture = SyscallFixture::new();

    let mut ids_seen: [u32; 256] = [INVALID_TASK_ID; 256];
    let mut seen_count = 0usize;

    for _i in 0..500 {
        let id = task_create(
            b"WrapTest\0".as_ptr() as *const c_char,
            dummy_task_entry,
            ptr::null_mut(),
            1,
            TASK_FLAG_KERNEL_MODE,
        );

        if id == INVALID_TASK_ID {
            continue;
        }

        for j in 0..seen_count {
            assert_test!(ids_seen[j] != id, "duplicate task ID {}", id);
        }

        if seen_count < ids_seen.len() {
            ids_seen[seen_count] = id;
            seen_count += 1;
        }

        task_terminate(id);
    }

    TestResult::Pass
}

pub fn test_clone_thread_tls_isolation() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    unsafe {
        (*parent_ptr).fs_base = 0x0000_1111_2222_3000;
    }

    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;
    let child_id = match task_clone(parent_ptr, flags, 0, 0, 0, 0x0000_5555_6666_7000) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*child_ptr).tgid,
            (*parent_ptr).tgid,
            "thread did not join parent thread-group"
        );
        assert_eq_test!(
            (*child_ptr).fs_base,
            0x0000_5555_6666_7000,
            "child TLS base not set by CLONE_SETTLS"
        );
        assert_eq_test!(
            (*parent_ptr).fs_base,
            0x0000_1111_2222_3000,
            "parent TLS base unexpectedly modified"
        );
    }

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_clone_then_fork_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(
        parent_id != INVALID_TASK_ID,
        "failed to create parent user task"
    );
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent task lookup failed");

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(parent_ptr, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let fork_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(fork_id != INVALID_TASK_ID, "fork after clone failed");
    task_set_state(fork_id, TaskStatus::Blocked);

    let thread_ptr = task_find_by_id(thread_id);
    let fork_ptr = task_find_by_id(fork_id);
    assert_not_null!(thread_ptr, "thread task lookup failed");
    assert_not_null!(fork_ptr, "fork child task lookup failed");

    unsafe {
        assert_eq_test!(
            (*thread_ptr).tgid,
            (*parent_ptr).tgid,
            "thread tgid mismatch"
        );
        assert_eq_test!(
            (*fork_ptr).tgid,
            fork_id,
            "fork child should be its own thread-group leader"
        );
        assert_eq_test!(
            (*fork_ptr).parent_task_id,
            parent_id,
            "fork child parent id mismatch"
        );
    }

    task_terminate(fork_id);
    task_terminate(thread_id);
    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_futex_wait_mismatch_and_wake_no_waiters() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wait_frame = zero_frame();
    wait_frame.regs_mut().rdi = uaddr;
    wait_frame.regs_mut().rsi = FUTEX_WAIT;
    wait_frame.regs_mut().rdx = 2;
    wait_frame.regs_mut().r10 = 0;
    let _ = with_user_process_context(pid, || {
        syscall_futex(task_ptr, &mut wait_frame as *mut UserContext)
    });
    assert_eq_test!(
        wait_frame.regs().rax,
        ERRNO_EAGAIN,
        "FUTEX_WAIT mismatch must return -EAGAIN"
    );

    let mut wake_frame = zero_frame();
    wake_frame.regs_mut().rdi = uaddr;
    wake_frame.regs_mut().rsi = FUTEX_WAKE;
    wake_frame.regs_mut().rdx = 1;
    let _ = with_user_process_context(pid, || {
        syscall_futex(task_ptr, &mut wake_frame as *mut UserContext)
    });
    assert_eq_test!(
        wake_frame.regs().rax,
        0,
        "FUTEX_WAKE with no waiters must return 0"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_lost_wakeup_regression() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    let mut wake_frame = zero_frame();
    wake_frame.regs_mut().rdi = uaddr;
    wake_frame.regs_mut().rsi = FUTEX_WAKE;
    wake_frame.regs_mut().rdx = 1;
    let _ = with_user_process_context(pid, || {
        syscall_futex(task_ptr, &mut wake_frame as *mut UserContext)
    });
    assert_eq_test!(
        wake_frame.regs().rax,
        0,
        "initial FUTEX_WAKE should wake no waiters"
    );

    let mut wait_frame = zero_frame();
    wait_frame.regs_mut().rdi = uaddr;
    wait_frame.regs_mut().rsi = FUTEX_WAIT;
    wait_frame.regs_mut().rdx = 2;
    wait_frame.regs_mut().r10 = 0;
    let _ = with_user_process_context(pid, || {
        syscall_futex(task_ptr, &mut wait_frame as *mut UserContext)
    });
    assert_eq_test!(
        wait_frame.regs().rax,
        ERRNO_EAGAIN,
        "post-wake mismatch must return -EAGAIN"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_futex_contention_path_stability() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let uaddr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_test!(
        user_copy_out(pid, uaddr, &1u32),
        "failed to initialize futex word"
    );

    for i in 0..64u64 {
        let mut wake_frame = zero_frame();
        wake_frame.regs_mut().rdi = uaddr;
        wake_frame.regs_mut().rsi = FUTEX_WAKE;
        wake_frame.regs_mut().rdx = (i % 4) + 1;
        let _ = with_user_process_context(pid, || {
            syscall_futex(task_ptr, &mut wake_frame as *mut UserContext)
        });
        if wake_frame.regs().rax > wake_frame.regs().rdx {
            task_terminate(task_id);
            return TestResult::Fail;
        }

        let mut wait_frame = zero_frame();
        wait_frame.regs_mut().rdi = uaddr;
        wait_frame.regs_mut().rsi = FUTEX_WAIT;
        wait_frame.regs_mut().rdx = 2;
        wait_frame.regs_mut().r10 = 0;
        let _ = with_user_process_context(pid, || {
            syscall_futex(task_ptr, &mut wait_frame as *mut UserContext)
        });
        if wait_frame.regs().rax != ERRNO_EAGAIN {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_install_deliver_and_sigreturn() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let new_action_addr = page;
    let old_action_addr = page + 128;

    let new_action = UserSigaction {
        sa_handler: 0x4001_0000,
        sa_flags: 0,
        sa_restorer: 0x4002_0000,
        sa_mask: sig_bit(SIGCHLD),
    };
    assert_test!(
        user_copy_out(pid, new_action_addr, &new_action),
        "failed to write new sigaction"
    );

    let mut action_frame = zero_frame();
    action_frame.regs_mut().rdi = SIGUSR1 as u64;
    action_frame.regs_mut().rsi = new_action_addr;
    action_frame.regs_mut().rdx = old_action_addr;
    action_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        syscall_rt_sigaction(task_ptr, &mut action_frame as *mut UserContext)
    });
    assert_eq_test!(action_frame.regs().rax, 0, "rt_sigaction failed");

    let old_action: UserSigaction = match user_copy_in(pid, old_action_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        old_action.sa_handler,
        0,
        "initial old action should be SIG_DFL"
    );

    let stack_top = process_vm_get_stack_top(pid);
    let original_rsp = stack_top.wrapping_sub(0x200);
    let original_rip = 0x5000_1234;

    let mut kill_frame = zero_frame();
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || {
        syscall_kill(task_ptr, &mut kill_frame as *mut UserContext)
    });
    assert_eq_test!(kill_frame.regs().rax, 0, "kill(SIGUSR1) failed");

    let mut user_frame = zero_frame();
    user_frame.regs_mut().rip = original_rip;
    user_frame.regs_mut().rsp = original_rsp;
    user_frame.regs_mut().rax = 0xAA55;
    user_frame.regs_mut().rbx = 0xBB66;
    let _ = with_user_process_context(pid, || {
        deliver_pending_signal(task_ptr, &mut user_frame as *mut UserContext)
    });

    assert_eq_test!(
        user_frame.regs().rip,
        new_action.sa_handler,
        "signal handler RIP not installed"
    );
    assert_eq_test!(
        user_frame.regs().rdi,
        SIGUSR1 as u64,
        "signal number not passed in RDI"
    );

    // The restorer address is pushed as a separate u64 at [rsp].
    // The SignalFrame starts at [rsp + 8].
    let restorer_on_stack: u64 = match user_copy_in(pid, user_frame.regs().rsp) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        restorer_on_stack,
        new_action.sa_restorer,
        "signal restorer mismatch"
    );

    let sigframe_addr = user_frame.regs().rsp.wrapping_add(8);
    let sigframe: SignalFrame = match user_copy_in(pid, sigframe_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        sigframe.rip,
        original_rip,
        "saved RIP mismatch in signal frame"
    );
    assert_eq_test!(
        sigframe.rsp,
        original_rsp,
        "saved RSP mismatch in signal frame"
    );

    // Simulate handler's `ret`: it pops the restorer, advancing RSP by 8
    // so it now points at the SignalFrame — matching the real flow.
    user_frame.regs_mut().rsp = user_frame.regs().rsp.wrapping_add(8);
    let _ = with_user_process_context(pid, || {
        syscall_rt_sigreturn(task_ptr, &mut user_frame as *mut UserContext)
    });
    assert_eq_test!(
        user_frame.regs().rip,
        original_rip,
        "rt_sigreturn did not restore RIP"
    );
    assert_eq_test!(
        user_frame.regs().rsp,
        original_rsp,
        "rt_sigreturn did not restore RSP"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigprocmask_block_then_unblock_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let set_addr = page;
    let old_addr = page + 128;
    let act_addr = page + 256;

    let action = UserSigaction {
        sa_handler: 0x4003_0000,
        sa_flags: 0,
        sa_restorer: 0x4004_0000,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, act_addr, &action),
        "failed to write action"
    );

    let mut install_frame = zero_frame();
    install_frame.regs_mut().rdi = SIGUSR1 as u64;
    install_frame.regs_mut().rsi = act_addr;
    install_frame.regs_mut().rdx = 0;
    install_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        syscall_rt_sigaction(task_ptr, &mut install_frame as *mut UserContext)
    });
    assert_eq_test!(install_frame.regs().rax, 0, "sigaction install failed");

    let block_set: SigSet = sig_bit(SIGUSR1);
    assert_test!(
        user_copy_out(pid, set_addr, &block_set),
        "failed to write block set"
    );

    let mut block_frame = zero_frame();
    block_frame.regs_mut().rdi = SIG_SETMASK as u64;
    block_frame.regs_mut().rsi = set_addr;
    block_frame.regs_mut().rdx = old_addr;
    block_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        syscall_rt_sigprocmask(task_ptr, &mut block_frame as *mut UserContext)
    });
    assert_eq_test!(
        block_frame.regs().rax,
        0,
        "rt_sigprocmask(SIG_SETMASK) failed"
    );

    let mut kill_frame = zero_frame();
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || {
        syscall_kill(task_ptr, &mut kill_frame as *mut UserContext)
    });
    assert_eq_test!(kill_frame.regs().rax, 0, "kill(SIGUSR1) failed");

    let stack_top = process_vm_get_stack_top(pid);
    let mut user_frame = zero_frame();
    user_frame.regs_mut().rip = 0x6000_1111;
    user_frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
    let _ = with_user_process_context(pid, || {
        deliver_pending_signal(task_ptr, &mut user_frame as *mut UserContext)
    });
    assert_eq_test!(
        user_frame.regs().rip,
        0x6000_1111,
        "blocked signal should not be delivered"
    );

    let mut unblock_frame = zero_frame();
    unblock_frame.regs_mut().rdi = SIG_UNBLOCK as u64;
    unblock_frame.regs_mut().rsi = set_addr;
    unblock_frame.regs_mut().rdx = 0;
    unblock_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        syscall_rt_sigprocmask(task_ptr, &mut unblock_frame as *mut UserContext)
    });
    assert_eq_test!(
        unblock_frame.regs().rax,
        0,
        "rt_sigprocmask(SIG_UNBLOCK) failed"
    );

    let _ = with_user_process_context(pid, || {
        deliver_pending_signal(task_ptr, &mut user_frame as *mut UserContext)
    });
    assert_eq_test!(
        user_frame.regs().rip,
        action.sa_handler,
        "unblocked pending signal was not delivered"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigchld_and_wait_interaction() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "task_fork failed");
    task_set_state(child_id, TaskStatus::Blocked);

    unsafe {
        (*parent_ptr).waiting_on.store(child_id, Ordering::Release);
    }
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Running),
        0,
        "failed to set parent running"
    );
    assert_eq_test!(
        task_set_state(parent_id, TaskStatus::Blocked),
        0,
        "failed to block parent"
    );

    assert_eq_test!(task_terminate(child_id), 0, "failed to terminate child");

    unsafe {
        let pending = (*parent_ptr).signal_pending.load(Ordering::Acquire);
        assert_test!(
            (pending & sig_bit(SIGCHLD)) != 0,
            "parent missing SIGCHLD pending bit"
        );
        assert_eq_test!(
            (*parent_ptr).waiting_on.load(Ordering::Acquire),
            INVALID_TASK_ID,
            "parent wait target not cleared after child exit"
        );
        assert_eq_test!(
            (*parent_ptr).status(),
            TaskStatus::Ready,
            "parent not readied after child exit"
        );
    }

    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_arch_prctl_set_get_fs_roundtrip() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let out_addr = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    let expected_fs = 0x0000_4000_5678_9000u64;
    let mut set_frame = zero_frame();
    set_frame.regs_mut().rdi = ARCH_SET_FS;
    set_frame.regs_mut().rsi = expected_fs;
    let _ = with_user_process_context(pid, || {
        syscall_arch_prctl(task_ptr, &mut set_frame as *mut UserContext)
    });
    assert_eq_test!(set_frame.regs().rax, 0, "ARCH_SET_FS failed");

    let mut get_frame = zero_frame();
    get_frame.regs_mut().rdi = ARCH_GET_FS;
    get_frame.regs_mut().rsi = out_addr;
    let _ = with_user_process_context(pid, || {
        syscall_arch_prctl(task_ptr, &mut get_frame as *mut UserContext)
    });
    assert_eq_test!(get_frame.regs().rax, 0, "ARCH_GET_FS failed");

    let got_fs: u64 = match user_copy_in(pid, out_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(got_fs, expected_fs, "ARCH_GET_FS returned wrong value");

    let child_no_settls = match task_clone(
        task_ptr,
        CLONE_VM | CLONE_SIGHAND | CLONE_THREAD,
        0,
        0,
        0,
        0,
    ) {
        Ok(id) => id,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let child_ptr = task_find_by_id(child_no_settls);
    assert_not_null!(child_ptr, "clone child lookup failed");
    unsafe {
        assert_eq_test!(
            (*child_ptr).fs_base,
            expected_fs,
            "clone without CLONE_SETTLS must inherit FS base"
        );
    }

    task_terminate(child_no_settls);
    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// Pipe Blocking & EOF Tests
// =============================================================================

/// Basic pipe write-then-read: write "hello", read it back, verify content.
pub fn test_pipe_write_read_basic() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let payload = b"hello";
    let written = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(
        written as usize,
        payload.len(),
        "write returned wrong count"
    );

    let mut out = [0u8; 16];
    let nread = file_read_fd(
        pid,
        read_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(nread as usize, payload.len(), "read returned wrong count");
    assert_test!(&out[..payload.len()] == payload, "read payload mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// EOF returns 0, not -1: write data, close writer, read data, read again for EOF.
pub fn test_pipe_eof_returns_zero() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let payload = b"data";
    let written = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write failed");

    // Close the write end before reading -- this sets up the EOF condition.
    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");

    // First read: should return the data.
    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(nread as usize, payload.len(), "first read wrong count");
    assert_test!(
        &out[..payload.len()] == payload,
        "first read payload mismatch"
    );

    // Second read: pipe empty + no writers = EOF (0), NOT error (-1).
    let eof = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(eof, 0, "EOF read should return 0, not -1");

    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Broken pipe: writing to a pipe with no readers should return -1.
pub fn test_pipe_broken_pipe() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Close read end first, then try to write.
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");

    let payload = b"orphan";
    let result = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(result, -32, "write to broken pipe should return EPIPE(-32)");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Multiple writes accumulate: write "aaa" then "bbb", read should yield "aaabbb".
pub fn test_pipe_multi_write_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let a = b"aaa";
    let b = b"bbb";
    let w1 = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(a));
    assert_eq_test!(w1 as usize, a.len(), "first write failed");

    let w2 = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(b));
    assert_eq_test!(w2 as usize, b.len(), "second write failed");

    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(nread as usize, 6, "read should return all 6 bytes");
    assert_test!(&out[..6] == b"aaabbb", "accumulated data mismatch");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Partial read: write 100 bytes, read 50, then read remaining 50.
pub fn test_pipe_partial_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Write 100 bytes (pattern: 0..99)
    let mut payload = [0u8; 100];
    for i in 0..100 {
        payload[i] = i as u8;
    }
    let written = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&payload));
    assert_eq_test!(written as usize, 100, "write 100 bytes failed");

    // Read first 50
    let mut buf1 = [0u8; 50];
    let r1 = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut buf1));
    assert_eq_test!(r1 as usize, 50, "first partial read wrong count");
    assert_test!(
        &buf1[..] == &payload[..50],
        "first partial read data mismatch"
    );

    // Read remaining 50
    let mut buf2 = [0u8; 50];
    let r2 = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut buf2));
    assert_eq_test!(r2 as usize, 50, "second partial read wrong count");
    assert_test!(
        &buf2[..] == &payload[50..100],
        "second partial read data mismatch"
    );

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Buffer full: fill the 4096-byte pipe buffer, then try to write 1 more byte
/// in non-blocking mode -- should return EAGAIN (-11).
pub fn test_pipe_buffer_full() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    // Create pipe with O_NONBLOCK so writes don't block when full.
    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create (nonblock) failed"
    );

    // Fill the pipe buffer (4096 bytes) in chunks.
    let chunk = [0xABu8; 512];
    let mut total_written: usize = 0;
    for _ in 0..8 {
        let w = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&chunk));
        assert_test!(w > 0, "write chunk failed while filling buffer");
        total_written += w as usize;
    }
    assert_eq_test!(total_written, 4096, "did not fill pipe buffer to 4096");

    // Now the pipe should be full. A non-blocking write of 1 byte should return EAGAIN.
    let extra = [0xCDu8; 1];
    let over = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&extra));
    assert_eq_test!(over, -11, "write to full pipe should return EAGAIN (-11)");

    // Also verify reading from an empty non-blocking pipe returns EAGAIN.
    // First drain the buffer.
    let mut drain = [0u8; 4096];
    let drained = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut drain));
    assert_eq_test!(drained as usize, 4096, "drain read wrong count");

    // Pipe is now empty with writers still open: non-blocking read should return EAGAIN.
    let mut one = [0u8; 1];
    let empty_read = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_eq_test!(
        empty_read,
        -11,
        "read from empty nonblock pipe should return EAGAIN (-11)"
    );

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

/// Regression: when the current task exits, its file table must be destroyed
/// so pipe writer refs are released and peer readers observe EOF.
pub fn test_exit_current_task_releases_pipe_refs() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );

    let p1 = task_find_by_id(t1);
    let p2 = task_find_by_id(t2);
    assert_not_null!(p1, "task1 lookup failed");
    assert_not_null!(p2, "task2 lookup failed");

    let pid1 = unsafe { (*p1).process_id };
    let pid2 = unsafe { (*p2).process_id };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Replace pid2's default console table with a clone of pid1.
    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2),
        0,
        "file table clone failed"
    );

    // Keep only the read end in pid2.
    assert_eq_test!(file_close_fd(pid2, write_fd), 0, "pid2 close write failed");

    // Make task1 appear as current so task_terminate() takes the current-task
    // cleanup path (the path that previously leaked file descriptors).
    make_task_current(p1);
    assert_eq_test!(task::task_terminate(t1), 0, "current-task terminate failed");
    park_bootstrap_on_current_cpu();

    // If writer refs were released correctly, empty nonblocking read returns EOF (0),
    // not EAGAIN (-11).
    let mut one = [0u8; 1];
    let r = file_read_fd(pid2, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_eq_test!(r, 0, "reader should observe EOF after current task exit");

    assert_eq_test!(file_close_fd(pid2, read_fd), 0, "pid2 close read failed");
    task_terminate(t2);
    TestResult::Pass
}

/// Regression test for the stale-argv spawn bug (compositor spawn failure).
///
/// Before the fix, `spawn_path_with_attrs()` used `syscall4` which left r8/r9
/// (argv_ptr / argc) as stale garbage.  The kernel handler would try to parse
/// argv from those garbage addresses, returning EINVAL instead of reaching the
/// actual exec path.  After the fix (syscall6 with explicit 0,0), r8/r9 are
/// always zero when no argv is passed.
///
/// This test verifies the two code paths return distinguishable errors:
///   - Garbage r8/r9 → argv parsing failure → ERRNO_EINVAL (-22)
///   - Zero   r8/r9 → exec path reached    → ExecError::NoEntry (-2)
pub fn test_spawn_path_stale_argv_regression() -> TestResult {
    use crate::syscall::handlers::syscall_spawn_path;
    use slopos_abi::syscall::ERRNO_EINVAL;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    // Map a user-accessible page and write a path that will fail at VFS open.
    let user_page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let path = b"/bin/noent";
    assert_test!(
        user_copy_out(pid, user_page, path),
        "failed to write path into user memory"
    );

    // ---- Case A: Stale/garbage r8 (argv_ptr) and r9 (argc) ----
    // Simulates the pre-fix bug where syscall4 left r8/r9 with junk.
    let mut frame_stale = zero_frame();
    frame_stale.regs_mut().rdi = user_page; // arg0 = path_ptr
    frame_stale.regs_mut().rsi = path.len() as u64; // arg1 = path_len
    frame_stale.regs_mut().rdx = 1; // arg2 = priority
    frame_stale.regs_mut().r10 = 0; // arg3 = flags
    frame_stale.regs_mut().r8 = 0xDEAD_BEEF_CAFE_BABEu64; // arg4 = garbage argv_ptr
    frame_stale.regs_mut().r9 = 42; // arg5 = garbage argc
    let _ = with_user_process_context(pid, || {
        syscall_spawn_path(task_ptr, &mut frame_stale as *mut UserContext)
    });
    assert_eq_test!(
        frame_stale.regs().rax,
        ERRNO_EINVAL,
        "stale argv must trigger EINVAL from argv parsing failure"
    );

    // ---- Case B: Clean r8=0, r9=0 (the fixed behavior) ----
    // With no argv, the handler skips argv parsing and reaches the exec path.
    let mut frame_clean = zero_frame();
    frame_clean.regs_mut().rdi = user_page; // arg0 = path_ptr
    frame_clean.regs_mut().rsi = path.len() as u64; // arg1 = path_len
    frame_clean.regs_mut().rdx = 1; // arg2 = priority
    frame_clean.regs_mut().r10 = 0; // arg3 = flags
    frame_clean.regs_mut().r8 = 0; // arg4 = no argv
    frame_clean.regs_mut().r9 = 0; // arg5 = no argc
    let _ = with_user_process_context(pid, || {
        syscall_spawn_path(task_ptr, &mut frame_clean as *mut UserContext)
    });

    // ExecError::NoEntry = -2, returned via ctx.ok(err as i32 as u64)
    let exec_no_entry = (-2i32) as u64;
    assert_eq_test!(
        frame_clean.regs().rax,
        exec_no_entry,
        "clean argv must reach exec path and return NoEntry for missing binary"
    );

    // The two error codes must differ — that's the whole point of this regression.
    assert_test!(
        frame_stale.regs().rax != frame_clean.regs().rax,
        "stale and clean argv paths must return different error codes"
    );

    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// /dev/tty Controlling Terminal Device
// =============================================================================

/// A freshly created task with no controlling terminal cannot open
/// `/dev/tty` — the open must return ENXIO (-6).
pub fn test_dev_tty_no_ctty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // The task has no controlling terminal by default.
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        None,
        "fresh task should have no controlling_tty"
    );

    let pid = unsafe { (*task_ptr).process_id };
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, b"/dev/tty", O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        fd,
        -6,
        "open(/dev/tty) without ctty should return -6 (ENXIO)"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// After acquiring a controlling terminal via TIOCSCTTY, opening
/// `/dev/tty` succeeds and returns a valid FD that can be used for read/write.
pub fn test_dev_tty_with_ctty_succeeds() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // Acquire controlling terminal via TIOCSCTTY.
    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0; // fd 0 (console)
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(task_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed");
    assert_eq_test!(
        unsafe { (*task_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "controlling_tty should be set after TIOCSCTTY"
    );

    // Now open /dev/tty — should succeed.
    let pid = unsafe { (*task_ptr).process_id };
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, b"/dev/tty", O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/tty) with ctty should succeed");

    // Clean up the FD.
    if fd >= 0 {
        let _ = file_close_fd(pid, fd);
    }
    task_terminate(task_id);
    TestResult::Pass
}

/// After `setsid()`, the controlling terminal is cleared, so opening
/// `/dev/tty` must return ENXIO.
pub fn test_setsid_then_dev_tty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    // Give parent a controlling terminal.
    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed");

    // Fork a child — it inherits the controlling terminal.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        Some(TtyIndex(0)),
        "child should inherit controlling_tty from parent"
    );

    // Child calls setsid() — controlling terminal cleared.
    let mut setsid_frame = zero_frame();
    let _ = syscall_setsid(child_ptr, &mut setsid_frame as *mut UserContext);
    assert_eq_test!(
        unsafe { (*child_ptr).controlling_tty },
        None,
        "setsid should clear controlling_tty"
    );

    // Now child tries to open /dev/tty — should fail with ENXIO.
    let child_pid = unsafe { (*child_ptr).process_id };
    make_task_current(child_ptr);
    let fd = file_open_for_process(child_pid, b"/dev/tty", O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        fd,
        -6,
        "open(/dev/tty) after setsid should return -6 (ENXIO)"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// A forked child inherits the parent's controlling terminal, so
/// `/dev/tty` resolves to the same TTY index as the parent.
pub fn test_fork_child_inherits_dev_tty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    // Parent acquires controlling terminal.
    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = syscall_ioctl(parent_ptr, &mut frame as *mut UserContext);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed for parent");

    // Fork child.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    let parent_ctty = unsafe { (*parent_ptr).controlling_tty };
    let child_ctty = unsafe { (*child_ptr).controlling_tty };
    assert_eq_test!(
        parent_ctty,
        child_ctty,
        "child should inherit same controlling_tty as parent"
    );

    // Child opens /dev/tty — should succeed (inherits parent's ctty).
    let child_pid = unsafe { (*child_ptr).process_id };
    make_task_current(child_ptr);
    let fd = file_open_for_process(child_pid, b"/dev/tty", O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "child open(/dev/tty) should succeed");

    if fd >= 0 {
        let _ = file_close_fd(child_pid, fd);
    }
    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// SYSCALL_VHANGUP is registered in the dispatch table.
pub fn test_vhangup_syscall_in_dispatch_table() -> TestResult {
    let entry = syscall_lookup(SYSCALL_VHANGUP);
    assert_not_null!(entry, "SYSCALL_VHANGUP lookup returned null");
    let entry_ref = unsafe { &*entry };
    assert_test!(
        entry_ref.handler.is_some(),
        "SYSCALL_VHANGUP has no handler"
    );
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_syscall_lookup_invalid_number,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_syscall_lookup_empty_slot, suite = syscall_valid);
slopos_testing::stest!(name = test_syscall_lookup_valid, suite = syscall_valid);
slopos_testing::stest!(
    name = test_process_syscall_lookup_valid,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_io_syscall_lookup_valid, suite = syscall_valid);
slopos_testing::stest!(
    name = test_net_scan_syscall_lookup_valid,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_fork_null_parent, suite = syscall_valid);
slopos_testing::stest!(name = test_fork_kernel_task, suite = syscall_valid);
slopos_testing::stest!(name = test_fork_terminated_parent, suite = syscall_valid);
slopos_testing::stest!(name = test_fork_blocked_parent, suite = syscall_valid);
slopos_testing::stest!(name = test_fork_cleanup_on_failure, suite = syscall_valid);
slopos_testing::stest!(name = test_user_ptr_null, suite = syscall_valid);
slopos_testing::stest!(name = test_user_ptr_kernel_address, suite = syscall_valid);
slopos_testing::stest!(name = test_user_ptr_misaligned, suite = syscall_valid);
slopos_testing::stest!(
    name = test_user_ptr_overflow_boundary,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_brk_extreme_values, suite = syscall_valid);
slopos_testing::stest!(name = test_memfd_create_boundaries, suite = syscall_valid);
slopos_testing::stest!(
    name = test_terminate_already_terminated,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_operations_on_terminated_task,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_fork_memory_pressure, suite = syscall_valid);
slopos_testing::stest!(name = test_task_id_wraparound, suite = syscall_valid);
slopos_testing::stest!(
    name = test_clone_thread_tls_isolation,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_clone_then_fork_interaction,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_futex_wait_mismatch_and_wake_no_waiters,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_futex_lost_wakeup_regression,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_futex_contention_path_stability,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_signal_install_deliver_and_sigreturn,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_sigprocmask_block_then_unblock_delivery,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_sigchld_and_wait_interaction,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_arch_prctl_set_get_fs_roundtrip,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_pipe_poll_eof_baseline, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_write_read_basic, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_eof_returns_zero, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_broken_pipe, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_multi_write_read, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_partial_read, suite = syscall_valid);
slopos_testing::stest!(name = test_pipe_buffer_full, suite = syscall_valid);
slopos_testing::stest!(
    name = test_exit_current_task_releases_pipe_refs,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_process_group_session_syscalls_baseline,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_kill_process_group_semantics,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_tiocsctty_session_leader_acquires_ctty,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_tiocsctty_non_leader_rejected,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_open_dev_tty_with_o_noctty_preserves_flag,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_setsid_child_preserves_parent_controlling_tty,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_hangup_clears_all_session_controlling_ttys,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_pts_open_acquires_controlling_tty_without_o_noctty,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_pts_open_with_o_noctty_skips_controlling_tty_acquire,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_vm_mmap_munmap_stress_baseline,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_spawn_path_stale_argv_regression,
    suite = syscall_valid
);
// /dev/tty Controlling Terminal Device
slopos_testing::stest!(
    name = test_dev_tty_no_ctty_returns_enxio,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_dev_tty_with_ctty_succeeds,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_setsid_then_dev_tty_returns_enxio,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_fork_child_inherits_dev_tty,
    suite = syscall_valid
);
// EXTPROC & vhangup
slopos_testing::stest!(
    name = test_vhangup_syscall_in_dispatch_table,
    suite = syscall_valid
);

slopos_testing::stest!(
    name = test_syscall_lookup_valid,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_process_syscall_lookup_valid,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_net_scan_syscall_lookup_valid,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_pipe_poll_eof_baseline,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_pipe_write_read_basic,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_pipe_eof_returns_zero,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(name = test_pipe_broken_pipe, suite = syscall_compat_smoke);
slopos_testing::stest!(
    name = test_pipe_multi_write_read,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(name = test_pipe_partial_read, suite = syscall_compat_smoke);
slopos_testing::stest!(name = test_pipe_buffer_full, suite = syscall_compat_smoke);
slopos_testing::stest!(
    name = test_exit_current_task_releases_pipe_refs,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_process_group_session_syscalls_baseline,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_kill_process_group_semantics,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_tiocsctty_session_leader_acquires_ctty,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_tiocsctty_non_leader_rejected,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_open_dev_tty_with_o_noctty_preserves_flag,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_setsid_child_preserves_parent_controlling_tty,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_hangup_clears_all_session_controlling_ttys,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_pts_open_acquires_controlling_tty_without_o_noctty,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_pts_open_with_o_noctty_skips_controlling_tty_acquire,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_sigchld_and_wait_interaction,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_clone_thread_tls_isolation,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_futex_wait_mismatch_and_wake_no_waiters,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_arch_prctl_set_get_fs_roundtrip,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_spawn_path_stale_argv_regression,
    suite = syscall_compat_smoke
);
// /dev/tty Controlling Terminal Device
slopos_testing::stest!(
    name = test_dev_tty_no_ctty_returns_enxio,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_dev_tty_with_ctty_succeeds,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_setsid_then_dev_tty_returns_enxio,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_fork_child_inherits_dev_tty,
    suite = syscall_compat_smoke
);
// EXTPROC & vhangup
slopos_testing::stest!(
    name = test_vhangup_syscall_in_dispatch_table,
    suite = syscall_compat_smoke
);
// AF_UNIX sockets
slopos_testing::stest!(
    name = test_unix_socket_send_recv_basic,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_unix_socket_poll_after_send,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_unix_socket_poll_before_send,
    suite = syscall_compat_smoke
);

// =============================================================================
// AF_UNIX Socket Tests
// =============================================================================

fn unix_create_connected_pair(pid: u32) -> Option<(i32, i32)> {
    use slopos_net::unix_socket;
    use slopos_net::unix_socket_file_ops::UNIX_SOCKET_FILE_OPS;

    let path = b"/test/sock";

    let srv_handle = unix_socket::unix_create()?;
    if unix_socket::unix_bind(srv_handle, path) != 0 {
        unix_socket::unix_close(srv_handle);
        return None;
    }
    if unix_socket::unix_listen(srv_handle, 4) != 0 {
        unix_socket::unix_close(srv_handle);
        return None;
    }
    unix_socket::unix_set_nonblocking(srv_handle, true);

    let cli_handle = unix_socket::unix_create()?;
    if unix_socket::unix_connect(cli_handle, path) != 0 {
        unix_socket::unix_close(cli_handle);
        unix_socket::unix_close(srv_handle);
        return None;
    }

    let accepted_handle = match unix_socket::unix_accept(srv_handle) {
        Ok(h) => h,
        Err(_) => {
            unix_socket::unix_close(cli_handle);
            unix_socket::unix_close(srv_handle);
            return None;
        }
    };

    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
    );
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
    );

    unix_socket::unix_close(srv_handle);

    if srv_fd < 0 || cli_fd < 0 {
        return None;
    }
    Some((srv_fd, cli_fd))
}

pub fn test_unix_socket_send_recv_basic() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let pid = unsafe { (*task_find_by_id(task_id)).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => return TestResult::Fail,
    };

    let payload = b"hello";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write wrong count");

    let mut out = [0u8; 16];
    let nread = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(nread as usize, payload.len(), "read wrong count");
    assert_test!(&out[..payload.len()] == payload, "payload mismatch");

    assert_eq_test!(file_close_fd(pid, srv_fd), 0);
    assert_eq_test!(file_close_fd(pid, cli_fd), 0);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_unix_socket_poll_after_send() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let pid = unsafe { (*task_find_by_id(task_id)).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => return TestResult::Fail,
    };

    let revents_before = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents_before & POLLIN) == 0, "readable before send");

    let payload = b"test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write wrong count");

    let revents_after = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents_after & POLLIN) != 0, "NOT readable after send");

    assert_eq_test!(file_close_fd(pid, srv_fd), 0);
    assert_eq_test!(file_close_fd(pid, cli_fd), 0);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_unix_socket_poll_before_send() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Make the user task PCR.current_task so file_poll_register_fd's
    // underlying enqueue_current() registers the FD-owning task. This
    // mirrors the real syscall_poll path, where the calling task polls
    // on its own FDs.
    make_task_current(task_ptr);

    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(
        reg.registered,
        "register must succeed when current_task owns the FD"
    );

    // Server sends — should wake registered waiter.
    let payload = b"wake";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write wrong count");

    // Data should be readable.
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!(
        (revents & POLLIN) != 0,
        "NOT readable after send with waiter"
    );

    slopos_fs::fileio::file_poll_unregister_fd(&reg);

    let mut out = [0u8; 16];
    let nread = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(nread as usize, payload.len(), "read wrong count");

    assert_eq_test!(file_close_fd(pid, srv_fd), 0);
    assert_eq_test!(file_close_fd(pid, cli_fd), 0);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

// =============================================================================
// Poll/Wakeup Race Condition Tests
// =============================================================================
// WillBlock State Machine Tests
// =============================================================================

/// sleep_current_task_ms uses CAS(Running, Blocked). From WillBlock state
/// (prepare_to_wait was called but no wakeup yet), this CAS must fail.
pub fn test_sleep_ms_cas_overwrites_wakeup() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // Set up: WillBlock (simulating prepare_to_wait without wakeup yet)
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);

    // CAS(Running, Blocked) must fail because state is WillBlock, not Running.
    let result = task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::Sleep,
    );

    let state = unsafe { (*task_ptr).status() };
    assert_test!(
        state != TaskStatus::Blocked,
        "sleep CAS should not block from WillBlock state"
    );
    assert_test!(
        result != 0,
        "CAS(Running, Blocked) should fail from WillBlock"
    );

    if state == TaskStatus::Blocked {
        let _ = task_set_state(task_id, TaskStatus::Running);
    }
    task_terminate(task_id);
    TestResult::Pass
}

/// After WillBlock->Running (wakeup), CAS(WillBlock, Blocked) must fail.
///
/// Simulates the interleaving where a wakeup arrives between
/// prepare_to_wait and the blocking CAS:
///   1. WillBlock (prepare_to_wait)
///   2. Running (wakeup)
///   3. CAS(WillBlock, Blocked) must fail (state is Running)
pub fn test_block_current_task_toctou_allows_reblock() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // prepare_to_wait
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);

    // Wakeup arrives
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    // CAS(WillBlock, Blocked) must fail because state is Running.
    let result = task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Blocked);

    let state = unsafe { (*task_ptr).status() };
    assert_test!(
        state != TaskStatus::Blocked,
        "CAS(WillBlock, Blocked) must not succeed when state is Running"
    );
    assert_eq_test!(
        state,
        TaskStatus::Running,
        "task should still be Running after failed block CAS"
    );
    assert_test!(
        result != 0,
        "CAS(WillBlock, Blocked) should fail from Running"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// WillBlock must be set BEFORE enqueueing on a wait queue.
///
/// Verifies that unblock_task sees WillBlock and transitions to Running
/// when the correct ordering (prepare_to_wait -> enqueue -> wakeup) is used.
pub fn test_wq_wrong_order_wakeup_lost() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // Set task to Running (starting state for pipe read path)
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    // prepare_to_wait -> WillBlock
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);

    // Wakeup arrives — unblock_task sees WillBlock -> Running.
    let result = unblock_task(task_ptr);
    assert_eq_test!(result, 0, "unblock_task should succeed from WillBlock");

    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(
        state,
        TaskStatus::Running,
        "wakeup must be preserved: state should be Running"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// WillBlock set before wakeup preserves the wakeup signal.
/// Positive counterpart to test_wq_wrong_order_wakeup_lost.
pub fn test_wq_correct_order_wakeup_preserved() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // CORRECT ORDER: set WillBlock first, then wakeup arrives.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);

    // Wakeup: unblock_task sees WillBlock -> transitions to Running.
    let result = unblock_task(task_ptr);
    assert_eq_test!(result, 0, "unblock_task should succeed");

    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(
        state,
        TaskStatus::Running,
        "wakeup should be preserved with correct ordering"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// try_transition_from(WillBlock, Blocked) rejects a task in Running state
/// and succeeds from WillBlock state.
pub fn test_try_transition_from_rejects_wrong_state() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // Set up: WillBlock -> Running (wakeup)
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    // CAS(WillBlock, Blocked) must fail when state is Running
    let result = task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Blocked);
    assert_test!(
        result != 0,
        "try_transition_from(WillBlock, Blocked) should fail when state is Running"
    );

    // The task should still be Running
    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(state, TaskStatus::Running, "state should still be Running");

    // Verify the CAS succeeds from the correct state
    assert_eq_test!(task_set_state(task_id, TaskStatus::WillBlock), 0);
    let result2 = task_try_transition_from(task_id, TaskStatus::WillBlock, TaskStatus::Blocked);
    assert_test!(
        result2 == 0,
        "try_transition_from(WillBlock, Blocked) should succeed when state IS WillBlock"
    );
    let state2 = unsafe { (*task_ptr).status() };
    assert_eq_test!(state2, TaskStatus::Blocked, "state should be Blocked");

    task_terminate(task_id);
    TestResult::Pass
}

/// E2E: call syscall_poll on a unix socket, write data, verify poll returns.
///
/// This exercises the full kernel poll path: prepare_to_wait, WQ registration,
/// readiness check, block_current_task_with_timeout, wakeup via unix_send.
/// Runs in two variants:
///   1. Data already buffered before poll — poll must return immediately.
///   2. No data, short timeout — poll must return 0 (timeout) within margin.
pub fn test_unix_socket_poll_syscall_e2e() -> TestResult {
    use crate::syscall::fs::syscall_poll;
    use slopos_abi::syscall::UserPollFd;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task ptr");
    let pid = unsafe { (*task_ptr).process_id };

    // Need a user-space page for the pollfd struct.
    let upage = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Skipped;
        }
    };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // ------------------------------------------------------------------
    // Variant 1: write data FIRST, then poll — must return immediately.
    // ------------------------------------------------------------------
    let payload = b"e2e-test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write failed");

    let pfd = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd), "copy pollfd to user");

    // Call syscall_poll: frame.regs_mut().rdi = pollfd_ptr, rsi = nfds, rdx = timeout_ms
    let mut frame = zero_frame();
    frame.regs_mut().rdi = upage; // pollfd array pointer
    frame.regs_mut().rsi = 1; // nfds = 1
    frame.regs_mut().rdx = 5000; // timeout_ms = 5000 (should not matter, data ready)

    let start = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        syscall_poll(task_ptr, &mut frame as *mut UserContext)
    });
    let elapsed = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start);

    // poll should return 1 (one fd ready)
    assert_eq_test!(frame.regs().rax, 1, "poll should report 1 ready fd");
    assert_test!(
        elapsed < 200,
        "poll with buffered data should return quickly"
    );

    // Verify revents
    if let Some(result_pfd) = user_copy_in::<UserPollFd>(pid, upage) {
        assert_test!(
            (result_pfd.revents & POLLIN) != 0,
            "poll should set POLLIN on readable socket"
        );
    }

    // Drain the data
    let mut out = [0u8; 16];
    let _ = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );

    // ------------------------------------------------------------------
    // Variant 2: no data, timeout=100ms — poll must return 0 (timeout).
    // ------------------------------------------------------------------
    let pfd_empty = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd_empty), "copy empty pollfd");

    let mut frame2 = zero_frame();
    frame2.regs_mut().rdi = upage;
    frame2.regs_mut().rsi = 1;
    frame2.regs_mut().rdx = 100; // 100ms timeout

    let start2 = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        syscall_poll(task_ptr, &mut frame2 as *mut UserContext)
    });
    let elapsed2 = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start2);

    // poll should return 0 (timeout, no data)
    assert_eq_test!(frame2.regs().rax, 0, "poll with no data should timeout");
    // Should take roughly 100ms (allow 50-500ms margin for timer granularity)
    assert_test!(
        elapsed2 >= 50 && elapsed2 <= 500,
        "poll timeout duration out of range"
    );

    // ------------------------------------------------------------------
    // Variant 3: write data while poll is "in progress" (simulated).
    // We write data, then immediately call poll with a long timeout.
    // If data is already there, poll returns instantly. This simulates
    // the compositor handshake pattern.
    // ------------------------------------------------------------------
    let payload2 = b"OutputInfo-sim";
    let written2 = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload2));
    assert_eq_test!(written2 as usize, payload2.len(), "write2 failed");

    let pfd3 = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd3), "copy pollfd3");

    let mut frame3 = zero_frame();
    frame3.regs_mut().rdi = upage;
    frame3.regs_mut().rsi = 1;
    frame3.regs_mut().rdx = 10_000; // 10s timeout (mimics compositor wait_recv)

    let start3 = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        syscall_poll(task_ptr, &mut frame3 as *mut UserContext)
    });
    let elapsed3 = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start3);

    assert_eq_test!(frame3.regs().rax, 1, "poll should find data immediately");
    assert_test!(
        elapsed3 < 200,
        "poll with pre-buffered data must not sleep (compositor handshake pattern)"
    );

    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// Compositor handshake: listen → connect(backlog) → accept → send → poll.
///
/// Exercises the EXACT code path of the compositor-shell handshake:
/// 1. Server binds and listens (compositor socket activation)
/// 2. Client connects (goes to backlog, no accept yet)
/// 3. Server accepts (gets side-B FD)
/// 4. Server sends OutputInfo-sized payload through side-B
/// 5. Client polls for POLLIN on its side-A socket
///
/// This catches bugs where the accept/send path fails to make data
/// visible to the client's poll/readiness check.
pub fn test_compositor_handshake_listen_accept_send_poll() -> TestResult {
    use slopos_net::unix_socket;
    use slopos_net::unix_socket_file_ops::UNIX_SOCKET_FILE_OPS;

    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let pid = unsafe { (*task_find_by_id(task_id)).process_id };

    let path = b"/test/compositor-handshake";

    // ── Server: bind + listen (like compositor socket activation) ──
    let listen_handle = unix_socket::unix_create().expect("listen socket create");
    assert_eq_test!(unix_socket::unix_bind(listen_handle, path), 0, "bind");
    assert_eq_test!(unix_socket::unix_listen(listen_handle, 4), 0, "listen");
    unix_socket::unix_set_nonblocking(listen_handle, true);

    // ── Client: connect (goes to backlog, no accept yet) ──
    let cli_handle = unix_socket::unix_create().expect("client socket create");
    let rc = unix_socket::unix_connect(cli_handle, path);
    assert_eq_test!(rc, 0, "connect");

    // Open FD for client side (like kernel does after connect syscall)
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
    );
    assert_test!(cli_fd >= 0, "cli fd open");

    // ── Verify: client socket has no data yet ──
    let revents0 = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!(
        (revents0 & POLLIN) == 0,
        "client should have no data before server accept+send"
    );

    // ── Server: accept (gets side-B) ──
    let accepted_handle = unix_socket::unix_accept(listen_handle).expect("accept");

    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
    );
    assert_test!(srv_fd >= 0, "srv fd open");

    // ── Server: send OutputInfo-sized payload (like compositor does) ──
    // OutputInfo is 4 x u32 = 16 bytes. With 4-byte length prefix = 20 bytes.
    let payload = b"OutputInfo-simul";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "server write failed");

    // ── Client: poll for POLLIN (like shell's wait_recv) ──
    let revents1 = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!(
        (revents1 & POLLIN) != 0,
        "client poll must see POLLIN after server send"
    );

    // ── Client: read the data ──
    let mut out = [0u8; 32];
    let nread = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(nread as usize, payload.len(), "read count");
    assert_test!(&out[..payload.len()] == payload, "payload mismatch");

    // Cleanup
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    unix_socket::unix_close(listen_handle);
    task_terminate(task_id);
    TestResult::Pass
}

/// WillBlock wakeup through unix socket write.
///
/// Exercises the exact wakeup path that syscall_poll relies on:
/// 1. prepare_to_wait → current task = WillBlock
/// 2. Register current task on client socket's RECV_WQS
/// 3. Server writes data → unix_send → RECV_WQS[peer].wake_all() → unblock_task
/// 4. unblock_task CAS(WillBlock, Running) → current task = Running
///
/// `prepare_to_wait` and `enqueue_current` operate on PCR.current_task.
/// The test makes the FD-owning user task current via `make_task_current`
/// so the polling identity matches the FD owner — exactly the invariant
/// the real syscall_poll path relies on.
pub fn test_unix_send_wakes_willblock_poll_waiter() -> TestResult {
    use crate::scheduler::scheduler::prepare_to_wait;

    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Make the FD-owning user task current. dispatch() sets it to Running,
    // which is the precondition for prepare_to_wait's Running → WillBlock.
    make_task_current(task_ptr);

    // Phase 1: prepare_to_wait → WillBlock
    prepare_to_wait();
    assert_eq_test!(
        unsafe { (*task_ptr).status() },
        TaskStatus::WillBlock,
        "STEP1: WillBlock"
    );

    // Phase 2: register + check readiness
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "STEP2: register");
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "STEP2: no data before write");

    // Phase 3: write data → RECV_WQS[peer].wake_all() → unblock_task CAS
    let payload = b"wake-test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "STEP3: write");

    // Phase 4: verify CAS(WillBlock, Running) ran via the wait queue
    let state_after = unsafe { (*task_ptr).status() };
    assert_eq_test!(state_after, TaskStatus::Running, "STEP4: must be Running");

    let revents_after = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents_after & POLLIN) != 0, "STEP4: POLLIN");

    // Cleanup. finish_wait is a no-op when state is already Running, but
    // we keep it for symmetry with the prepare_to_wait above and so the
    // invariant survives if a future change leaves the task in WillBlock.
    slopos_kernel_services::driver_runtime::finish_wait();
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

/// Verifies that Ready → WillBlock is a valid transition.
///
/// When a task is preempted by the scheduler (Running → Ready) and then
/// resumes in the poll loop, prepare_to_wait must be able to set WillBlock
/// from Ready. Without this transition, prepare_to_wait silently fails
/// and the poll handler busy-loops until timeout.
pub fn test_ready_to_willblock_transition() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task ptr");

    // Set task to Ready (simulating scheduler preemption).
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);

    // Ready → WillBlock must succeed.
    assert_eq_test!(
        task_set_state(task_id, TaskStatus::WillBlock),
        0,
        "Ready -> WillBlock must be a valid transition"
    );
    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(state, TaskStatus::WillBlock, "should be WillBlock");

    // Also verify WillBlock → Running (wakeup) still works from this path.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    task_terminate(task_id);
    TestResult::Pass
}

/// Demonstrates the check-first-register-second race (the OLD broken pattern).
///
/// Manually simulates the sequence that unix_poll_fused used to execute:
/// 1. Check readiness (empty) — under UNIX_STATE lock
/// 2. Data arrives + wake_all fires — nobody on the queue yet
/// 3. Register on the queue — too late, wakeup already fired
///
/// The task should still be WillBlock because unblock_task was never
/// called (wake_all found nobody to dequeue).  This proves the race
/// existed by construction.
pub fn test_poll_fused_gap_demonstrates_race() -> TestResult {
    use crate::scheduler::scheduler::prepare_to_wait;

    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Make the FD-owning user task current — needed for prepare_to_wait
    // and for enqueue_current to operate on a real, observable task.
    make_task_current(task_ptr);

    // Step 1: prepare_to_wait → WillBlock
    prepare_to_wait();
    assert_eq_test!(
        unsafe { (*task_ptr).status() },
        TaskStatus::WillBlock,
        "WillBlock"
    );

    // Step 2: Check readiness WITHOUT registering (old broken order)
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "no data yet");

    // Step 3: Data arrives — wake_all fires with nobody on the queue!
    let payload = b"race-demo";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    // Step 4: NOW register (too late — wakeup already fired)
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);

    // The task must STILL be WillBlock because wake_all found nobody on
    // the queue and never called unblock_task. This is the lost-wakeup
    // signature that the register-first ordering exists to prevent.
    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(
        state,
        TaskStatus::WillBlock,
        "wakeup lost — still WillBlock"
    );

    // Cleanup. finish_wait transitions WillBlock → Running so the task
    // is in a clean state before terminate.
    slopos_kernel_services::driver_runtime::finish_wait();
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

/// Proves the register-first-then-check order preserves wakeups.
///
/// Simulates the FIXED pattern (Linux sock_poll_wait order):
/// 1. Register on the queue FIRST
/// 2. Data arrives + wake_all fires — finds the task on the queue
/// 3. unblock_task CAS(WillBlock → Running) — wakeup preserved
/// 4. Check readiness — sees data
///
/// This is the exact invariant that the fixed poll_fused implements.
pub fn test_poll_fused_register_first_catches_wakeup() -> TestResult {
    use crate::scheduler::scheduler::prepare_to_wait;

    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = unsafe { (*task_ptr).process_id };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Make the FD-owning user task current. Pairs with the gap-test
    // counterpart so both tests measure the same observable: the state
    // of the same task, as a function of register/check ordering.
    make_task_current(task_ptr);

    // Step 1: prepare_to_wait → WillBlock
    prepare_to_wait();
    assert_eq_test!(
        unsafe { (*task_ptr).status() },
        TaskStatus::WillBlock,
        "WillBlock"
    );

    // Step 2: Register FIRST (the Linux pattern)
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "register");

    // Step 3: Data arrives — wake_all finds us on the queue!
    let payload = b"race-fix";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    // Step 4: Wakeup preserved — unblock_task CAS(WillBlock → Running)
    let state = unsafe { (*task_ptr).status() };
    assert_eq_test!(state, TaskStatus::Running, "wakeup preserved — Running");

    // Step 5: Readiness check sees the data
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) != 0, "POLLIN");

    // Cleanup
    slopos_kernel_services::driver_runtime::finish_wait();
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_sleep_ms_cas_overwrites_wakeup,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_block_current_task_toctou_allows_reblock,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_wq_wrong_order_wakeup_lost,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_wq_correct_order_wakeup_preserved,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_try_transition_from_rejects_wrong_state,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_unix_socket_poll_syscall_e2e,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_compositor_handshake_listen_accept_send_poll,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_unix_send_wakes_willblock_poll_waiter,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_ready_to_willblock_transition,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_poll_fused_gap_demonstrates_race,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_poll_fused_register_first_catches_wakeup,
    suite = poll_wakeup_race
);
