//! Syscall Validation Tests
//!
//! Targets: invalid/null pointer handling, boundary conditions,
//! permission checks, resource exhaustion, and dispatch edge cases.

use core::ffi::c_char;
use core::ptr;

use crate::syscall::fs::syscall_ioctl;
use crate::syscall::handlers::{
    syscall_arch_prctl, syscall_futex, syscall_getpgid, syscall_setpgid, syscall_setsid,
};
use crate::syscall::signal::{
    deliver_pending_signal, deliver_pending_signal_on_irq_exit, syscall_kill, syscall_rt_sigaction,
    syscall_rt_sigprocmask, syscall_rt_sigreturn,
};
use slopos_abi::addr::PhysAddr;
use slopos_abi::fs::O_RDONLY;
use slopos_abi::signal::{
    SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGCONT, SIGHUP, SIGINT, SIGSTOP, SIGTERM, SIGTSTP,
    SIGTTIN, SIGTTOU, SIGUSR1, SIGWINCH, SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit,
    sig_default_action, sig_default_ignores,
};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, CLONE_SETTLS, CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, ERRNO_EAGAIN,
    F_GETFL, F_SETFD, FD_CLOEXEC, FUTEX_WAIT, FUTEX_WAKE, MAP_ANONYMOUS, MAP_PRIVATE, O_NOCTTY,
    O_NONBLOCK, POLLIN, SYSCALL_ARCH_PRCTL, SYSCALL_CLONE, SYSCALL_FUTEX, SYSCALL_GETPGID,
    SYSCALL_IOCTL, SYSCALL_KILL, SYSCALL_NET_SCAN, SYSCALL_PIPE, SYSCALL_PIPE2, SYSCALL_POLL,
    SYSCALL_RT_SIGACTION, SYSCALL_RT_SIGPROCMASK, SYSCALL_RT_SIGRETURN, SYSCALL_SELECT,
    SYSCALL_SETPGID, SYSCALL_SETSID, SYSCALL_TABLE_SIZE, SYSCALL_VHANGUP, TIOCSCTTY, TtyIndex,
};
use slopos_abi::task::{INVALID_TASK_ID, TASK_FLAG_KERNEL_MODE, TASK_FLAG_USER_MODE, TaskStatus};
use slopos_mm::page_alloc::{alloc_kernel_page, free_page_frame};
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::task::SchedPlacement;
use slopos_ostd::task::{new_group_in_session, new_session_group};
use slopos_ostd::user::context::UserContext;
use slopos_ostd::{KArc, KBox, klog_info};
use slopos_sched::task_struct::{SignalAction, Task};
use slopos_testing::{TestResult, assert_eq_test, assert_not_null, assert_test, fail, pass};

use crate::exec::{FdAction, apply_fd_actions};
use crate::syscall::handlers::syscall_lookup;
use slopos_abi::io::{KernelIoBuf, KernelIoBufRef};
use slopos_abi::task::BlockReason;
use slopos_fs::fileio::{
    file_close_fd, file_dup_fd, file_dup3_fd, file_fcntl_fd, file_open_for_process,
    file_open_tty_fd, file_pipe_create, file_poll_fd, file_read_fd, file_seek_fd, file_write_fd,
    fileio_clone_table_for_process, fileio_create_empty_table_for_process,
    fileio_destroy_table_for_process,
};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_sched::scheduler::unblock_task;
use slopos_sched::task;
use slopos_sched::task::{
    task_clone, task_controlling_tty, task_create, task_find_by_id, task_fork, task_fs_base,
    task_pgid, task_process_group, task_process_id, task_sched_placement_store, task_set_state,
    task_set_state_from_with_reason, task_sid, task_signal_pending, task_status, task_terminate,
    task_try_transition_from,
};

// =============================================================================
// Test Helpers
// =============================================================================

/// Wrapper around the hermetic `KernelTestScope` so existing call sites
/// (`let _f = SyscallFixture::new();`) keep working without churn.
/// The previous hand-rolled fixture leaked PCR pointers and per-CPU
/// `enabled` bits; the hermetic scope's registry walk handles every
/// such leak through the per-subsystem `HermeticState` impls in
/// `slopos_sched::test_hermetic`.
type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

/// Park PCR's `current_task` on the BSP bootstrap stub. Used by tests
/// that mutate the running-task pointer. The hermetic
/// `BspCurrentTask` impl restores the original value on scope drop.
fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::set_current_task(
        slopos_sched::safestack_rt::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

fn make_task_current(task_ptr: *mut Task) {
    assert!(!task_ptr.is_null(), "make_task_current: null task_ptr");
    if task_status(task_ptr) == Some(TaskStatus::Blocked) {
        let task_id = slopos_sched::task::task_id_of(task_ptr).unwrap_or(INVALID_TASK_ID);
        assert_eq!(task_set_state(task_id, TaskStatus::Ready), 0);
    }
    task_sched_placement_store(task_ptr, SchedPlacement::OnCpu);
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    slopos_sched::scheduler::dispatch_for_test(cpu_id, task_ptr);
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
    let user_entry = slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);
    let id = task_create(
        b"UserTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        TASK_FLAG_USER_MODE,
    );
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
    if !slopos_mm::process_vm::process_vm_activate(pid) {
        return None;
    }
    set_test_process_id(pid);
    let out = f();
    set_test_process_id(slopos_abi::task::INVALID_PROCESS_ID);
    // Reset to kernel master — the test scope runs against the kernel
    // master VmSpace once the per-process scope returns.
    slopos_kernel_services::kernel_vm_space::kernel_vm_space()
        .lock()
        .activate_kernel_master();
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

    let phys: PhysAddr = alloc_kernel_page();
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
        // Map failed — release the physical page we allocated above
        // so this helper doesn't leak frames on the error path.
        free_page_frame(phys);
        return None;
    }

    Some(base)
}

// =============================================================================
// Syscall Dispatch Tests
// =============================================================================

pub fn test_syscall_lookup_invalid_number() -> TestResult {
    assert_test!(
        syscall_lookup(0xFFFF).is_none(),
        "should reject out-of-bounds"
    );
    assert_test!(
        syscall_lookup(SYSCALL_TABLE_SIZE as u64).is_none(),
        "should reject boundary"
    );
    assert_test!(syscall_lookup(u64::MAX).is_none(), "should reject u64::MAX");
    TestResult::Pass
}

pub fn test_syscall_lookup_empty_slot() -> TestResult {
    let entry = syscall_lookup(9);
    assert_test!(entry.is_none(), "unimplemented slot should return null");
    TestResult::Pass
}

pub fn test_index_tty_io_syscalls_retired() -> TestResult {
    // 146/147/148 were index-addressed TTY read/write/open. They are
    // retired to unregistered slots (dispatch returns ENOSYS); TTY access
    // is fd-only now. Guards against anyone re-registering the numbers.
    assert_test!(
        syscall_lookup(146).is_none(),
        "146 (tty_read) must be retired"
    );
    assert_test!(
        syscall_lookup(147).is_none(),
        "147 (tty_write) must be retired"
    );
    assert_test!(
        syscall_lookup(148).is_none(),
        "148 (open_tty_fd) must be retired"
    );
    TestResult::Pass
}

pub fn test_syscall_lookup_valid() -> TestResult {
    // SYSCALL_EXIT = 1
    let entry = syscall_lookup(1);
    let Some(entry_ref) = entry else {
        klog_info!("SYSCALL_EXIT lookup returned None");
        return TestResult::Fail;
    };
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
        let Some(entry) = syscall_lookup(sysno) else {
            klog_info!("required syscall {} missing from table", sysno);
            return TestResult::Fail;
        };
        assert_test!(entry.handler.is_some(), "required syscall has no handler");
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
        let Some(entry) = syscall_lookup(sysno) else {
            klog_info!("required syscall {} missing from dispatch table", sysno);
            return TestResult::Fail;
        };
        assert_test!(
            entry.handler.is_some(),
            "required syscall has no handler in dispatch table"
        );
    }

    TestResult::Pass
}

pub fn test_net_scan_syscall_lookup_valid() -> TestResult {
    let Some(entry) = syscall_lookup(SYSCALL_NET_SCAN) else {
        klog_info!("net_scan syscall missing from table");
        return TestResult::Fail;
    };
    assert_test!(entry.handler.is_some(), "net_scan syscall has no handler");
    TestResult::Pass
}

pub fn test_pipe_poll_eof_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_getpgid, parent_ptr, &mut frame);
    assert_eq_test!(
        frame.regs().rax as u32,
        task_pgid(parent_ptr).unwrap_or(0),
        "getpgid self mismatch"
    );

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = child_id as u64;
    setpgid_frame.regs_mut().rsi = parent_id as u64;
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setpgid, parent_ptr, &mut setpgid_frame);
    assert_eq_test!(
        setpgid_frame.regs().rax,
        0,
        "setpgid should succeed for child"
    );
    assert_eq_test!(
        task_pgid(child_ptr).unwrap_or(0),
        parent_id,
        "child pgid mismatch after setpgid"
    );

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, child_ptr, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.regs().rax as u32,
        child_id,
        "setsid should return child sid"
    );
    assert_eq_test!(
        task_sid(child_ptr).unwrap_or(0),
        child_id,
        "child sid mismatch after setsid"
    );
    assert_eq_test!(
        task_pgid(child_ptr).unwrap_or(0),
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
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setpgid, leader_ptr, &mut setpgid_frame);
    assert_eq_test!(
        setpgid_frame.regs().rax,
        0,
        "setpgid should succeed for member"
    );

    let leader_pid = task_process_id(leader_ptr).unwrap_or(0);
    let member_pid = task_process_id(member_ptr).unwrap_or(0);

    let mut probe_frame = zero_frame();
    probe_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    probe_frame.regs_mut().rsi = 0;
    let _ = with_user_process_context(leader_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, leader_ptr, &mut probe_frame)
    });
    assert_eq_test!(
        probe_frame.regs().rax,
        0,
        "kill(group, 0) probe should succeed"
    );

    slopos_sched::task::task_signal_pending_store(leader_ptr, 0);
    slopos_sched::task::task_signal_pending_store(member_ptr, 0);

    let mut negative_group_frame = zero_frame();
    negative_group_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    negative_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(leader_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_kill,
            leader_ptr,
            &mut negative_group_frame,
        )
    });
    assert_eq_test!(
        negative_group_frame.regs().rax,
        0,
        "kill(-pgid, SIGUSR1) failed"
    );

    let pending_bit = sig_bit(SIGUSR1);
    let leader_pending = task_signal_pending(leader_ptr);
    let member_pending = task_signal_pending(member_ptr);
    assert_test!(
        (leader_pending & pending_bit) != 0,
        "leader did not receive group signal"
    );
    assert_test!(
        (member_pending & pending_bit) != 0,
        "member did not receive group signal"
    );

    slopos_sched::task::task_signal_pending_store(leader_ptr, 0);
    slopos_sched::task::task_signal_pending_store(member_ptr, 0);

    let mut caller_group_frame = zero_frame();
    caller_group_frame.regs_mut().rdi = 0;
    caller_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(member_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_kill,
            member_ptr,
            &mut caller_group_frame,
        )
    });
    assert_eq_test!(caller_group_frame.regs().rax, 0, "kill(0, SIGUSR1) failed");

    let leader_pending_after = task_signal_pending(leader_ptr);
    let member_pending_after = task_signal_pending(member_ptr);
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

/// A user task is born its own process-group leader with a live group object;
/// fork shares that object by identity; setsid installs a fresh one.
pub fn test_process_group_object_fork_and_setsid_identity() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID);
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr);

    let parent_pg = task_process_group(parent_ptr).expect("parent carries a process group");
    assert_eq_test!(parent_pg.id(), parent_id, "leader group id == leader pid");
    assert_eq_test!(
        parent_pg.session_id(),
        parent_id,
        "leader session id == leader pid"
    );

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr);

    let child_pg = task_process_group(child_ptr).expect("child inherits a process group");
    assert_test!(
        KArc::ptr_eq(&parent_pg, &child_pg),
        "fork shares the parent's group object by identity"
    );

    // setsid on the child swaps to a brand-new session + group.
    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, child_ptr, &mut setsid_frame);
    let child_pg2 = task_process_group(child_ptr).expect("child group after setsid");
    assert_test!(
        !KArc::ptr_eq(&parent_pg, &child_pg2),
        "setsid installs a new group object"
    );
    assert_eq_test!(child_pg2.id(), child_id, "new group id == child pid");
    assert_eq_test!(
        child_pg2.session_id(),
        child_id,
        "new session id == child pid"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// The strong graph `ProcessGroup -> Session` is a DAG kept alive by member
/// tasks: a session outlives every group that belongs to it and is freed only
/// when the last group drops. A weak handle proves the death.
pub fn test_process_group_session_dag_lifetime() -> TestResult {
    let _fixture = SyscallFixture::new();

    let pg = new_session_group(42).expect("mint session+group");
    assert_eq_test!(pg.id(), 42, "group id");
    assert_eq_test!(pg.session_id(), 42, "session id == leader pid");

    let session_weak = KArc::downgrade(pg.session());

    // A second group inside the same session pins that session.
    let pg2 = new_group_in_session(43, pg.session().clone()).expect("mint second group");
    assert_eq_test!(pg2.session_id(), 42, "second group shares the session");

    // Dropping the first group leaves the session alive via the second.
    drop(pg);
    assert_test!(
        session_weak.upgrade().is_some(),
        "session alive while any group lives"
    );

    // Dropping the last group frees the session — the weak goes dead.
    drop(pg2);
    assert_test!(
        session_weak.upgrade().is_none(),
        "session freed when its last group drops"
    );
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, task_ptr, &mut frame);
    assert_eq_test!(
        frame.regs().rax,
        0,
        "TIOCSCTTY should succeed for session leader"
    );

    let sid = task_sid(task_ptr).unwrap_or(0);
    let ctty = task_controlling_tty(task_ptr);
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, child_ptr, &mut frame);
    assert_test!(
        frame.regs().rax != 0,
        "TIOCSCTTY should fail for non-session leader"
    );

    assert_eq_test!(
        task_controlling_tty(child_ptr),
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, task_ptr, &mut frame);
    assert_eq_test!(
        frame.regs().rax,
        0,
        "TIOCSCTTY should succeed before /dev/tty open"
    );

    let pid = task_process_id(task_ptr).unwrap_or(0);
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, parent_ptr, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.regs().rax, 0, "parent TIOCSCTTY should succeed");

    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, child_ptr, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.regs().rax as u32,
        child_id,
        "setsid should succeed for child"
    );
    assert_eq_test!(
        task_controlling_tty(child_ptr),
        None,
        "child should drop inherited ctty"
    );
    assert_eq_test!(
        task_controlling_tty(parent_ptr),
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, leader_ptr, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.regs().rax, 0, "leader TIOCSCTTY should succeed");

    let child_id = task_fork(leader_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    slopos_kernel_services::syscall_services::tty::hangup(TtyIndex(0));

    assert_eq_test!(
        task_controlling_tty(leader_ptr),
        None,
        "leader ctty should clear on hangup"
    );
    assert_eq_test!(
        task_controlling_tty(child_ptr),
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

    // The returned backing IS the master open — held for the test so the
    // pair stays alive, dropped at the end to tear it down.
    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty()
    {
        Ok(v) => v,
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

    let pid = task_process_id(task_ptr).unwrap_or(0);
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N) failed");
    assert_eq_test!(
        task_controlling_tty(task_ptr),
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
    drop(master_open);
    TestResult::Pass
}

pub fn test_pts_open_with_o_noctty_skips_controlling_tty_acquire() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty()
    {
        Ok(v) => v,
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

    let pid = task_process_id(task_ptr).unwrap_or(0);
    make_task_current(task_ptr);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N, O_NOCTTY) failed");
    assert_eq_test!(
        task_controlling_tty(task_ptr),
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
    drop(master_open);
    TestResult::Pass
}

pub fn test_tty_poll_after_close_reuse_no_crossobject() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // The returned backing owns the pair for the test's duration.
    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty()
    {
        Ok(v) => v,
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
            drop(master_open);
            return TestResult::Fail;
        }
    };
    let _ = slopos_kernel_services::syscall_services::tty::set_pty_lock(master_idx, false);

    let pid = task_process_id(task_ptr).unwrap_or(0);
    // file_poll_register_fd registers the current task as the waiter, so the
    // FD-owning task must be PCR.current_task (mirrors the real poll path).
    make_task_current(task_ptr);

    let slave_fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    assert_test!(slave_fd >= 0, "open(/dev/pts/N) failed");

    // The registration carries only a KWeak<OpenFile> — never a strong ref —
    // so it resolves to the live open file while the fd is open.
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, slave_fd, POLLIN);
    assert_test!(!reg.is_stale(), "fresh registration must not be stale");

    // Close the slave fd: drops the last OpenFile alias.
    assert_eq_test!(file_close_fd(pid, slave_fd), 0, "close slave fd failed");
    assert_test!(
        reg.is_stale(),
        "registration must go stale once its fd closes"
    );

    // Reopen — reuses the freed fd number for a fresh, distinct open file.
    let reused_fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    assert_test!(reused_fd >= 0, "reopen(/dev/pts/N) failed");
    assert_eq_test!(reused_fd, slave_fd, "expected fd-number reuse");

    // Same fd number, but the old registration stays dead while a fresh
    // registration on the reused fd resolves live: the two can never be the
    // same object, so the stale registration cannot adopt the reused fd.
    let reg_reused = slopos_fs::fileio::file_poll_register_fd(pid, reused_fd, POLLIN);
    assert_test!(
        !reg_reused.is_stale(),
        "reused fd must resolve to a live object"
    );
    assert_test!(
        reg.is_stale(),
        "stale registration must not adopt the reused fd"
    );

    // Unregistering the stale registration is a safe no-op.
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    slopos_fs::fileio::file_poll_unregister_fd(&reg_reused);

    park_bootstrap_on_current_cpu();
    let _ = file_close_fd(pid, reused_fd);
    task_terminate(task_id);
    drop(master_open);
    TestResult::Pass
}

pub fn test_vm_mmap_munmap_stress_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

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

    use slopos_sched::task::task_fork;
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

    use slopos_sched::task::task_fork;
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

    use slopos_sched::task::task_fork;

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    task_terminate(task_id);

    let task_ptr_after = task_find_by_id(task_id);
    if !task_ptr_after.is_null() {
        let state = task_status(task_ptr_after).unwrap_or(TaskStatus::Terminated);
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

    use slopos_sched::task::{task_fork, task_set_state};

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
    if let Some((handle, _ops, backing)) = result {
        // ftruncate with zero size should fail
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 0);
        assert_test!(rc < 0, "ftruncate(0) should fail");

        // ftruncate with valid size should succeed
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 4096);
        assert_eq_test!(rc, 0, "ftruncate(4096) should succeed");

        // ftruncate again should fail (one-shot)
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 8192);
        assert_test!(rc < 0, "ftruncate twice should fail");

        // Dropping the backing runs the memfd teardown (the old close path).
        drop(backing);
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
        let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
        assert_test!(state != TaskStatus::Ready, "terminated task in READY state");
    }

    TestResult::Pass
}

pub fn test_operations_on_terminated_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);

    use slopos_sched::task::task_get_info;
    let mut task_ptr: *mut Task = ptr::null_mut();
    let _info_result = task_get_info(task_id, &mut task_ptr);

    use slopos_sched::task::task_set_state;
    let state_result = task_set_state(task_id, TaskStatus::Ready);
    if state_result == 0 {
        let task = task_find_by_id(task_id);
        if !task.is_null() {
            let current_state = task_status(task).unwrap_or(TaskStatus::Terminated);
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
    use slopos_mm::page_alloc::{alloc_kernel_page_with, free_page_frame};
    use slopos_ostd::mm::frame::FrameAllocOptions;

    let mut stress_pages: [PhysAddr; 128] = [PhysAddr::NULL; 128];
    let mut stress_count = 0usize;

    for _ in 0..128 {
        let phys = alloc_kernel_page_with(FrameAllocOptions::single().with_no_pcp());
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

    slopos_sched::task::task_set_fs_base(parent_ptr, 0x0000_1111_2222_3000);

    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;
    let child_id = match task_clone(
        parent_ptr,
        core::ptr::null(),
        flags,
        0,
        0,
        0,
        0x0000_5555_6666_7000,
    ) {
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

    use slopos_sched::task::{task_fs_base, task_tgid};
    assert_eq_test!(
        task_tgid(child_ptr),
        task_tgid(parent_ptr),
        "thread did not join parent thread-group"
    );
    assert_eq_test!(
        task_fs_base(child_ptr),
        Some(0x0000_5555_6666_7000),
        "child TLS base not set by CLONE_SETTLS"
    );
    assert_eq_test!(
        task_fs_base(parent_ptr),
        Some(0x0000_1111_2222_3000),
        "parent TLS base unexpectedly modified"
    );

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
    let thread_id = match task_clone(parent_ptr, core::ptr::null(), thread_flags, 0, 0, 0, 0) {
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

    use slopos_sched::task::{task_parent_task_id, task_tgid};
    assert_eq_test!(
        task_tgid(thread_ptr),
        task_tgid(parent_ptr),
        "thread tgid mismatch"
    );
    assert_eq_test!(
        task_tgid(fork_ptr),
        Some(fork_id),
        "fork child should be its own thread-group leader"
    );
    assert_eq_test!(
        task_parent_task_id(fork_ptr),
        Some(parent_id),
        "fork child parent id mismatch"
    );

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wait_frame)
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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wake_frame)
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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wake_frame)
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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wait_frame)
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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
            crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wake_frame)
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
            crate::syscall::dispatch::dispatch_handler(syscall_futex, task_ptr, &mut wait_frame)
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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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

    let mut action_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    action_frame.regs_mut().rdi = SIGUSR1 as u64;
    action_frame.regs_mut().rsi = new_action_addr;
    action_frame.regs_mut().rdx = old_action_addr;
    action_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            task_ptr,
            &mut *action_frame,
        )
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

    let mut kill_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, task_ptr, &mut *kill_frame)
    });
    assert_eq_test!(kill_frame.regs().rax, 0, "kill(SIGUSR1) failed");

    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = original_rip;
    user_frame.regs_mut().rsp = original_rsp;
    user_frame.regs_mut().rax = 0xAA55;
    user_frame.regs_mut().rbx = 0xBB66;
    let _ = with_user_process_context(pid, || {
        deliver_pending_signal(task_ptr, &mut *user_frame as *mut UserContext)
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
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigreturn, task_ptr, &mut *user_frame)
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

/// Build a synthetic user-mode `InterruptFrame` carrying the given
/// RIP/RSP so the IRQ-exit delivery path treats it as a return-to-user
/// frame (`cs & 3 == 3`). Heap-backed to keep it off the test stack.
fn user_irq_frame(rip: u64, rsp: u64) -> KBox<slopos_arch::InterruptFrame> {
    let mut frame: KBox<slopos_arch::InterruptFrame> = KBox::zeroed().expect("alloc");
    frame.rip = rip;
    frame.rsp = rsp;
    frame.cs = 0x23; // user code selector (RPL 3)
    frame.ss = 0x1B; // user data selector (RPL 3)
    frame.rflags = 0x202; // IF + MBO
    frame
}

pub fn test_signal_delivery_on_irq_exit_dispatch() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Install a SIGINT handler with a restorer.
    let action = UserSigaction {
        sa_handler: 0x4100_0000,
        sa_flags: 0,
        sa_restorer: 0x4200_0000,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, page, &action),
        "failed to write SIGINT action"
    );
    let mut install_frame = zero_frame();
    install_frame.regs_mut().rdi = SIGINT as u64;
    install_frame.regs_mut().rsi = page;
    install_frame.regs_mut().rdx = 0;
    install_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            task_ptr,
            &mut install_frame,
        )
    });
    assert_eq_test!(install_frame.regs().rax, 0, "SIGINT install failed");

    // Raise SIGINT directly (mirrors what kill() leaves pending).
    let _ = task::task_signal_raise(task_ptr, sig_bit(SIGINT));

    // The IRQ-exit path resolves the task via scheduler_get_current_task().
    make_task_current(task_ptr);

    let stack_top = process_vm_get_stack_top(pid);
    let original_rip = 0x5500_2222;
    let original_rsp = stack_top.wrapping_sub(0x200);
    let mut frame = user_irq_frame(original_rip, original_rsp);

    let frame_ptr = &mut *frame as *mut slopos_arch::InterruptFrame;
    let _ = with_user_process_context(pid, || deliver_pending_signal_on_irq_exit(frame_ptr));
    // Restore the bootstrap current-task so task_terminate below does not
    // run the is_current Zombie path against the test task.
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        frame.rip,
        action.sa_handler,
        "IRQ-exit RIP not redirected to handler"
    );
    assert_eq_test!(frame.rdi, SIGINT as u64, "signum not in RDI");
    assert_eq_test!(frame.rsi, 0, "RSI not zeroed");
    assert_eq_test!(frame.rdx, 0, "RDX not zeroed");

    // The handler RSP points at the restorer word; the SignalFrame
    // begins at RSP + 8, followed by the FPU/vector save area. The frame
    // is aligned so the handler enters with `rsp % 16 == 8` (SysV ABI:
    // `(rsp + 8) % 16 == 0` at a function's entry point).
    let total_size =
        8 + core::mem::size_of::<SignalFrame>() as u64 + slopos_ostd::task::FPU_STATE_SIZE as u64;
    let expected_frame_addr = (original_rsp.wrapping_sub(total_size) & !0xF).wrapping_sub(8);
    assert_eq_test!(frame.rsp, expected_frame_addr, "handler RSP mismatch");

    let restorer_on_stack: u64 = match user_copy_in(pid, frame.rsp) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        restorer_on_stack,
        action.sa_restorer,
        "restorer not on stack"
    );

    let sigframe_addr = frame.rsp.wrapping_add(8);
    let sigframe: SignalFrame = match user_copy_in(pid, sigframe_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(sigframe.rip, original_rip, "saved RIP mismatch");
    assert_eq_test!(sigframe.rsp, original_rsp, "saved RSP mismatch");
    assert_eq_test!(sigframe.signum, SIGINT as u64, "signum mismatch in frame");

    // The handler signal must now be blocked (no SA_NODEFER) and the
    // pending bit cleared.
    let blocked = task::task_signal_pending(task_ptr); // pending cleared
    assert_eq_test!(blocked, 0, "pending SIGINT bit should be cleared");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_delivery_on_irq_exit_kernel_frame_untouched() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    let _ = task::task_signal_raise(task_ptr, sig_bit(SIGINT));
    make_task_current(task_ptr);

    let original_rip = 0xFFFF_8000_0001_0000;
    let original_rsp = 0xFFFF_8000_0002_0000;
    let mut frame = user_irq_frame(original_rip, original_rsp);
    // Kernel-mode return: CS RPL 0.
    frame.cs = 0x08;
    frame.ss = 0x10;

    let frame_ptr = &mut *frame as *mut slopos_arch::InterruptFrame;
    deliver_pending_signal_on_irq_exit(frame_ptr);
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        frame.rip,
        original_rip,
        "kernel-mode frame RIP must be untouched"
    );
    assert_eq_test!(
        frame.rsp,
        original_rsp,
        "kernel-mode frame RSP must be untouched"
    );
    // Pending bit must still be set — nothing was delivered.
    assert_eq_test!(
        task::task_signal_pending(task_ptr) & sig_bit(SIGINT),
        sig_bit(SIGINT),
        "pending SIGINT must remain when frame is kernel-mode"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_delivery_on_irq_exit_copy_failure_rearms() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    let action = UserSigaction {
        sa_handler: 0x4100_0000,
        sa_flags: 0,
        sa_restorer: 0x4200_0000,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, page, &action),
        "failed to write SIGINT action"
    );
    let mut install_frame = zero_frame();
    install_frame.regs_mut().rdi = SIGINT as u64;
    install_frame.regs_mut().rsi = page;
    install_frame.regs_mut().rdx = 0;
    install_frame.regs_mut().r10 = core::mem::size_of::<SigSet>() as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            task_ptr,
            &mut install_frame,
        )
    });
    assert_eq_test!(install_frame.regs().rax, 0, "SIGINT install failed");

    let _ = task::task_signal_raise(task_ptr, sig_bit(SIGINT));
    make_task_current(task_ptr);

    // RSP points into an unmapped user address: the restorer copy_to_user
    // fails, so delivery must abort, re-arm the pending bit, and leave the
    // frame unmodified.
    let original_rip = 0x5500_3333;
    let original_rsp = 0x0000_3000_0000_0000; // valid user range, unmapped
    let mut frame = user_irq_frame(original_rip, original_rsp);

    let frame_ptr = &mut *frame as *mut slopos_arch::InterruptFrame;
    let _ = with_user_process_context(pid, || deliver_pending_signal_on_irq_exit(frame_ptr));
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        frame.rip,
        original_rip,
        "frame RIP must be untouched on copy failure"
    );
    assert_eq_test!(
        frame.rsp,
        original_rsp,
        "frame RSP must be untouched on copy failure"
    );
    assert_eq_test!(
        task::task_signal_pending(task_ptr) & sig_bit(SIGINT),
        sig_bit(SIGINT),
        "pending SIGINT must be re-armed after copy_to_user failure"
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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            task_ptr,
            &mut install_frame,
        )
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
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigprocmask,
            task_ptr,
            &mut block_frame,
        )
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
        crate::syscall::dispatch::dispatch_handler(syscall_kill, task_ptr, &mut kill_frame)
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
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigprocmask,
            task_ptr,
            &mut unblock_frame,
        )
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
    // SIGCHLD signal delivery is independent of the wait-wakeup
    // mechanism (waitpid wakes via the per-task `waiters` WaitQueue,
    // exercised by `sched/src/sched_tests.rs`'s
    // `test_task_wait_*`). This test's scope is the SIGCHLD
    // pending-bit propagation through the send-time disposition gate:
    // SIGCHLD's default is Ignore, so an unblocked default-disposition
    // parent never accumulates the bit, while a parent that blocked
    // SIGCHLD (the signalfd pattern) must still see it pend.
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_ptr = task_find_by_id(parent_id);
    assert_not_null!(parent_ptr, "parent lookup failed");

    // Unblocked + SIG_DFL: the exit-path raise is dropped at the send
    // site — no stale pending bit, no spurious wake.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "task_fork failed");
    task_set_state(child_id, TaskStatus::Blocked);

    assert_eq_test!(task_terminate(child_id), 0, "failed to terminate child");

    let pending = task_signal_pending(parent_ptr);
    assert_eq_test!(
        pending & sig_bit(SIGCHLD),
        0,
        "default-ignored SIGCHLD must be dropped at send, not pend"
    );

    // Blocked SIGCHLD: the bit must pend so a signalfd drain (or a
    // later-installed handler) can observe the exit.
    if let Some(t) = task::task_borrow_mut(parent_ptr) {
        t.signal_blocked = sig_bit(SIGCHLD);
    }

    let child2_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child2_id != INVALID_TASK_ID, "second task_fork failed");
    task_set_state(child2_id, TaskStatus::Blocked);

    assert_eq_test!(
        task_terminate(child2_id),
        0,
        "failed to terminate second child"
    );

    let pending = task_signal_pending(parent_ptr);
    assert_test!(
        (pending & sig_bit(SIGCHLD)) != 0,
        "parent missing SIGCHLD pending bit after child exit (SIGCHLD blocked)"
    );

    task_terminate(parent_id);
    TestResult::Pass
}

pub fn test_arch_prctl_set_get_fs_roundtrip() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
        crate::syscall::dispatch::dispatch_handler(syscall_arch_prctl, task_ptr, &mut set_frame)
    });
    assert_eq_test!(set_frame.regs().rax, 0, "ARCH_SET_FS failed");

    let mut get_frame = zero_frame();
    get_frame.regs_mut().rdi = ARCH_GET_FS;
    get_frame.regs_mut().rsi = out_addr;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_arch_prctl, task_ptr, &mut get_frame)
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
        core::ptr::null(),
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
    assert_eq_test!(
        task_fs_base(child_ptr),
        Some(expected_fs),
        "clone without CLONE_SETTLS must inherit FS base"
    );

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
    let mut drain: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let drained = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut *drain));
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

    let pid1 = task_process_id(p1).unwrap_or(0);
    let pid2 = task_process_id(p2).unwrap_or(0);

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

/// Fork-style clones keep close-on-exec descriptors (POSIX fork; only exec
/// strips them). Spawn no longer whole-table clones — see the fd-action tests.
pub fn test_fork_clone_keeps_cloexec_fds() -> TestResult {
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

    let pid1 = task_process_id(p1).unwrap_or(0);
    let pid2 = task_process_id(p2).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );
    assert_eq_test!(
        file_fcntl_fd(pid1, write_fd, F_SETFD, FD_CLOEXEC),
        0,
        "set cloexec failed"
    );

    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2),
        0,
        "fork clone failed"
    );
    assert_eq_test!(
        file_close_fd(pid2, write_fd),
        0,
        "fork clone must keep cloexec descriptors"
    );
    assert_eq_test!(file_close_fd(pid2, read_fd), 0, "pid2 close read failed");

    assert_eq_test!(file_close_fd(pid1, write_fd), 0, "pid1 close write failed");
    assert_eq_test!(file_close_fd(pid1, read_fd), 0, "pid1 close read failed");
    task_terminate(t1);
    task_terminate(t2);
    TestResult::Pass
}

/// A spawned child with no actions starts with an empty fd table — no stdio.
pub fn test_spawn_empty_table_unless_actions() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p = task_find_by_id(t);
    assert_not_null!(p, "task lookup failed");
    let pid = task_process_id(p).unwrap_or(0);

    // The bootstrap console table is replaced with an empty one.
    fileio_destroy_table_for_process(pid);
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid),
        0,
        "create empty failed"
    );

    assert_test!(file_close_fd(pid, 0) != 0, "fd 0 should be absent");
    assert_test!(file_close_fd(pid, 1) != 0, "fd 1 should be absent");
    assert_test!(file_close_fd(pid, 2) != 0, "fd 2 should be absent");

    task_terminate(t);
    TestResult::Pass
}

/// CloneFd shares the description: after the parent closes its own write end,
/// the child's cloned end keeps the pipe open, so the reader sees no EOF.
pub fn test_spawn_clone_fd_shares_backing() -> TestResult {
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
    let pid1 = task_process_id(p1).unwrap_or(0);
    let pid2 = task_process_id(p2).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2),
        0,
        "create empty failed"
    );
    let actions = [FdAction::Clone {
        src_fd: write_fd,
        target_fd: 1,
    }];
    assert_test!(
        apply_fd_actions(pid1, pid2, &actions).is_ok(),
        "clone action failed"
    );

    // Parent drops its write end; the child's clone still keeps the pipe
    // writable, so a nonblocking read is not EOF.
    assert_eq_test!(file_close_fd(pid1, write_fd), 0, "pid1 close write failed");
    let mut one = [0u8; 1];
    let r = file_read_fd(pid1, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_test!(
        r != 0,
        "reader must not see EOF while child holds a write end"
    );

    // Dropping the child's clone too → EOF.
    assert_eq_test!(file_close_fd(pid2, 1), 0, "pid2 close write failed");
    let r2 = file_read_fd(pid1, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_eq_test!(r2, 0, "reader should see EOF after every write end closes");

    assert_eq_test!(file_close_fd(pid1, read_fd), 0, "pid1 close read failed");
    task_terminate(t1);
    task_terminate(t2);
    TestResult::Pass
}

/// TransferFd moves the description: the parent slot is emptied and the child
/// receives it at the target fd.
pub fn test_spawn_transfer_fd_moves() -> TestResult {
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
    let pid1 = task_process_id(p1).unwrap_or(0);
    let pid2 = task_process_id(p2).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2),
        0,
        "create empty failed"
    );
    let actions = [FdAction::Transfer {
        src_fd: write_fd,
        target_fd: 3,
    }];
    assert_test!(
        apply_fd_actions(pid1, pid2, &actions).is_ok(),
        "transfer action failed"
    );

    assert_test!(
        file_close_fd(pid1, write_fd) != 0,
        "parent slot must be emptied by transfer"
    );
    assert_eq_test!(
        file_close_fd(pid2, 3),
        0,
        "child must hold the transferred fd"
    );

    assert_eq_test!(file_close_fd(pid1, read_fd), 0, "pid1 close read failed");
    task_terminate(t1);
    task_terminate(t2);
    TestResult::Pass
}

/// A mid-list bad fd aborts the whole action list, and the parent table is
/// untouched: a `Transfer` staged before the failing action must not have
/// emptied its parent slot or closed the description.
pub fn test_spawn_actions_all_or_nothing() -> TestResult {
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
    let pid1 = task_process_id(p1).unwrap_or(0);
    let pid2 = task_process_id(p2).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2);
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2),
        0,
        "create empty failed"
    );

    // fd 99 is out of range → the second action fails and the list aborts.
    let actions = [
        FdAction::Transfer {
            src_fd: write_fd,
            target_fd: 0,
        },
        FdAction::Clone {
            src_fd: 99,
            target_fd: 1,
        },
    ];
    assert_test!(
        apply_fd_actions(pid1, pid2, &actions).is_err(),
        "a bad src fd must abort the action list"
    );

    // The aborted spawn tears the child table down; the parent must still
    // hold a live write end, so the reader sees no EOF.
    fileio_destroy_table_for_process(pid2);
    let mut one = [0u8; 1];
    let r = file_read_fd(pid1, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_test!(r != 0, "parent write end must survive the aborted transfer");
    assert_eq_test!(
        file_close_fd(pid1, write_fd),
        0,
        "parent slot must still hold the transferred-then-aborted fd"
    );

    assert_eq_test!(file_close_fd(pid1, read_fd), 0, "pid1 close read failed");
    task_terminate(t1);
    task_terminate(t2);
    TestResult::Pass
}

/// The spawn ABI reads its `SpawnAttrs` by pointer (arg4 = attrs_ptr). A bad
/// attrs pointer is rejected with EFAULT before exec; a valid one reaches the
/// exec path and reports the real load error.
pub fn test_spawn_path_rejects_bad_attrs() -> TestResult {
    use crate::syscall::handlers::syscall_spawn_path;
    use slopos_abi::spawn::SpawnAttrs;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    // Map a user page; write a path that will fail at VFS open plus a valid
    // attrs struct (Normal priority, no fd actions) at a non-overlapping offset.
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
    let attrs = SpawnAttrs {
        priority: 2, // TaskPriority::Normal (KernelIo=1 is kernel-only)
        _pad: [0; 3],
        flags: 0,
        _pad2: 0,
        actions_ptr: 0,
        actions_len: 0,
        sigdefault_mask: 0,
    };
    let attrs_addr = user_page + 512;
    assert_test!(
        user_copy_out(pid, attrs_addr, &attrs),
        "failed to write attrs into user memory"
    );

    // ---- Case A: garbage attrs pointer → EFAULT before exec ----
    let mut frame_bad = zero_frame();
    frame_bad.regs_mut().rdi = user_page; // path_ptr
    frame_bad.regs_mut().rsi = path.len() as u64; // path_len
    frame_bad.regs_mut().rdx = 0; // argv_ptr
    frame_bad.regs_mut().r10 = 0; // argc
    frame_bad.regs_mut().r8 = 0xDEAD_BEEF_CAFE_BABEu64; // attrs_ptr (garbage)
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_spawn_path, task_ptr, &mut frame_bad)
    });
    assert_eq_test!(
        frame_bad.regs().rax,
        slopos_abi::Errno::EFAULT.as_u64(),
        "garbage attrs pointer must return EFAULT"
    );

    // ---- Case B: valid attrs, missing binary → reaches exec → NoEntry (-2) ----
    let mut frame_ok = zero_frame();
    frame_ok.regs_mut().rdi = user_page;
    frame_ok.regs_mut().rsi = path.len() as u64;
    frame_ok.regs_mut().rdx = 0;
    frame_ok.regs_mut().r10 = 0;
    frame_ok.regs_mut().r8 = attrs_addr;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_spawn_path, task_ptr, &mut frame_ok)
    });
    // ExecError::NoEntry = -2, returned via ctx.ok(err as i32 as u64).
    let exec_no_entry = (-2i32) as u64;
    assert_eq_test!(
        frame_ok.regs().rax,
        exec_no_entry,
        "valid attrs with missing binary must reach exec and return NoEntry"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// execve resets caught handlers to SIG_DFL but preserves SIG_IGN and SIG_DFL
/// (the stale-handler-pointer fix).
pub fn test_execve_resets_caught_signals_keeps_ignored() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p = task_find_by_id(t);
    assert_not_null!(p, "task lookup failed");

    // SIGINT caught (custom handler), SIGTSTP ignored, SIGTERM default.
    if let Some(task) = task::task_borrow_mut(p) {
        task.signal_actions[(SIGINT - 1) as usize] = SignalAction {
            handler: 0x4100_0000,
            mask: 0,
            flags: 0,
            restorer: 0,
        };
        task.signal_actions[(SIGTSTP - 1) as usize] = SignalAction {
            handler: SIG_IGN,
            mask: 0,
            flags: 0,
            restorer: 0,
        };
        task.signal_actions[(SIGTERM - 1) as usize] = SignalAction::default();
    }

    if let Some(task) = task::task_borrow_mut(p) {
        task::task_reset_caught_handlers(task);
    }

    let ok = if let Some(task) = task::task_borrow_mut(p) {
        task.signal_actions[(SIGINT - 1) as usize].handler == slopos_abi::signal::SIG_DFL
            && task.signal_actions[(SIGTSTP - 1) as usize].handler == SIG_IGN
            && task.signal_actions[(SIGTERM - 1) as usize].handler == slopos_abi::signal::SIG_DFL
    } else {
        false
    };

    task_terminate(t);
    if ok {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// `sigdefault` (POSIX_SPAWN_SETSIGDEF and the syscall) forces masked signals
/// to SIG_DFL, overriding a caught handler or SIG_IGN.
pub fn test_sigdefault_forces_default_over_ignore() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p = task_find_by_id(t);
    assert_not_null!(p, "task lookup failed");

    if let Some(task) = task::task_borrow_mut(p) {
        task.signal_actions[(SIGINT - 1) as usize] = SignalAction {
            handler: 0x4100_0000,
            mask: 0,
            flags: 0,
            restorer: 0,
        };
        task.signal_actions[(SIGTSTP - 1) as usize] = SignalAction {
            handler: SIG_IGN,
            mask: 0,
            flags: 0,
            restorer: 0,
        };
    }

    let mask = slopos_abi::signal::sig_bit(SIGINT) | slopos_abi::signal::sig_bit(SIGTSTP);
    if let Some(task) = task::task_borrow_mut(p) {
        task::task_default_signals_in_mask(task, mask);
    }

    let ok = if let Some(task) = task::task_borrow_mut(p) {
        task.signal_actions[(SIGINT - 1) as usize].handler == slopos_abi::signal::SIG_DFL
            && task.signal_actions[(SIGTSTP - 1) as usize].handler == slopos_abi::signal::SIG_DFL
    } else {
        false
    };

    task_terminate(t);
    if ok {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
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
        task_controlling_tty(task_ptr),
        None,
        "fresh task should have no controlling_tty"
    );

    let pid = task_process_id(task_ptr).unwrap_or(0);
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, task_ptr, &mut frame);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed");
    assert_eq_test!(
        task_controlling_tty(task_ptr),
        Some(TtyIndex(0)),
        "controlling_tty should be set after TIOCSCTTY"
    );

    // Now open /dev/tty — should succeed.
    let pid = task_process_id(task_ptr).unwrap_or(0);
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, parent_ptr, &mut frame);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed");

    // Fork a child — it inherits the controlling terminal.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    assert_eq_test!(
        task_controlling_tty(child_ptr),
        Some(TtyIndex(0)),
        "child should inherit controlling_tty from parent"
    );

    // Child calls setsid() — controlling terminal cleared.
    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, child_ptr, &mut setsid_frame);
    assert_eq_test!(
        task_controlling_tty(child_ptr),
        None,
        "setsid should clear controlling_tty"
    );

    // Now child tries to open /dev/tty — should fail with ENXIO.
    let child_pid = task_process_id(child_ptr).unwrap_or(0);
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
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, parent_ptr, &mut frame);
    assert_eq_test!(frame.regs().rax, 0, "TIOCSCTTY should succeed for parent");

    // Fork child.
    let child_id = task_fork(parent_ptr, core::ptr::null());
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_ptr = task_find_by_id(child_id);
    assert_not_null!(child_ptr, "child lookup failed");

    // Child should have inherited controlling_tty.
    let parent_ctty = task_controlling_tty(parent_ptr);
    let child_ctty = task_controlling_tty(child_ptr);
    assert_eq_test!(
        parent_ctty,
        child_ctty,
        "child should inherit same controlling_tty as parent"
    );

    // Child opens /dev/tty — should succeed (inherits parent's ctty).
    let child_pid = task_process_id(child_ptr).unwrap_or(0);
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
    let Some(entry_ref) = syscall_lookup(SYSCALL_VHANGUP) else {
        klog_info!("SYSCALL_VHANGUP lookup returned None");
        return TestResult::Fail;
    };
    assert_test!(
        entry_ref.handler.is_some(),
        "SYSCALL_VHANGUP has no handler"
    );
    TestResult::Pass
}

// ── Resource-lifetime redesign S1: fd-table single-owner semantics ──────────

/// dup must NOT copy the close-on-exec flag: cloexec is per-fd-entry, not
/// shared on the open file. The source fd keeps its cloexec bit; the new
/// fd is created without it. (Audit defect D1.)
pub fn test_dup_does_not_copy_cloexec() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    // Mark the source cloexec.
    assert_eq_test!(
        file_fcntl_fd(pid, write_fd, F_SETFD, FD_CLOEXEC),
        0,
        "set cloexec failed"
    );

    let dup_fd = file_dup_fd(pid, write_fd);
    assert_test!(dup_fd >= 0, "dup failed");

    // Source still cloexec; the dup is not.
    assert_eq_test!(
        file_fcntl_fd(pid, write_fd, slopos_abi::syscall::F_GETFD, 0),
        FD_CLOEXEC as i64,
        "source fd must retain cloexec"
    );
    assert_eq_test!(
        file_fcntl_fd(pid, dup_fd, slopos_abi::syscall::F_GETFD, 0),
        0,
        "dup fd must NOT inherit cloexec"
    );

    // dup3 with O_CLOEXEC, by contrast, sets it on the target only.
    let target = 20;
    assert_eq_test!(
        file_dup3_fd(pid, read_fd, target, FD_CLOEXEC as u32),
        target,
        "dup3 failed"
    );
    assert_eq_test!(
        file_fcntl_fd(pid, target, slopos_abi::syscall::F_GETFD, 0),
        FD_CLOEXEC as i64,
        "dup3 with O_CLOEXEC must set cloexec on the new fd"
    );
    assert_eq_test!(
        file_fcntl_fd(pid, read_fd, slopos_abi::syscall::F_GETFD, 0),
        0,
        "dup3 must not touch the source cloexec"
    );

    let _ = file_close_fd(pid, dup_fd);
    let _ = file_close_fd(pid, target);
    let _ = file_close_fd(pid, write_fd);
    let _ = file_close_fd(pid, read_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// Closing an fd twice is safe: the first close tears the entry out and
/// drops it; the second finds an empty slot and returns EBADF — never a
/// double teardown of the backing object. (Audit defect class D1/D2.)
pub fn test_close_twice_is_safe() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    assert_eq_test!(
        file_close_fd(pid, write_fd),
        0,
        "first close should succeed"
    );
    assert_test!(
        file_close_fd(pid, write_fd) != 0,
        "second close of the same fd must fail (EBADF), not double-teardown"
    );

    let _ = file_close_fd(pid, read_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// Closing one of two dup'd write ends must NOT tear down the shared
/// backing: the peer reader must not observe EOF until the *last* write
/// alias closes. Exercises single-owner teardown (last KArc drop == one
/// release). (Audit defect class D1/D2.)
pub fn test_close_while_dup_keeps_object_alive() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let write_dup = file_dup_fd(pid, write_fd);
    assert_test!(write_dup >= 0, "dup of write end failed");

    // Close one write alias: the writer count must NOT reach zero, so a
    // nonblocking read sees EAGAIN (no data, writers still present), not
    // EOF (0).
    assert_eq_test!(
        file_close_fd(pid, write_fd),
        0,
        "close one write alias failed"
    );
    let mut buf = [0u8; 4];
    let r = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut buf));
    assert_eq_test!(
        r,
        ERRNO_EAGAIN as isize,
        "reader must NOT see EOF while a write alias is still open"
    );

    // Close the last write alias: now the reader observes EOF (0).
    assert_eq_test!(
        file_close_fd(pid, write_dup),
        0,
        "close last write alias failed"
    );
    let eof = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut buf));
    assert_eq_test!(
        eof,
        0,
        "reader must see EOF after the LAST write alias closes"
    );

    let _ = file_close_fd(pid, read_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// Forcing EMFILE on a tty open must tear down only the open that
/// failed: the backing clone minted for the attempt drops exactly once
/// inside the failed install, and the live tty (and its pair) survives
/// untouched. A second teardown is unrepresentable — there is no
/// release call for a caller to balance.
pub fn test_open_tty_fd_emfile_no_double_teardown() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

    // Open a PTY master to get a real, independently-tracked tty index.
    let master_fd = file_open_for_process(pid, b"/dev/ptmx", O_RDONLY);
    assert_test!(master_fd >= 0, "ptmx open failed");
    let Some(master_tty) = slopos_fs::fileio::file_get_tty_index(pid, master_fd) else {
        let _ = file_close_fd(pid, master_fd);
        task_terminate(task_id);
        return TestResult::Fail;
    };

    // Fill the fd table so the next install hits EMFILE. dup the master
    // into every remaining slot.
    loop {
        let fd = file_dup_fd(pid, master_fd);
        if fd < 0 {
            break;
        }
    }

    // Baseline: hold a probe clone of the live backing and record the
    // strong count (fd aliases share one OpenFile, so the count here is
    // the fd-table owner plus this probe).
    let probe_backing = match slopos_kernel_services::syscall_services::tty::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            let _ = file_close_fd(pid, master_fd);
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let before = slopos_ostd::KArc::strong_count(&probe_backing);

    // Mirror the syscall caller: mint a backing, then attempt the open
    // with a full table. The failed install consumes and drops the mint.
    let mint = match slopos_kernel_services::syscall_services::tty::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let probe = file_open_tty_fd(pid, master_tty, 0, mint);
    assert_test!(probe < 0, "open should fail with a full fd table");

    // The failed open's mint dropped exactly once: the count is back at
    // the baseline and the tty is still open (not collapsed).
    let after = slopos_ostd::KArc::strong_count(&probe_backing);
    assert_eq_test!(
        after,
        before,
        "backing strong count must return to baseline after a failed open"
    );
    assert_test!(
        slopos_kernel_services::syscall_services::tty::open_tty(master_tty).is_ok(),
        "tty must still be open after the failed install"
    );

    drop(probe_backing);
    let _ = file_close_fd(pid, master_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// TTY ioctls must never change the terminal's open state: only opening
/// (cloning the backing) and closing (dropping a clone) may move the
/// strong count. Exercises a representative set of state-touching
/// service calls against a live PTY and asserts the count is untouched.
pub fn test_tty_ioctl_never_changes_open_state() -> TestResult {
    use slopos_kernel_services::syscall_services::tty as ttysvc;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

    let master_fd = file_open_for_process(pid, b"/dev/ptmx", O_RDONLY);
    assert_test!(master_fd >= 0, "ptmx open failed");
    let Some(master_tty) = slopos_fs::fileio::file_get_tty_index(pid, master_fd) else {
        task_terminate(task_id);
        return TestResult::Fail;
    };

    let probe = match ttysvc::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let baseline = slopos_ostd::KArc::strong_count(&probe);

    // Terminal configuration, sizing, PTY control, exclusivity — the
    // ioctl surface the syscall handler dispatches to.
    if let Ok(t) = ttysvc::get_termios(master_tty) {
        let _ = ttysvc::set_termios(master_tty, &t);
    }
    if let Ok(ws) = ttysvc::get_winsize(master_tty) {
        let _ = ttysvc::set_winsize(master_tty, &ws);
    }
    let _ = ttysvc::get_pty_number(master_tty);
    let _ = ttysvc::set_pty_lock(master_tty, true);
    let _ = ttysvc::get_pty_lock(master_tty);
    let _ = ttysvc::set_pty_lock(master_tty, false);
    let _ = ttysvc::set_packet_mode(master_tty, true);
    let _ = ttysvc::set_packet_mode(master_tty, false);
    let _ = ttysvc::set_exclusive(master_tty, true);
    let _ = ttysvc::get_exclusive(master_tty);
    let _ = ttysvc::set_exclusive(master_tty, false);
    let _ = ttysvc::bytes_available(master_tty);
    let _ = ttysvc::tcflush(master_tty, 2);

    assert_eq_test!(
        slopos_ostd::KArc::strong_count(&probe),
        baseline,
        "ioctls must not change the tty open state"
    );

    drop(probe);
    let _ = file_close_fd(pid, master_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// Passing a TTY fd via SCM_RIGHTS must be lifetime-balanced: in-flight
/// custody shares the sender's open-file description (no shadow
/// reference on the tty backing), and the receiver's close never tears
/// down the sender's terminal.
pub fn test_scm_rights_tty_balanced() -> TestResult {
    use slopos_kernel_services::syscall_services::tty as ttysvc;
    use slopos_net::unix_socket;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    let master_fd = file_open_for_process(pid, b"/dev/ptmx", O_RDONLY);
    assert_test!(master_fd >= 0, "ptmx open failed");
    let Some(master_tty) = slopos_fs::fileio::file_get_tty_index(pid, master_fd) else {
        task_terminate(task_id);
        return TestResult::Fail;
    };
    let probe = match ttysvc::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let baseline = slopos_ostd::KArc::strong_count(&probe);

    // Send the tty fd through the pair. The in-flight alias shares the
    // fd's open-file description, so the backing count stays put.
    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, master_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);
    let n = unix_socket::unix_sendmsg(srv, b"T", &mut files);
    assert_test!(n == 1, "sendmsg returned {}", n);
    assert_eq_test!(
        slopos_ostd::KArc::strong_count(&probe),
        baseline,
        "in-flight custody must not mint a shadow tty reference"
    );

    // Receive + install: the fd lands on the same terminal.
    let mut buf = [0u8; 4];
    let mut out: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("out vec alloc");
    let (bytes_read, n_fds) = unix_socket::unix_recvmsg(cli, &mut buf, &mut out, 1);
    assert_eq_test!(bytes_read, 1, "recvmsg byte count");
    assert_eq_test!(n_fds, 1, "recvmsg must deliver the tty fd");
    let recv_fd = slopos_fs::fileio_install_file_ref(pid, out.pop().expect("delivered file"));
    assert_test!(recv_fd >= 0, "install failed");
    assert_eq_test!(
        slopos_fs::fileio::file_get_tty_index(pid, recv_fd),
        Some(master_tty),
        "received fd must reference the same terminal"
    );

    // Receiver close: balanced — the sender's terminal survives.
    let _ = file_close_fd(pid, recv_fd);
    assert_eq_test!(
        slopos_ostd::KArc::strong_count(&probe),
        baseline,
        "receiver close must be balanced against the shared description"
    );
    assert_test!(
        ttysvc::open_tty(master_tty).is_ok(),
        "sender's tty must still be open after the receiver closes"
    );

    drop(probe);
    let _ = file_close_fd(pid, master_fd);
    unix_socket::unix_close(srv);
    unix_socket::unix_close(cli);
    task_terminate(task_id);
    TestResult::Pass
}

/// dup'd fds share one open file description, so they share the file
/// offset (POSIX). Writing through one dup advances the offset seen by
/// the other. Pins the AtomicU64 shared-position semantics.
/// Synthetic seekable backing for [`test_dup_shares_offset`]: a 64-byte
/// "file" whose content at offset `o` is the byte `o`. No filesystem
/// dependency — the stest phase runs before any disk is mounted, and the
/// property under test (dup aliases share one offset) lives entirely in
/// the `OpenFile` layer.
struct SeekProbeOps;

const SEEK_PROBE_SIZE: u64 = 64;

impl slopos_abi::FileOps for SeekProbeOps {
    fn kind(&self) -> slopos_abi::FileKind {
        slopos_abi::FileKind::Regular
    }

    fn read(
        &self,
        _handle: usize,
        buf: &mut dyn slopos_abi::io::IoBufWrite,
        offset: u64,
        _flags: u32,
    ) -> isize {
        let mut written = 0usize;
        while written < buf.len() && offset + (written as u64) < SEEK_PROBE_SIZE {
            let byte = [(offset + written as u64) as u8];
            if buf.copy_in(written, &byte).is_err() {
                break;
            }
            written += 1;
        }
        written as isize
    }

    fn write(
        &self,
        _handle: usize,
        buf: &dyn slopos_abi::io::IoBufRead,
        _offset: u64,
        _flags: u32,
    ) -> isize {
        buf.len() as isize
    }

    fn seekable(&self) -> bool {
        true
    }

    fn size(&self, _handle: usize) -> Option<u64> {
        Some(SEEK_PROBE_SIZE)
    }
}

static SEEK_PROBE_OPS: SeekProbeOps = SeekProbeOps;

pub fn test_dup_shares_offset() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let fd = slopos_fs::fileio::fileio_open_fd_with_ops(pid, &SEEK_PROBE_OPS, 0, None);
    assert_test!(fd >= 0, "synthetic open failed");

    let dup = file_dup_fd(pid, fd);
    assert_test!(dup >= 0, "dup failed");

    // Read through the original; the shared offset advances for both.
    let mut first = [0u8; 4];
    let r = file_read_fd(pid, fd, &mut KernelIoBuf::new(&mut first));
    assert_eq_test!(r as usize, first.len(), "read via original failed");
    assert_test!(first == [0, 1, 2, 3], "content at offset 0 mismatch");

    // The dup sees the advanced offset: SEEK_CUR(0) reports the position
    // the original's read left behind.
    let pos = file_seek_fd(pid, dup, 0, slopos_abi::syscall::SEEK_CUR as u32);
    assert_eq_test!(
        pos,
        first.len() as i64,
        "dup must observe the shared offset advanced by the original's read"
    );

    // Continue reading through the DUP: it picks up where the original
    // stopped (one offset, two fds).
    let mut second = [0u8; 4];
    let r = file_read_fd(pid, dup, &mut KernelIoBuf::new(&mut second));
    assert_eq_test!(r as usize, second.len(), "read via dup failed");
    assert_test!(second == [4, 5, 6, 7], "dup read must continue the offset");

    // Rewind via the dup; the original observes the rewind (shared).
    let rewound = file_seek_fd(pid, dup, 0, slopos_abi::syscall::SEEK_SET as u32);
    assert_eq_test!(rewound, 0, "seek to start via dup failed");
    let mut again = [0u8; 4];
    let r = file_read_fd(pid, fd, &mut KernelIoBuf::new(&mut again));
    assert_eq_test!(r as usize, again.len(), "read after dup-rewind failed");
    assert_test!(
        again == [0, 1, 2, 3],
        "original must observe the rewind done via the dup"
    );

    let _ = file_close_fd(pid, dup);
    let _ = file_close_fd(pid, fd);
    task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_syscall_lookup_invalid_number,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_syscall_lookup_empty_slot, suite = syscall_valid);
slopos_testing::stest!(
    name = test_index_tty_io_syscalls_retired,
    suite = syscall_valid
);
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
    name = test_signal_delivery_on_irq_exit_dispatch,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_signal_delivery_on_irq_exit_kernel_frame_untouched,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_signal_delivery_on_irq_exit_copy_failure_rearms,
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
    name = test_fork_clone_keeps_cloexec_fds,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_spawn_empty_table_unless_actions,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_spawn_clone_fd_shares_backing,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_spawn_transfer_fd_moves, suite = syscall_valid);
slopos_testing::stest!(
    name = test_spawn_actions_all_or_nothing,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_dup_does_not_copy_cloexec, suite = syscall_valid);
slopos_testing::stest!(name = test_close_twice_is_safe, suite = syscall_valid);
slopos_testing::stest!(
    name = test_close_while_dup_keeps_object_alive,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_open_tty_fd_emfile_no_double_teardown,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_tty_ioctl_never_changes_open_state,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_scm_rights_tty_balanced, suite = unix_scm_rights);
slopos_testing::stest!(name = test_dup_shares_offset, suite = syscall_valid);
slopos_testing::stest!(
    name = test_process_group_session_syscalls_baseline,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_kill_process_group_semantics,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_process_group_object_fork_and_setsid_identity,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_process_group_session_dag_lifetime,
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
    name = test_tty_poll_after_close_reuse_no_crossobject,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_vm_mmap_munmap_stress_baseline,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_spawn_path_rejects_bad_attrs,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_execve_resets_caught_signals_keeps_ignored,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_sigdefault_forces_default_over_ignore,
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
    name = test_process_group_object_fork_and_setsid_identity,
    suite = syscall_compat_smoke
);
slopos_testing::stest!(
    name = test_process_group_session_dag_lifetime,
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
    name = test_spawn_path_rejects_bad_attrs,
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

    let srv_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(accepted_handle)?;
    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
        Some(srv_backing),
    );
    let cli_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(cli_handle)?;
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
        Some(cli_backing),
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
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

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
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
// Wait/wake state machine tests — Phase 5 collapsed `WillBlock` into
// the wait queue's lock-held `Running → Blocked` CAS. The tests below
// exercise the post-Phase-5 protocol directly.
// =============================================================================

/// `sleep_current_task_ms`'s `CAS(Running, Blocked)` must fail when the
/// task isn't `Running`. This is the post-Phase-5 invariant: a wait
/// path that already CAS'd the task to `Blocked` (under a wait queue's
/// SpinLock) must reject a concurrent sleep-blocking attempt.
pub fn test_sleep_ms_cas_overwrites_wakeup() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // Set up: task is `Blocked` at creation. Publish it Ready, then Running,
    // before modelling the wait queue's lock-held CAS.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    let blocked_cas = task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::IoWait,
    );
    assert_eq_test!(blocked_cas, 0);

    // A racing sleep_current_task_ms call would now try CAS(Running, Blocked).
    // It must fail — the task is no longer Running.
    let result = task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::Sleep,
    );

    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Blocked, "state stays Blocked");
    assert_test!(
        result != 0,
        "CAS(Running, Blocked) should fail when task is already Blocked"
    );

    let _ = task_set_state(task_id, TaskStatus::Ready);
    task_terminate(task_id);
    TestResult::Pass
}

/// After a wake transitions the task `Blocked → Ready`, a stale
/// blocker that retries `CAS(Running, Blocked)` must fail. Models the
/// post-Phase-5 race: a wait-queue waiter committed to `Blocked` under
/// the queue lock; the producer's `wake_one` CAS-flipped the waiter to
/// `Ready`; a buggy retry path that looked up the task and tried to
/// re-block it would fail because the state is no longer `Running`.
pub fn test_block_current_task_toctou_allows_reblock() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // 1. Task is Running.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    // 2. Wait-queue protocol commits Blocked under the queue lock.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0);
    // 3. Producer wakes the task: Blocked → Ready.
    let cas_wake = task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready);
    assert_eq_test!(cas_wake, 0);

    // 4. A stale "block again" CAS source `Running` must fail.
    let result = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Ready, "state should still be Ready");
    assert_test!(result != 0, "CAS(Running, Blocked) should fail from Ready");

    task_terminate(task_id);
    TestResult::Pass
}

/// Models the wait-queue protocol: under the queue's SpinLock, a
/// waiter CAS-flips Running → Blocked. A racing producer that takes
/// the same lock observes the committed Blocked state and CAS-flips it
/// to Ready via `unblock_task`. Both transitions must succeed.
pub fn test_wq_wrong_order_wakeup_lost() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // 1. Task starts Running (precondition for wait-queue's lock-held CAS).
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    // 2. Wait-queue protocol: under the queue lock, push node + CAS.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0);

    // 3. Producer wakeup: unblock_task CAS(Blocked → Ready).
    let result = unblock_task(task_ptr);
    assert_eq_test!(result, 0, "unblock_task should succeed from Blocked");

    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(
        state,
        TaskStatus::Ready,
        "wakeup transitions Blocked → Ready"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// Positive counterpart to `test_wq_wrong_order_wakeup_lost`: an
/// `unblock_task` against a task that is still `Running` (the
/// producer's `wake_one` ran before the consumer entered the queue
/// lock) is a benign no-op — the consumer's subsequent re-check inside
/// the lock observes the producer's update and skips the block.
pub fn test_wq_correct_order_wakeup_preserved() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    // unblock_task on a Running task is a no-op — there's nothing to
    // unblock, but the call must not corrupt the state.
    let result = unblock_task(task_ptr);
    assert_eq_test!(result, 0, "unblock_task on Running task is a no-op");

    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Running, "state stays Running");

    task_terminate(task_id);
    TestResult::Pass
}

/// `try_transition_from(Running, Blocked)` rejects a task in Ready state
/// and succeeds from Running state. The wait-queue protocol relies on
/// this asymmetry to detect a wake that already won the race (Ready)
/// and skip the CAS that would otherwise put the (now-runnable) task
/// to sleep.
pub fn test_try_transition_from_rejects_wrong_state() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr);

    // Set up: task is Ready (a wake has already transitioned us off Running).
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);

    // CAS(Running, Blocked) must fail when state is Ready.
    let result = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_test!(
        result != 0,
        "try_transition_from(Running, Blocked) should fail from Ready"
    );

    // Move to Running and verify CAS succeeds from there.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    let result2 = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(
        result2,
        0,
        "try_transition_from(Running, Blocked) succeeds from Running"
    );
    let state2 = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state2, TaskStatus::Blocked, "state should be Blocked");

    task_terminate(task_id);
    TestResult::Pass
}

/// Exercises [`TaskState`]'s fused (status, reason, epoch) atomic
/// directly. Covers:
/// - Successful `try_transition` flips status+reason and advances
///   the epoch in one CAS.
/// - `try_transition` from the wrong expected state fails and returns
///   the current view.
/// - `bump_epoch` advances the epoch while preserving status/reason.
/// - `force_set` round-trips through `snapshot` for every defined
///   `(TaskStatus, BlockReason)` pair, covering the bit-field maxima
///   on both axes.
/// - 16 consecutive `bump_epoch` calls advance the epoch by 16 (mod
///   2^32), exercising the wrapping arithmetic that backs the wrap
///   from `u32::MAX` to `0`.
pub fn test_task_state_fused_cas() -> TestResult {
    use slopos_sched::task_state::TaskState;

    let s = TaskState::invalid();
    s.force_set(TaskStatus::Running, BlockReason::None);
    let before = s.snapshot();
    assert_eq_test!(before.status, TaskStatus::Running);
    assert_eq_test!(before.reason, BlockReason::None);

    // Successful transition: status + reason flip in one CAS.
    let r = s.try_transition(
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::FutexWait,
    );
    let after = match r {
        Ok(view) => view,
        Err(_) => return TestResult::Fail,
    };
    assert_eq_test!(after.status, TaskStatus::Blocked);
    assert_eq_test!(after.reason, BlockReason::FutexWait);
    assert_test!(after.epoch != before.epoch, "epoch must advance");

    // CAS from wrong expected fails and returns the current view.
    let err = s
        .try_transition(TaskStatus::Running, TaskStatus::Ready, BlockReason::None)
        .expect_err("wrong-expected CAS must fail");
    assert_eq_test!(err.status, TaskStatus::Blocked, "view returned on Err");

    // bump_epoch preserves status + reason while advancing the epoch.
    let pre_bump = s.snapshot();
    s.bump_epoch();
    let post_bump = s.snapshot();
    assert_eq_test!(post_bump.status, pre_bump.status, "bump preserves status");
    assert_eq_test!(post_bump.reason, pre_bump.reason, "bump preserves reason");
    assert_test!(post_bump.epoch != pre_bump.epoch, "bump advances epoch");

    // Pack/unpack roundtrip across every defined (status, reason)
    // pair, including the highest-numbered variant on each axis
    // (TaskStatus::Terminated = 4, BlockReason::FutexWait = 8).
    let statuses = [
        TaskStatus::Invalid,
        TaskStatus::Ready,
        TaskStatus::Running,
        TaskStatus::Blocked,
        TaskStatus::Terminated,
    ];
    let reasons = [
        BlockReason::None,
        BlockReason::Sleep,
        BlockReason::IoWait,
        BlockReason::MutexWait,
        BlockReason::KeyboardWait,
        BlockReason::IpcWait,
        BlockReason::Generic,
        BlockReason::FutexWait,
    ];
    for st in statuses {
        for rn in reasons {
            s.force_set(st, rn);
            let v = s.snapshot();
            assert_eq_test!(v.status, st, "status roundtrip");
            assert_eq_test!(v.reason, rn, "reason roundtrip");
        }
    }

    // 16 consecutive bumps advance the epoch by exactly 16 (mod 2^32).
    // The wrap from u32::MAX → 0 uses the same `wrapping_add(1)` as
    // every other increment, so demonstrating mod-2^32 arithmetic
    // here transitively validates the wrap without needing 2^32
    // iterations.
    s.force_set(TaskStatus::Ready, BlockReason::None);
    let e0 = s.snapshot().epoch;
    for _ in 0..16 {
        s.bump_epoch();
    }
    let e1 = s.snapshot().epoch;
    assert_eq_test!(
        e1.wrapping_sub(e0),
        16,
        "16 bumps must advance epoch by 16 (mod 2^32)"
    );
    assert_eq_test!(
        s.snapshot().status,
        TaskStatus::Ready,
        "status preserved across bumps"
    );

    TestResult::Pass
}

/// E2E: call syscall_poll on a unix socket, write data, verify poll returns.
///
/// Exercises the full kernel poll path: WQ registration, readiness
/// check, block_current_task_with_timeout, and wakeup via unix_send.
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
    let pid = task_process_id(task_ptr).unwrap_or(0);

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
        crate::syscall::dispatch::dispatch_handler(syscall_poll, task_ptr, &mut frame)
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
        crate::syscall::dispatch::dispatch_handler(syscall_poll, task_ptr, &mut frame2)
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
        crate::syscall::dispatch::dispatch_handler(syscall_poll, task_ptr, &mut frame3)
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
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

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
    let cli_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(cli_handle)
        .expect("cli backing alloc");
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
        Some(cli_backing),
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

    let srv_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(accepted_handle)
        .expect("srv backing alloc");
    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
        Some(srv_backing),
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

/// Wakeup through a unix socket write reaches the wait queue and
/// CAS-flips the registered task `Blocked → Ready`.
///
/// Exercises the post-Phase-5 wait protocol end-to-end:
/// 1. Task starts Running.
/// 2. Caller registers on the unix socket's recv wait queue.
/// 3. Caller commits to Blocked under the wait queue's SpinLock
///    (modelled here by an explicit CAS — the real wait_event closes
///    the same window).
/// 4. Producer writes data; the producer's unix_send invokes
///    `wake_all` on RECV_WQS, which CAS-flips the registered task to
///    Ready via `unblock_task`.
pub fn test_unix_send_wakes_blocked_poll_waiter() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Make the FD-owning user task current.  dispatch() sets it
    // Running, which is the precondition for the wait-queue
    // protocol's Running → Blocked CAS.
    make_task_current(task_ptr);

    // Step 1: register on the recv WQ FIRST (Linux sock_poll_wait order).
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "STEP1: register");
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "STEP1: no data before write");

    // Step 2: commit Blocked under the wait queue's SpinLock (modelled
    // here by an explicit CAS — wait_event's lock-held push + CAS
    // closes the same race window).
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "STEP2: Running → Blocked");

    // Step 3: producer writes data → wake_all → unblock_task CAS.
    let payload = b"wake-test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "STEP3: write");

    // Step 4: the registered task must now be Ready.
    let state_after = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state_after, TaskStatus::Ready, "STEP4: Blocked → Ready");

    let revents_after = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents_after & POLLIN) != 0, "STEP4: POLLIN");

    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

/// Demonstrates the check-first-register-second race (the OLD broken pattern).
///
/// Manually simulates the sequence that `unix_poll_fused` used to execute:
/// 1. Check readiness (empty).
/// 2. Data arrives + `wake_all` fires — nobody on the queue yet.
/// 3. Register on the queue — too late, wakeup already fired.
///
/// The committed-Blocked task stays `Blocked` because `wake_all` found
/// no waiters on the queue. This is the lost-wakeup signature that
/// the register-first ordering (post-Phase-5 wait_event) exists to
/// prevent.
pub fn test_poll_fused_gap_demonstrates_race() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Task identity matters: enqueue_current operates on PCR.current_task.
    make_task_current(task_ptr);

    // Step 1: commit Blocked WITHOUT first registering on the queue
    // (the buggy pre-fix ordering).
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "STEP1: Running → Blocked");

    // Step 2: check readiness without registering.
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "no data yet");

    // Step 3: data arrives — wake_all fires with nobody on the queue!
    let payload = b"race-demo";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    // Step 4: NOW register (too late — wake already fired against an
    // empty queue, no `unblock_task` ran).
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);

    // Task must STILL be Blocked — the lost-wakeup signature.
    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Blocked, "wakeup lost — still Blocked");

    // Cleanup: undo the manual Blocked transition before terminate.
    let _ = task_set_state(task_id, TaskStatus::Ready);
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

/// Proves the register-first-then-check order preserves wakeups.
///
/// Simulates the FIXED pattern (Linux sock_poll_wait order, post-
/// Phase-5 wait_event):
/// 1. Register on the queue FIRST.
/// 2. Commit Blocked under the wait queue's SpinLock.
/// 3. Data arrives + wake_all fires — finds the task on the queue
///    and CAS-flips Blocked → Ready.
/// 4. Caller's loop re-checks the condition and observes the data.
pub fn test_poll_fused_register_first_catches_wakeup() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    make_task_current(task_ptr);

    // Step 1: register FIRST.
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "register");

    // Step 2: commit Blocked under the queue lock (modelled here as a
    // direct CAS).
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "Running → Blocked");

    // Step 3: data arrives — wake_all finds us on the queue.
    let payload = b"race-fix";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    // Step 4: wakeup preserved — Blocked → Ready.
    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Ready, "wakeup preserved — Ready");

    // Step 5: readiness check sees the data.
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) != 0, "POLLIN");

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
slopos_testing::stest!(name = test_task_state_fused_cas, suite = poll_wakeup_race);
slopos_testing::stest!(
    name = test_unix_socket_poll_syscall_e2e,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_compositor_handshake_listen_accept_send_poll,
    suite = poll_wakeup_race
);
slopos_testing::stest!(
    name = test_unix_send_wakes_blocked_poll_waiter,
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

// =============================================================================
// AF_UNIX SCM_RIGHTS atomicity tests
// =============================================================================
//
// `unix_sendmsg` must publish data bytes and ancillary fds together:
// the peer's `unix_recvmsg` either sees both or neither. Without the
// single-critical-section publish, the preempt-on-enqueue scheduler
// (Phase 1.2) makes the peer drain the data FIFO before the sender
// commits the fds — surfacing as a Wayland-style decoder receiving
// `SurfaceAttach { buffer_fd: None }`.

/// Build a connected AF_UNIX pair and return the raw socket handles
/// (not the fd-table-installed `i32`s). Tests that work below the fd
/// layer use this so they can call `unix_sendmsg`/`unix_recvmsg`
/// directly.
fn unix_create_connected_pair_raw() -> Option<(
    slopos_net::unix_socket::SocketHandle,
    slopos_net::unix_socket::SocketHandle,
)> {
    use slopos_net::unix_socket;

    let path = b"/scm-rights/sock";

    let srv = unix_socket::unix_create()?;
    if unix_socket::unix_bind(srv, path) != 0 {
        unix_socket::unix_close(srv);
        return None;
    }
    if unix_socket::unix_listen(srv, 4) != 0 {
        unix_socket::unix_close(srv);
        return None;
    }
    unix_socket::unix_set_nonblocking(srv, true);

    let cli = unix_socket::unix_create()?;
    if unix_socket::unix_connect(cli, path) != 0 {
        unix_socket::unix_close(cli);
        unix_socket::unix_close(srv);
        return None;
    }

    let accepted = match unix_socket::unix_accept(srv) {
        Ok(h) => h,
        Err(_) => {
            unix_socket::unix_close(cli);
            unix_socket::unix_close(srv);
            return None;
        }
    };
    unix_socket::unix_close(srv);
    // Both halves of the pair are non-blocking so tests that probe
    // empty FIFOs return EAGAIN instead of parking the test
    // thread on the per-slot wait queue (no scheduler context).
    unix_socket::unix_set_nonblocking(accepted, true);
    unix_socket::unix_set_nonblocking(cli, true);
    Some((accepted, cli))
}

/// SCM_RIGHTS atomicity: one `unix_sendmsg` with both data and an fd
/// must deliver both to the peer's next `unix_recvmsg` — never just
/// the data with the fd trailing in a separate ancillary state. This
/// is the regression that caused `SurfaceAttach` decode to see
/// `buffer_fd: None` and windows to not render.
pub fn test_unix_scm_rights_atomic_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_net::unix_socket;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    // A memfd fd in the sender's table; the in-flight alias is a FileRef
    // minted from it.
    let (mfd_handle, mfd_ops, mfd_backing) = match slopos_mm::memfd::memfd_create(0) {
        Some(h) => h,
        None => {
            unix_socket::unix_close(srv);
            unix_socket::unix_close(cli);
            return fail!("memfd_create failed");
        }
    };
    let mfd_fd =
        slopos_fs::fileio::fileio_open_fd_with_ops(pid, mfd_ops, mfd_handle, Some(mfd_backing));
    assert_test!(mfd_fd >= 0, "memfd fd install failed");

    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);

    let payload = b"ATOM";
    let n = unix_socket::unix_sendmsg(srv, payload, &mut files);
    assert_test!(
        n == payload.len() as i32,
        "unix_sendmsg returned {} (expected {})",
        n,
        payload.len()
    );
    assert_test!(
        files.is_empty(),
        "unix_sendmsg must move the alias into the queue on success"
    );

    let mut buf = [0u8; 16];
    let mut out: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("out vec alloc");
    let (bytes_read, n_fds) = unix_socket::unix_recvmsg(cli, &mut buf, &mut out, 1);

    assert_eq_test!(bytes_read, payload.len() as i32, "recvmsg byte count");
    assert_eq_test!(n_fds, 1, "recvmsg must deliver the companion fd");
    assert_test!(&buf[..payload.len()] == payload, "recvmsg payload mismatch");

    // The delivered alias shares the sender's open-file description:
    // install it and compare the resolved backing handle.
    let recv_fd = slopos_fs::fileio_install_file_ref(pid, out.pop().expect("delivered file"));
    assert_test!(recv_fd >= 0, "install of delivered file failed");
    let (kind, handle) =
        slopos_fs::fileio::fileio_get_open_file_handle(pid, recv_fd).expect("resolve recv fd");
    assert_test!(
        kind == slopos_abi::file_ops::FileKind::Memfd && handle == mfd_handle,
        "delivered fd must reference the sender's memfd (kind {:?}, handle {})",
        kind,
        handle
    );

    let _ = file_close_fd(pid, recv_fd);
    let _ = file_close_fd(pid, mfd_fd);
    unix_socket::unix_close(srv);
    unix_socket::unix_close(cli);
    task_terminate(task_id);
    pass!()
}

/// Ancillary queue overflow: a `sendmsg` whose fd count would push
/// the per-direction anc queue past `MAX_INFLIGHT_FDS` must reject
/// with ENOMEM, leaving the aliases with the caller — no
/// partial-publish to the peer.
pub fn test_unix_scm_rights_anc_queue_full_no_partial() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_net::unix_socket;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    // One memfd fd; every queued alias clones from it.
    let (mfd_handle, mfd_ops, mfd_backing) = match slopos_mm::memfd::memfd_create(0) {
        Some(p) => p,
        None => return fail!("memfd_create failed"),
    };
    let mfd_fd =
        slopos_fs::fileio::fileio_open_fd_with_ops(pid, mfd_ops, mfd_handle, Some(mfd_backing));
    assert_test!(mfd_fd >= 0, "memfd fd install failed");

    // Fill the anc queue up to capacity.
    const CAP: usize = 8;
    for _ in 0..CAP {
        let mut one: slopos_ostd::KVec<slopos_fs::FileRef> =
            slopos_ostd::KVec::with_capacity(1).expect("fill vec alloc");
        let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("fill clone failed");
        let _ = one.push(alias);
        let n = unix_socket::unix_sendmsg(srv, &[], &mut one);
        assert_test!(n >= 0, "fill push returned {}", n);
    }

    // Capacity is now at the cap. One more fd → ENOMEM, alias returned.
    let mut overflow: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("overflow vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("overflow clone failed");
    let _ = overflow.push(alias);
    let rc = unix_socket::unix_sendmsg(srv, b"X", &mut overflow);
    assert_test!(rc == -12, "expected ENOMEM (-12), got {}", rc);
    assert_eq_test!(
        overflow.len(),
        1,
        "rejected send must leave the alias with the caller"
    );
    drop(overflow);

    // Peer should see exactly the 8 originally-sent fds, no overflow.
    // The overflow `b"X"` must NOT be in the data FIFO since the
    // sendmsg returned ENOMEM before committing anything.
    //
    // Drain anc with an empty data slice (skips `unix_recv` so we
    // don't trip EAGAIN from the empty non-blocking FIFO). Then
    // explicitly probe the data FIFO with a small read.
    let mut out: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(16).expect("out vec alloc");
    let (anc_drain_bytes, n_fds) = unix_socket::unix_recvmsg(cli, &mut [], &mut out, 16);
    assert_eq_test!(n_fds, CAP, "peer must see all 8 fds, no overflow");
    assert_eq_test!(anc_drain_bytes, 0, "anc-drain returns zero data");

    let mut probe_buf = [0u8; 4];
    let probe = unix_socket::unix_recv(cli, &mut probe_buf);
    assert_test!(
        probe == 0 || probe == -11,
        "data FIFO must be empty (got {}); overflow 'X' leaked",
        probe
    );

    // Dropping the drained aliases closes them.
    drop(out);
    let _ = file_close_fd(pid, mfd_fd);
    unix_socket::unix_close(srv);
    unix_socket::unix_close(cli);
    task_terminate(task_id);
    pass!()
}

/// A failed send must not strand the passed file: the alias stays with
/// the caller (whose drop closes it), and the sender's own fd keeps the
/// file alive — no leak, no double teardown.
pub fn test_unix_scm_rights_error_returns_custody() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_net::unix_socket;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let pid = task_process_id(task_find_by_id(task_id)).unwrap_or(0);

    // Build a pair, close the peer immediately → next send sees
    // EPIPE because peer_closed is set.
    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };
    unix_socket::unix_close(cli);

    let (mfd_handle, mfd_ops, mfd_backing) = match slopos_mm::memfd::memfd_create(0) {
        Some(p) => p,
        None => return fail!("memfd_create failed"),
    };
    let mfd_fd =
        slopos_fs::fileio::fileio_open_fd_with_ops(pid, mfd_ops, mfd_handle, Some(mfd_backing));
    assert_test!(mfd_fd >= 0, "memfd fd install failed");
    assert_test!(
        slopos_mm::memfd::memfd_ftruncate(mfd_handle, 4096) == 0,
        "ftruncate failed"
    );

    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);
    let rc = unix_socket::unix_sendmsg(srv, b"DEAD", &mut files);
    assert_test!(rc == -32, "expected EPIPE (-32), got {}", rc);
    assert_eq_test!(
        files.len(),
        1,
        "failed sendmsg must leave the alias with the caller"
    );

    // Dropping the returned alias closes it; the sender's fd still pins
    // the memfd — only the final close tears it down.
    drop(files);
    assert_test!(
        slopos_mm::memfd::memfd_size(mfd_handle) >= 4096,
        "memfd must survive the dropped alias while its fd is open"
    );
    let _ = file_close_fd(pid, mfd_fd);
    assert_test!(
        slopos_mm::memfd::memfd_size(mfd_handle) == 0,
        "memfd must be torn down after the last reference closes"
    );

    unix_socket::unix_close(srv);
    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_unix_scm_rights_atomic_delivery,
    suite = unix_scm_rights
);
slopos_testing::stest!(
    name = test_unix_scm_rights_anc_queue_full_no_partial,
    suite = unix_scm_rights
);
slopos_testing::stest!(
    name = test_unix_scm_rights_error_returns_custody,
    suite = unix_scm_rights
);

// =============================================================================
// Signal default-disposition and send-time drop tests
// =============================================================================

/// The default-action table: informational signals are ignored, job-control
/// signals stop/continue, everything else terminates. Regression guard for
/// the terminal-resize crash: SIGWINCH defaulting to Terminate killed the
/// shell on every TIOCSWINSZ.
pub fn test_sig_default_action_table() -> TestResult {
    assert_test!(
        matches!(sig_default_action(SIGWINCH), SigDefault::Ignore),
        "SIGWINCH default must be Ignore"
    );
    assert_test!(
        matches!(sig_default_action(SIGCHLD), SigDefault::Ignore),
        "SIGCHLD default must be Ignore"
    );
    assert_test!(
        matches!(sig_default_action(SIGCONT), SigDefault::Continue),
        "SIGCONT default must be Continue"
    );
    for sig in [SIGSTOP, SIGTSTP, SIGTTIN, SIGTTOU] {
        assert_test!(
            matches!(sig_default_action(sig), SigDefault::Stop),
            "job-control signal {} default must be Stop",
            sig
        );
    }
    for sig in [SIGHUP, SIGINT, SIGTERM, SIGUSR1] {
        assert_test!(
            matches!(sig_default_action(sig), SigDefault::Terminate),
            "signal {} default must be Terminate",
            sig
        );
    }
    // Send-time droppability is strictly the Ignore class: Stop/Continue
    // stay deliverable so implementing real job control later does not
    // require revisiting raise sites.
    assert_test!(
        sig_default_ignores(SIGWINCH),
        "SIGWINCH must be send-time droppable"
    );
    assert_test!(
        !sig_default_ignores(SIGTERM),
        "SIGTERM must not be send-time droppable"
    );
    assert_test!(
        !sig_default_ignores(SIGTSTP),
        "Stop-class must not be send-time droppable"
    );
    assert_test!(
        !sig_default_ignores(SIGCONT),
        "Continue-class must not be send-time droppable"
    );
    pass!()
}

/// `task_signal_post` drops unblocked signals whose disposition discards
/// them, and pends everything else.
pub fn test_signal_post_disposition_gate() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");

    // Fresh task: every action is SIG_DFL. A default-ignored SIGWINCH is
    // dropped at the send site — no pending bit, no wake requested.
    assert_test!(
        !task::task_signal_post(task_ptr, SIGWINCH),
        "default-ignored SIGWINCH must be dropped at send"
    );
    assert_eq_test!(
        task_signal_pending(task_ptr),
        0,
        "dropped signal must not pend"
    );

    // A default-terminate signal pends and requests a wake.
    assert_test!(
        task::task_signal_post(task_ptr, SIGTERM),
        "SIGTERM must pend"
    );
    assert_eq_test!(
        task_signal_pending(task_ptr),
        sig_bit(SIGTERM),
        "SIGTERM pending bit expected"
    );
    task::task_signal_pending_store(task_ptr, 0);

    // A blocked signal pends regardless of disposition: a signalfd reader
    // or a later-installed handler may drain it after unblocking.
    if let Some(t) = task::task_borrow_mut(task_ptr) {
        t.signal_blocked = sig_bit(SIGWINCH);
    }
    assert_test!(
        task::task_signal_post(task_ptr, SIGWINCH),
        "blocked SIGWINCH must pend"
    );
    assert_eq_test!(
        task_signal_pending(task_ptr),
        sig_bit(SIGWINCH),
        "blocked SIGWINCH pending bit expected"
    );
    task::task_signal_pending_store(task_ptr, 0);
    if let Some(t) = task::task_borrow_mut(task_ptr) {
        t.signal_blocked = 0;
    }

    // A real handler overrides the default-ignore drop.
    if let Some(t) = task::task_borrow_mut(task_ptr) {
        t.signal_actions[(SIGWINCH - 1) as usize] = SignalAction {
            handler: 0x4100_0000,
            mask: 0,
            flags: 0,
            restorer: 0x4200_0000,
        };
    }
    assert_test!(
        task::task_signal_post(task_ptr, SIGWINCH),
        "handled SIGWINCH must pend"
    );
    task::task_signal_pending_store(task_ptr, 0);

    // SIG_IGN drops even a default-terminate signal.
    if let Some(t) = task::task_borrow_mut(task_ptr) {
        t.signal_actions[(SIGTERM - 1) as usize] = SignalAction {
            handler: SIG_IGN,
            mask: 0,
            flags: 0,
            restorer: 0,
        };
    }
    assert_test!(
        !task::task_signal_post(task_ptr, SIGTERM),
        "SIG_IGN SIGTERM must be dropped at send"
    );
    assert_eq_test!(
        task_signal_pending(task_ptr),
        0,
        "ignored SIGTERM must not pend"
    );

    task_terminate(task_id);
    pass!()
}

/// End-to-end regression for the resize crash: kill(SIGWINCH) against a
/// default-disposition task succeeds per POSIX, and the target survives —
/// both via the send-time drop and (for a directly-pended bit) via the
/// delivery-point discard.
pub fn test_kill_default_ignored_sigwinch_target_survives() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_ptr = task_find_by_id(task_id);
    assert_not_null!(task_ptr, "task lookup failed");
    let pid = task_process_id(task_ptr).unwrap_or(0);

    // kill() reports success even though the disposition discards the
    // signal at the send site.
    let mut kill_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGWINCH as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, task_ptr, &mut *kill_frame)
    });
    assert_eq_test!(kill_frame.regs().rax, 0, "kill(SIGWINCH) must succeed");
    assert_eq_test!(
        task_signal_pending(task_ptr),
        0,
        "SIGWINCH must be dropped at the send site"
    );

    // Belt-and-braces: a SIGWINCH bit pended directly (bypassing the send
    // gate) must be discarded at the delivery point, never terminate.
    let _ = task::task_signal_raise(task_ptr, sig_bit(SIGWINCH));
    let original_rip = 0x5000_4321u64;
    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = original_rip;
    let _ = with_user_process_context(pid, || {
        deliver_pending_signal(task_ptr, &mut *user_frame as *mut UserContext)
    });
    assert_eq_test!(
        user_frame.regs().rip,
        original_rip,
        "ignored delivery must not redirect RIP"
    );
    assert_eq_test!(
        task_signal_pending(task_ptr),
        0,
        "delivery must consume the ignored signal"
    );
    let state = task_status(task_ptr).unwrap_or(TaskStatus::Terminated);
    assert_test!(
        state != TaskStatus::Zombie && state != TaskStatus::Terminated,
        "SIGWINCH must not terminate the target"
    );

    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_sig_default_action_table,
    suite = signal_dispositions
);
slopos_testing::stest!(
    name = test_signal_post_disposition_gate,
    suite = signal_dispositions
);
slopos_testing::stest!(
    name = test_kill_default_ignored_sigwinch_target_survives,
    suite = signal_dispositions
);
