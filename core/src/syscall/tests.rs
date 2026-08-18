//! Syscall validation tests: invalid/null pointer handling, boundary
//! conditions, permission checks, resource exhaustion, and dispatch edge cases.

use core::ffi::c_char;
use core::ptr;
use slopos_fs::fileio::FdTable;

use crate::syscall::fs::syscall_ioctl;
use crate::syscall::handlers::{
    syscall_arch_prctl, syscall_futex, syscall_getpgid, syscall_setpgid, syscall_setsid,
    syscall_user_read, syscall_waitpid,
};
use crate::syscall::keymap_handlers::syscall_keymap_load;
use crate::syscall::signal::{
    deliver_pending_signal, deliver_pending_signal_on_irq_exit, syscall_kill, syscall_rt_sigaction,
    syscall_rt_sigprocmask, syscall_rt_sigreturn,
};
use slopos_abi::addr::PhysAddr;
use slopos_abi::fs::O_RDONLY;
use slopos_abi::signal::{
    NSIG, SA_NODEFER, SA_RESTART, SIG_DFL, SIG_IGN, SIG_SETMASK, SIG_UNBLOCK, SIGCHLD, SIGCONT,
    SIGHUP, SIGINT, SIGKILL, SIGSTOP, SIGTERM, SIGTSTP, SIGTTIN, SIGTTOU, SIGUSR1, SIGUSR2,
    SIGWINCH, SigDefault, SigSet, SignalFrame, UserSigaction, sig_bit, sig_default_action,
    sig_default_ignores,
};
use slopos_abi::syscall::{
    ARCH_GET_FS, ARCH_SET_FS, CLONE_SETTLS, CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, ERRNO_EAGAIN,
    F_GETFL, F_SETFD, FD_CLOEXEC, FUTEX_WAIT, FUTEX_WAKE, MAP_ANONYMOUS, MAP_PRIVATE, O_NOCTTY,
    O_NONBLOCK, POLLIN, SYSCALL_ARCH_PRCTL, SYSCALL_CLONE, SYSCALL_FUTEX, SYSCALL_GETPGID,
    SYSCALL_IOCTL, SYSCALL_KILL, SYSCALL_PIPE, SYSCALL_PIPE2, SYSCALL_POLL, SYSCALL_RT_SIGACTION,
    SYSCALL_RT_SIGPROCMASK, SYSCALL_RT_SIGRETURN, SYSCALL_SELECT, SYSCALL_SETPGID, SYSCALL_SETSID,
    SYSCALL_TABLE_SIZE, SYSCALL_VHANGUP, TIOCSCTTY, TtyIndex,
};
use slopos_abi::task::{
    INVALID_TASK_ID, TASK_FLAG_COMPOSITOR, TASK_FLAG_CONSOLE_ADMIN, TASK_FLAG_KERNEL_MODE,
    TASK_FLAG_NET_ADMIN, TASK_FLAG_SYSTEM, TASK_FLAG_USER_MODE, TaskStatus,
};
use slopos_mm::page_alloc::{alloc_kernel_page, free_page_frame};
use slopos_mm::paging_defs::PageFlags;
use slopos_mm::process_vm::{process_vm_alloc, process_vm_get_stack_top};
use slopos_mm::user_copy::{copy_from_user, copy_to_user, set_test_process_id};
use slopos_mm::user_ptr::UserPtr;
use slopos_ostd::task::SchedPlacement;
use slopos_ostd::task::{new_group_in_session, new_session_group};
use slopos_ostd::user::context::UserContext;
use slopos_ostd::{KArc, KBox, klog_info};
use slopos_sched::task_struct::{Current, SignalAction};
use slopos_testing::{TestResult, assert_eq_test, assert_some, assert_test, fail, pass};

use crate::exec::{FdAction, apply_fd_actions};
use crate::syscall::handlers::syscall_lookup;
use slopos_abi::io::{KernelIoBuf, KernelIoBufRef};
use slopos_abi::task::BlockReason;
use slopos_fs::fileio::{
    file_close_fd, file_dup_fd, file_dup3_fd, file_fcntl_fd, file_open_for_process,
    file_open_tty_fd, file_pipe_create, file_poll_fd, file_read_fd, file_seek_fd, file_write_fd,
    fileio_clone_table_for_process, fileio_close_on_exec, fileio_create_empty_table_for_process,
    fileio_destroy_table_for_process, fileio_get_open_file_handle,
};
use slopos_mm::memory_layout_defs::PROCESS_CODE_START_VA;
use slopos_sched::scheduler::unblock_task;
use slopos_sched::task;
use slopos_sched::task::{
    task_clone, task_create, task_find_by_id, task_fork, task_set_state,
    task_set_state_from_with_reason, task_terminate, task_try_transition_from,
};

/// Hermetic scope: its registry walk restores every piece of cross-test state a
/// syscall test can disturb, so a test cannot leak by forgetting a teardown.
type SyscallFixture = slopos_sched::test_fixture::KernelTestScope;

/// Park PCR's `current_task` on the BSP bootstrap stub, for tests that mutate
/// the running-task pointer. `BspCurrentTask` restores it on scope drop.
fn park_bootstrap_on_current_cpu() {
    slopos_arch::pcr::park_bootstrap_task(
        slopos_ostd::task::bootstrap::BSP_BOOTSTRAP_TASK.get() as *mut ()
    );
}

fn make_task_current(task_id: u32) {
    let task = task_find_by_id(task_id).expect("make_task_current: no such task");
    if task.status() == TaskStatus::Blocked {
        assert_eq!(task_set_state(task_id, TaskStatus::Ready), 0);
    }
    task.set_sched_placement(SchedPlacement::OnCpu);
    let cpu_id = slopos_arch::pcr::get_current_cpu();
    assert!(
        slopos_sched::scheduler::dispatch_task_for_test(cpu_id, task_id),
        "make_task_current: task vanished before dispatch"
    );
}

/// Deliver a pending signal the way production does: on the task's own CPU with
/// the `Current` witness. Restores the bootstrap current-task afterwards so a
/// later `task_terminate` does not take the is-current Zombie path.
fn deliver_pending_signal_as_current(task_id: u32, table: FdTable, ctx: &UserContext) {
    make_task_current(task_id);
    {
        let current = Current::get().expect("current task after dispatch");
        let _ = with_user_process_context(table, || deliver_pending_signal(&current, ctx));
    }
    park_bootstrap_on_current_cpu();
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
    create_test_user_task_with(TASK_FLAG_USER_MODE)
}

fn create_test_user_task_with(flags: u16) -> u32 {
    let user_entry = slopos_sched::task::task_entry_from_kernel_va(PROCESS_CODE_START_VA as u64);
    task_create(
        b"UserTest\0".as_ptr() as *const c_char,
        user_entry,
        ptr::null_mut(),
        1,
        flags,
    )
}

/// Zero-initialised `UserContext`; tests load syscall arg registers through
/// `regs_mut()` before dispatching.
fn zero_frame() -> UserContext {
    UserContext::const_zeroed()
}

/// A zeroed syscall frame on the heap. `UserContext` is 192 bytes with its own
/// unmerged slot per local, so tests holding several take them off the stack.
fn zero_frame_boxed() -> KBox<UserContext> {
    KBox::zeroed().expect("frame alloc")
}

fn pts_path_for(number: u32) -> Option<[u8; 11]> {
    if number > 9 {
        return None;
    }
    let mut path = *b"/dev/pts/0\0";
    path[9] = b'0' + number as u8;
    Some(path)
}

/// Resolve a pid this test just created into the designator `mm` takes.
fn resolve_pid(pid: u32) -> slopos_ostd::process::ProcessId {
    slopos_ostd::process::ProcessId::resolve(pid).expect("a pid this test just created")
}

fn with_user_process_context<R>(table: FdTable, f: impl FnOnce() -> R) -> Option<R> {
    let process = table.process()?;
    if slopos_mm::process_vm::process_vm_get_ostd_pml4_paddr(process) == 0 {
        return None;
    }
    if !slopos_mm::process_vm::process_vm_activate(process) {
        return None;
    }
    // The PCR still carries a bare pid across the syscall boundary, so the
    // designator becomes a number again exactly here.
    set_test_process_id(table.id());
    let out = f();
    set_test_process_id(slopos_abi::task::INVALID_PROCESS_ID);
    slopos_kernel_services::kernel_vm_space::kernel_vm_space()
        .lock()
        .activate_kernel_master();
    Some(out)
}

fn user_copy_out<T: Copy>(table: FdTable, addr: u64, value: &T) -> bool {
    with_user_process_context(table, || {
        let ptr = match UserPtr::<T>::try_new(addr) {
            Ok(p_guard) => p_guard,
            Err(_) => return false,
        };
        copy_to_user(ptr, value).is_ok()
    })
    .unwrap_or(false)
}

fn user_copy_in<T: Copy>(table: FdTable, addr: u64) -> Option<T> {
    with_user_process_context(table, || {
        let ptr = UserPtr::<T>::try_new(addr).ok()?;
        copy_from_user(ptr).ok()
    })?
}

fn map_user_rw_page(table: FdTable) -> Option<u64> {
    let process = table.process()?;
    let base = process_vm_alloc(process, 4096, PageFlags::USER_RW.bits() as u32);
    if base == 0 {
        return None;
    }

    let phys: PhysAddr = alloc_kernel_page();
    if phys.is_null() {
        return None;
    }

    let mapped = slopos_mm::process_vm::process_vm_with_vm_space(process, |vs| {
        slopos_mm::user_mappings::ostd_map_4kb_user(
            vs,
            slopos_abi::addr::VirtAddr::new(base),
            phys,
            PageFlags::USER_RW.bits(),
        )
        .is_ok()
    });
    if !matches!(mapped, Some(true)) {
        free_page_frame(phys);
        return None;
    }

    Some(base)
}

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
    // Retired index-addressed TTY calls; TTY access is fd-only. Guards the
    // numbers against re-registration.
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

/// 120 and 123 are retired: reusing either hands a binary built against them a
/// different call under a number it already knows. Literals — constants are gone.
pub fn test_retired_net_syscalls_stay_unhandled() -> TestResult {
    for number in [120u64, 123] {
        if let Some(entry) = syscall_lookup(number)
            && entry.handler.is_some()
        {
            klog_info!("retired syscall {} has a handler again", number);
            return TestResult::Fail;
        }
    }
    TestResult::Pass
}

pub fn test_pipe_poll_eof_baseline() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
    let parent_guard = assert_some!(task_find_by_id(parent_id));

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id));

    let mut frame = zero_frame();
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_getpgid, &parent_guard, &mut frame);
    assert_eq_test!(
        frame.rax() as u32,
        parent_guard.pgid(),
        "getpgid self mismatch"
    );

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = child_id as u64;
    setpgid_frame.regs_mut().rsi = parent_id as u64;
    let _ = crate::syscall::dispatch::dispatch_handler(
        syscall_setpgid,
        &parent_guard,
        &mut setpgid_frame,
    );
    assert_eq_test!(setpgid_frame.rax(), 0, "setpgid should succeed for child");
    assert_eq_test!(
        child_guard.pgid(),
        parent_id,
        "child pgid mismatch after setpgid"
    );

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, &child_guard, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.rax() as u32,
        child_id,
        "setsid should return child sid"
    );
    assert_eq_test!(
        child_guard.sid(),
        child_id,
        "child sid mismatch after setsid"
    );
    assert_eq_test!(
        child_guard.pgid(),
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
    let leader_guard = assert_some!(task_find_by_id(leader_id), "leader lookup failed");

    let member_id = task_fork(&leader_guard, None);
    assert_test!(member_id != INVALID_TASK_ID, "failed to fork member task");
    task_set_state(member_id, TaskStatus::Blocked);
    let member_guard = assert_some!(task_find_by_id(member_id), "member lookup failed");

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = member_id as u64;
    setpgid_frame.regs_mut().rsi = leader_id as u64;
    let _ = crate::syscall::dispatch::dispatch_handler(
        syscall_setpgid,
        &leader_guard,
        &mut setpgid_frame,
    );
    assert_eq_test!(setpgid_frame.rax(), 0, "setpgid should succeed for member");

    let Some(leader_pid) = leader_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(member_pid) = member_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut probe_frame = zero_frame();
    probe_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    probe_frame.regs_mut().rsi = 0;
    let _ = with_user_process_context(leader_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &leader_guard, &mut probe_frame)
    });
    assert_eq_test!(probe_frame.rax(), 0, "kill(group, 0) probe should succeed");

    leader_guard.set_signal_pending(0);
    member_guard.set_signal_pending(0);

    let mut negative_group_frame = zero_frame();
    negative_group_frame.regs_mut().rdi = (-(leader_id as i32) as i64) as u64;
    negative_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(leader_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_kill,
            &leader_guard,
            &mut negative_group_frame,
        )
    });
    assert_eq_test!(negative_group_frame.rax(), 0, "kill(-pgid, SIGUSR1) failed");

    let pending_bit = sig_bit(SIGUSR1);
    let leader_pending = leader_guard.signal_pending();
    let member_pending = member_guard.signal_pending();
    assert_test!(
        (leader_pending & pending_bit) != 0,
        "leader did not receive group signal"
    );
    assert_test!(
        (member_pending & pending_bit) != 0,
        "member did not receive group signal"
    );

    leader_guard.set_signal_pending(0);
    member_guard.set_signal_pending(0);

    let mut caller_group_frame = zero_frame();
    caller_group_frame.regs_mut().rdi = 0;
    caller_group_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(member_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_kill,
            &member_guard,
            &mut caller_group_frame,
        )
    });
    assert_eq_test!(caller_group_frame.rax(), 0, "kill(0, SIGUSR1) failed");

    let leader_pending_after = leader_guard.signal_pending();
    let member_pending_after = member_guard.signal_pending();
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
    let parent_guard = assert_some!(task_find_by_id(parent_id));

    let parent_pg = parent_guard
        .process_group
        .load()
        .expect("parent carries a process group");
    assert_eq_test!(parent_pg.id(), parent_id, "leader group id == leader pid");
    assert_eq_test!(
        parent_pg.session_id(),
        parent_id,
        "leader session id == leader pid"
    );

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id));

    let child_pg = child_guard
        .process_group
        .load()
        .expect("child inherits a process group");
    assert_test!(
        KArc::ptr_eq(&parent_pg, &child_pg),
        "fork shares the parent's group object by identity"
    );

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, &child_guard, &mut setsid_frame);
    let child_pg2 = child_guard
        .process_group
        .load()
        .expect("child group after setsid");
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

/// A membership handle read out of a task's group slot stays valid after a
/// *different* task republishes that slot.
///
/// `setpgid` writes the target's group field and `task_process_group` clones it
/// from any CPU, both holding nothing. The slot is what keeps them apart: a read
/// mints inside an RCU read-side section, and the displaced reference is
/// released only after a grace period.
pub fn test_process_group_slot_survives_republication() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID);
    let parent_guard = assert_some!(task_find_by_id(parent_id));

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID);
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id));

    // Counted relative to whatever else holds the group, so the assertions
    // below stay exact without assuming this test is the only holder.
    let held = child_guard
        .process_group
        .load()
        .expect("child inherits a group");
    assert_eq_test!(held.id(), parent_id, "child starts in the parent's group");
    let observer = KArc::downgrade(&held);
    let count_before = KArc::strong_count(&held);

    let mut setpgid_frame = zero_frame();
    setpgid_frame.regs_mut().rdi = child_id as u64;
    setpgid_frame.regs_mut().rsi = child_id as u64;
    let _ = crate::syscall::dispatch::dispatch_handler(
        syscall_setpgid,
        &parent_guard,
        &mut setpgid_frame,
    );
    assert_eq_test!(setpgid_frame.rax(), 0, "setpgid should succeed");

    let replaced = child_guard
        .process_group
        .load()
        .expect("child carries its new group");
    assert_test!(
        !KArc::ptr_eq(&held, &replaced),
        "setpgid published a different group object"
    );
    assert_eq_test!(replaced.id(), child_id, "new group id == child pid");
    assert_eq_test!(
        held.id(),
        parent_id,
        "the displaced group is still readable through the handle"
    );

    // Retire whatever the store deferred, on whichever CPU runs the callback.
    crate::tests::rcu_cb_tests::drain_until(|| KArc::strong_count(&held) == count_before - 1);
    assert_eq_test!(
        KArc::strong_count(&held),
        count_before - 1,
        "the store released the slot's reference exactly once"
    );
    assert_test!(
        observer.upgrade().is_some(),
        "the reader's own reference outlives the republication"
    );

    task_terminate(child_id);
    task_terminate(parent_id);
    TestResult::Pass
}

/// `ProcessGroup -> Session` is a strong DAG: a session outlives every group in
/// it and is freed only when the last group drops.
pub fn test_process_group_session_dag_lifetime() -> TestResult {
    let _fixture = SyscallFixture::new();

    let pg = new_session_group(42).expect("mint session+group");
    assert_eq_test!(pg.id(), 42, "group id");
    assert_eq_test!(pg.session_id(), 42, "session id == leader pid");

    let session_weak = KArc::downgrade(pg.session());

    let pg2 = new_group_in_session(43, pg.session().clone()).expect("mint second group");
    assert_eq_test!(pg2.session_id(), 42, "second group shares the session");

    drop(pg);
    assert_test!(
        session_weak.upgrade().is_some(),
        "session alive while any group lives"
    );

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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &task_guard, &mut frame);
    assert_eq_test!(
        frame.rax(),
        0,
        "TIOCSCTTY should succeed for session leader"
    );

    let sid = task_guard.sid();
    let ctty = task_guard.controlling_tty();
    assert_eq_test!(ctty, Some(TtyIndex(0)), "controlling_tty should be tty0");

    let tty_sid =
        slopos_kernel_services::syscall_services::tty::get_session_id(TtyIndex(0)).unwrap_or(0);
    assert_eq_test!(tty_sid, sid, "tty session should match caller sid");

    task_terminate(task_id);
    TestResult::Pass
}

/// The console read resolves the caller's controlling terminal rather than
/// `TtyIndex(0)`, so no task reads the operator's keystrokes off a terminal it
/// does not own.
pub fn test_console_read_without_a_controlling_tty_is_refused() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    assert_eq_test!(
        task_guard.controlling_tty(),
        None,
        "fixture task already owns a terminal"
    );

    let Some(user_buf) = map_user_rw_page(pid) else {
        return fail!("could not map a user page");
    };

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = user_buf;
    frame.regs_mut().rsi = 16;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_user_read, &task_guard, &mut *frame)
    });
    assert_eq_test!(
        frame.rax(),
        slopos_abi::Errno::ENXIO.as_u64(),
        "a task with no controlling terminal was served a read"
    );

    drop(task_guard);
    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_console_read_without_a_controlling_tty_is_refused,
    suite = syscall_core
);

pub fn test_tiocsctty_non_leader_rejected() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_guard = assert_some!(task_find_by_id(child_id), "child lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &child_guard, &mut frame);
    assert_test!(
        frame.rax() != 0,
        "TIOCSCTTY should fail for non-session leader"
    );

    assert_eq_test!(
        child_guard.controlling_tty(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &task_guard, &mut frame);
    assert_eq_test!(
        frame.rax(),
        0,
        "TIOCSCTTY should succeed before /dev/tty open"
    );

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(task_id);
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
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.regs_mut().rdi = 0;
    ioctl_frame.regs_mut().rsi = TIOCSCTTY;
    ioctl_frame.regs_mut().rdx = 0;
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &parent_guard, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.rax(), 0, "parent TIOCSCTTY should succeed");

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);

    let child_guard = assert_some!(task_find_by_id(child_id), "child lookup failed");

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, &child_guard, &mut setsid_frame);
    assert_eq_test!(
        setsid_frame.rax() as u32,
        child_id,
        "setsid should succeed for child"
    );
    assert_eq_test!(
        child_guard.controlling_tty(),
        None,
        "child should drop inherited ctty"
    );
    assert_eq_test!(
        parent_guard.controlling_tty(),
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
    let leader_guard = assert_some!(task_find_by_id(leader_id), "leader lookup failed");

    let mut ioctl_frame = zero_frame();
    ioctl_frame.regs_mut().rdi = 0;
    ioctl_frame.regs_mut().rsi = TIOCSCTTY;
    ioctl_frame.regs_mut().rdx = 0;
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &leader_guard, &mut ioctl_frame);
    assert_eq_test!(ioctl_frame.rax(), 0, "leader TIOCSCTTY should succeed");

    let child_id = task_fork(&leader_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id), "child lookup failed");

    slopos_kernel_services::syscall_services::tty::hangup(TtyIndex(0));

    assert_eq_test!(
        leader_guard.controlling_tty(),
        None,
        "leader ctty should clear on hangup"
    );
    assert_eq_test!(
        child_guard.controlling_tty(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    // The master open holds the pair alive; dropped at the end to tear it down.
    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty(
        slopos_ostd::process::quota::root(),
    ) {
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

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(task_id);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N) failed");
    assert_eq_test!(
        task_guard.controlling_tty(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty(
        slopos_ostd::process::quota::root(),
    ) {
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

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(task_id);
    let fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/pts/N, O_NOCTTY) failed");
    assert_eq_test!(
        task_guard.controlling_tty(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    // The returned backing owns the pair for the test's duration.
    let (master_idx, master_open) = match slopos_kernel_services::syscall_services::tty::alloc_pty(
        slopos_ostd::process::quota::root(),
    ) {
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

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    // file_poll_register_fd registers the current task as the waiter, so the
    // FD-owning task must be PCR.current_task (mirrors the real poll path).
    make_task_current(task_id);

    let slave_fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    assert_test!(slave_fd >= 0, "open(/dev/pts/N) failed");

    // The registration carries only a KWeak<OpenFile> — never a strong ref —
    // so it resolves to the live open file while the fd is open.
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, slave_fd, POLLIN);
    assert_test!(!reg.is_stale(), "fresh registration must not be stale");

    assert_eq_test!(file_close_fd(pid, slave_fd), 0, "close slave fd failed");
    assert_test!(
        reg.is_stale(),
        "registration must go stale once its fd closes"
    );

    let reused_fd = file_open_for_process(pid, &path[..path.len() - 1], O_RDONLY | O_NOCTTY as u32);
    assert_test!(reused_fd >= 0, "reopen(/dev/pts/N) failed");
    assert_eq_test!(reused_fd, slave_fd, "expected fd-number reuse");

    let reg_reused = slopos_fs::fileio::file_poll_register_fd(pid, reused_fd, POLLIN);
    assert_test!(
        !reg_reused.is_stale(),
        "reused fd must resolve to a live object"
    );
    assert_test!(
        reg.is_stale(),
        "stale registration must not adopt the reused fd"
    );

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
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    for _ in 0..128 {
        let addr = slopos_mm::process_vm::process_vm_mmap(
            pid.process().expect("a live process"),
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
        if slopos_mm::process_vm::process_vm_munmap(
            pid.process().expect("a live process"),
            addr,
            4096,
        ) != 0
        {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    }

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_fork_kernel_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let kernel_task_id = create_test_kernel_task();
    assert_test!(kernel_task_id != INVALID_TASK_ID);

    let kernel_task_guard = assert_some!(task_find_by_id(kernel_task_id));

    use slopos_sched::task::task_fork;
    let child_id = task_fork(&kernel_task_guard, None);
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

    assert_test!(task_find_by_id(task_id).is_some());

    task_terminate(task_id);

    if let Some(task_after) = task_find_by_id(task_id) {
        let state = task_after.status();
        if state == TaskStatus::Terminated {
            let child_id = task_fork(&task_after, None);
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

    let task_guard = assert_some!(task_find_by_id(task_id));

    task_set_state(task_id, TaskStatus::Blocked);

    let child_id = task_fork(&task_guard, None);

    task_terminate(task_id);
    if child_id != INVALID_TASK_ID {
        task_terminate(child_id);
    }

    TestResult::Pass
}

pub fn test_fork_cleanup_on_failure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let free_before = slopos_mm::page_alloc::get_page_allocator_stats().free;

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..5 {
        let _ = slopos_mm::process_vm::process_vm_alloc(
            resolve_pid(parent_pid),
            4096 * 4,
            slopos_mm::paging_defs::PageFlags::WRITABLE.bits() as u32,
        );
    }

    for _ in 0..3 {
        let child_pid = slopos_mm::process_vm::process_vm_clone_cow(resolve_pid(parent_pid));
        if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
            slopos_mm::process_vm::destroy_process_vm(resolve_pid(child_pid));
        }
    }

    slopos_mm::process_vm::destroy_process_vm(resolve_pid(parent_pid));

    let free_after = slopos_mm::page_alloc::get_page_allocator_stats().free;

    let leak = free_before.saturating_sub(free_after);
    assert_test!(leak <= 64, "memory leak after fork cleanup: {} pages", leak);

    TestResult::Pass
}

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

pub fn test_brk_extreme_values() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();
    let pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(pid != slopos_abi::task::INVALID_PROCESS_ID);

    let current_brk = slopos_mm::process_vm::process_vm_brk(resolve_pid(pid), 0);
    if current_brk == 0 {
        klog_info!("SYSCALL_TEST: Initial brk returned 0 (might be a bug)");
    }

    let max_brk = slopos_mm::process_vm::process_vm_brk(resolve_pid(pid), u64::MAX);
    assert_test!(max_brk != u64::MAX, "brk accepted u64::MAX");

    let kernel_brk = slopos_mm::process_vm::process_vm_brk(resolve_pid(pid), 0xFFFF_8000_0000_0000);
    assert_test!(
        kernel_brk != 0xFFFF_8000_0000_0000,
        "brk accepted kernel address"
    );

    slopos_mm::process_vm::destroy_process_vm(resolve_pid(pid));
    TestResult::Pass
}

pub fn test_memfd_create_boundaries() -> TestResult {
    let result = slopos_mm::memfd::memfd_create(0, slopos_ostd::process::quota::root());
    assert_test!(result.is_some(), "memfd_create should succeed");
    if let Some((handle, _ops, backing)) = result {
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 0);
        assert_test!(rc < 0, "ftruncate(0) should fail");

        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 4096);
        assert_eq_test!(rc, 0, "ftruncate(4096) should succeed");

        // Sizing is one-shot.
        let rc = slopos_mm::memfd::memfd_ftruncate(handle, 8192);
        assert_test!(rc < 0, "ftruncate twice should fail");

        // Dropping the backing runs the memfd teardown.
        drop(backing);
    }
    TestResult::Pass
}

pub fn test_terminate_already_terminated() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    assert_eq_test!(task_terminate(task_id), 0, "first termination failed");

    let _r2 = task_terminate(task_id);

    if let Some(task) = task_find_by_id(task_id) {
        let state = task.status();
        assert_test!(state != TaskStatus::Ready, "terminated task in READY state");
    }

    TestResult::Pass
}

pub fn test_operations_on_terminated_task() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID);

    task_terminate(task_id);

    // Looking a terminated task up must be a clean miss, not a fault.
    let _ = task_find_by_id(task_id);

    use slopos_sched::task::task_set_state;
    let state_result = task_set_state(task_id, TaskStatus::Ready);
    if state_result == 0 {
        if let Some(task) = task_find_by_id(task_id) {
            let current_state = task.status();
            assert_test!(
                current_state != TaskStatus::Ready,
                "revived terminated task"
            );
        }
    }

    TestResult::Pass
}

pub fn test_fork_memory_pressure() -> TestResult {
    let _fixture = SyscallFixture::new();

    slopos_mm::process_vm::init_process_vm();

    let parent_pid = slopos_mm::process_vm::create_process_vm();
    assert_test!(parent_pid != slopos_abi::task::INVALID_PROCESS_ID);

    for _ in 0..10 {
        let addr = slopos_mm::process_vm::process_vm_alloc(
            resolve_pid(parent_pid),
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

    let child_pid = slopos_mm::process_vm::process_vm_clone_cow(resolve_pid(parent_pid));

    let free_before = slopos_mm::page_alloc::get_page_allocator_stats().free;

    if child_pid != slopos_abi::task::INVALID_PROCESS_ID {
        slopos_mm::process_vm::destroy_process_vm(resolve_pid(child_pid));
    }
    slopos_mm::process_vm::destroy_process_vm(resolve_pid(parent_pid));

    for i in 0..stress_count {
        free_page_frame(stress_pages[i]);
    }

    let free_after = slopos_mm::page_alloc::get_page_allocator_stats().free;

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
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent task lookup failed");

    parent_guard.set_fs_base(0x0000_1111_2222_3000);

    let flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD | CLONE_SETTLS;
    let child_id = match task_clone(&parent_guard, None, flags, 0, 0, 0, 0x0000_5555_6666_7000) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let child_guard = assert_some!(task_find_by_id(child_id), "child task lookup failed");

    assert_eq_test!(
        child_guard.tgid,
        parent_guard.tgid,
        "thread did not join parent thread-group"
    );
    assert_eq_test!(
        child_guard.fs_base(),
        0x0000_5555_6666_7000,
        "child TLS base not set by CLONE_SETTLS"
    );
    assert_eq_test!(
        parent_guard.fs_base(),
        0x0000_1111_2222_3000,
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
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent task lookup failed");

    let thread_flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD;
    let thread_id = match task_clone(&parent_guard, None, thread_flags, 0, 0, 0, 0) {
        Ok(id) => {
            task_set_state(id, TaskStatus::Blocked);
            id
        }
        Err(_) => {
            task_terminate(parent_id);
            return TestResult::Fail;
        }
    };

    let fork_id = task_fork(&parent_guard, None);
    assert_test!(fork_id != INVALID_TASK_ID, "fork after clone failed");
    task_set_state(fork_id, TaskStatus::Blocked);

    let thread_guard = assert_some!(task_find_by_id(thread_id), "thread task lookup failed");
    let fork_guard = assert_some!(task_find_by_id(fork_id), "fork child task lookup failed");

    assert_eq_test!(thread_guard.tgid, parent_guard.tgid, "thread tgid mismatch");
    assert_eq_test!(
        fork_guard.tgid,
        fork_id,
        "fork child should be its own thread-group leader"
    );
    assert_eq_test!(
        fork_guard.parent_task_id(),
        parent_id,
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wait_frame)
    });
    assert_eq_test!(
        wait_frame.rax(),
        ERRNO_EAGAIN,
        "FUTEX_WAIT mismatch must return -EAGAIN"
    );

    let mut wake_frame = zero_frame();
    wake_frame.regs_mut().rdi = uaddr;
    wake_frame.regs_mut().rsi = FUTEX_WAKE;
    wake_frame.regs_mut().rdx = 1;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wake_frame)
    });
    assert_eq_test!(
        wake_frame.rax(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
        crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wake_frame)
    });
    assert_eq_test!(
        wake_frame.rax(),
        0,
        "initial FUTEX_WAKE should wake no waiters"
    );

    let mut wait_frame = zero_frame();
    wait_frame.regs_mut().rdi = uaddr;
    wait_frame.regs_mut().rsi = FUTEX_WAIT;
    wait_frame.regs_mut().rdx = 2;
    wait_frame.regs_mut().r10 = 0;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wait_frame)
    });
    assert_eq_test!(
        wait_frame.rax(),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
            crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wake_frame)
        });
        if wake_frame.rax() > wake_frame.rdx() {
            task_terminate(task_id);
            return TestResult::Fail;
        }

        let mut wait_frame = zero_frame();
        wait_frame.regs_mut().rdi = uaddr;
        wait_frame.regs_mut().rsi = FUTEX_WAIT;
        wait_frame.regs_mut().rdx = 2;
        wait_frame.regs_mut().r10 = 0;
        let _ = with_user_process_context(pid, || {
            crate::syscall::dispatch::dispatch_handler(syscall_futex, &task_guard, &mut wait_frame)
        });
        if wait_frame.rax() != ERRNO_EAGAIN {
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
            &task_guard,
            &mut *action_frame,
        )
    });
    assert_eq_test!(action_frame.rax(), 0, "rt_sigaction failed");

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

    let stack_top = process_vm_get_stack_top(pid.process().expect("a live process"));
    let original_rsp = stack_top.wrapping_sub(0x200);
    let original_rip = 0x5000_1234;

    let mut kill_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &task_guard, &mut *kill_frame)
    });
    assert_eq_test!(kill_frame.rax(), 0, "kill(SIGUSR1) failed");

    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = original_rip;
    user_frame.regs_mut().rsp = original_rsp;
    user_frame.regs_mut().rax = 0xAA55;
    user_frame.regs_mut().rbx = 0xBB66;
    deliver_pending_signal_as_current(task_id, pid, &user_frame);

    assert_eq_test!(
        user_frame.rip(),
        new_action.sa_handler,
        "signal handler RIP not installed"
    );
    assert_eq_test!(
        user_frame.rdi(),
        SIGUSR1 as u64,
        "signal number not passed in RDI"
    );

    // Restorer at [rsp]; the SignalFrame starts at [rsp + 8].
    let restorer_on_stack: u64 = match user_copy_in(pid, user_frame.rsp()) {
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

    let sigframe_addr = user_frame.rsp().wrapping_add(8);
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

    // Simulate the handler's `ret` popping the restorer.
    user_frame.regs_mut().rsp = user_frame.rsp().wrapping_add(8);
    // Sigreturn restores the FPU image through the `Current` witness and
    // refuses the frame without one, so run it as current like production.
    make_task_current(task_id);
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigreturn,
            &task_guard,
            &mut *user_frame,
        )
    });
    park_bootstrap_on_current_cpu();
    assert_eq_test!(
        user_frame.rip(),
        original_rip,
        "rt_sigreturn did not restore RIP"
    );
    assert_eq_test!(
        user_frame.rsp(),
        original_rsp,
        "rt_sigreturn did not restore RSP"
    );

    // One byte of the XSAVE image turned into a component XCR0 does not enable.
    // `XRSTOR64` faults on that in ring 0, so sigreturn must refuse the whole
    // frame, and refuse it before committing any register.
    let poison_addr = sigframe_addr
        .wrapping_add(core::mem::size_of::<SignalFrame>() as u64)
        .wrapping_add(slopos_ostd::task::XSTATE_BV_OFFSET as u64 + 7);
    assert_test!(
        user_copy_out(pid, poison_addr, &0x80u8),
        "failed to poison the frame's XSAVE image"
    );

    let handler_rip = 0x4003_0000;
    user_frame.regs_mut().rip = handler_rip;
    user_frame.regs_mut().rsp = sigframe_addr;

    make_task_current(task_id);
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigreturn,
            &task_guard,
            &mut *user_frame,
        )
    });
    // The poison must not survive in the task's own save area either: the
    // scheduler restores that slot on the next context switch.
    let leftover = {
        let current = assert_some!(Current::get(), "current task after dispatch");
        current.task().with_fpu_bytes_mut(&current, |data| {
            data[slopos_ostd::task::XSTATE_BV_OFFSET + 7]
        })
    };
    park_bootstrap_on_current_cpu();

    assert_eq_test!(
        user_frame.rax(),
        slopos_abi::Errno::EFAULT.as_u64(),
        "a malformed FPU image did not fail rt_sigreturn"
    );
    assert_eq_test!(
        user_frame.rip(),
        handler_rip,
        "a rejected sigreturn moved RIP anyway"
    );
    assert_eq_test!(
        user_frame.rsp(),
        sigframe_addr,
        "a rejected sigreturn moved RSP anyway"
    );
    assert_eq_test!(
        leftover,
        0,
        "the rejected image was left in the task's save area"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// A synthetic user-mode `InterruptFrame` (`cs & 3 == 3`) carrying the given
/// RIP/RSP. Heap-backed to keep it off the test stack.
fn user_irq_frame(rip: u64, rsp: u64) -> KBox<slopos_arch::InterruptFrame> {
    let mut frame: KBox<slopos_arch::InterruptFrame> = KBox::zeroed().expect("alloc");
    frame.rip = rip;
    frame.rsp = rsp;
    frame.cs = 0x23; // user code selector (RPL 3)
    frame.ss = 0x1B; // user data selector (RPL 3)
    frame.rflags = 0x202; // IF + MBO
    frame
}

/// The working directory round-trips through its witness cell. A regression in
/// the cell's bounds handling or in the publish-length-after-bytes ordering
/// surfaces as a corrupted path in userland, not as a fault.
pub fn test_task_cwd_round_trips_through_the_cell() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_kernel_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    assert_test!(task_find_by_id(task_id).is_some(), "task lookup failed");
    make_task_current(task_id);

    let Some(current) = Current::get() else {
        task_terminate(task_id);
        return TestResult::Fail;
    };

    let initial_ok = current
        .task()
        .with_cwd(&current, |cwd| cwd == b"/\0".as_slice());
    assert_test!(initial_ok, "fresh task cwd is not \"/\"");

    assert_test!(
        current.task().set_cwd(&current, b"/usr/share"),
        "set_cwd rejected a path that fits"
    );
    let round_trip = current
        .task()
        .with_cwd(&current, |cwd| cwd == b"/usr/share\0".as_slice());
    assert_test!(round_trip, "cwd did not round-trip through the cell");

    // The bounds check is the cell's own: one byte short of the buffer is the
    // longest path that fits with its NUL.
    let too_long = [b'a'; 256];
    assert_test!(
        !current.task().set_cwd(&current, &too_long),
        "set_cwd accepted a path with no room for its NUL"
    );
    let unchanged = current
        .task()
        .with_cwd(&current, |cwd| cwd == b"/usr/share\0".as_slice());
    assert_test!(unchanged, "a rejected set_cwd still mutated the buffer");

    drop(current);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_delivery_on_irq_exit_dispatch() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
            &task_guard,
            &mut install_frame,
        )
    });
    assert_eq_test!(install_frame.rax(), 0, "SIGINT install failed");

    // Raised directly; this is what `kill()` would leave pending.
    let _ = task::task_signal_raise(&*task_guard, sig_bit(SIGINT));

    // The IRQ-exit path resolves the task from PCR.current_task.
    make_task_current(task_id);

    let stack_top = process_vm_get_stack_top(pid.process().expect("a live process"));
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

    // Handler RSP points at the restorer word, the SignalFrame at RSP + 8, then
    // the FPU save area — aligned so the handler enters with `rsp % 16 == 8`
    // (SysV: `(rsp + 8) % 16 == 0` at a function's entry point).
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

    let blocked = task_guard.signal_pending();
    assert_eq_test!(blocked, 0, "pending SIGINT bit should be cleared");

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_signal_delivery_on_irq_exit_kernel_frame_untouched() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    let _ = task::task_signal_raise(&*task_guard, sig_bit(SIGINT));
    make_task_current(task_id);

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
    assert_eq_test!(
        task_guard.signal_pending() & sig_bit(SIGINT),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
            &task_guard,
            &mut install_frame,
        )
    });
    assert_eq_test!(install_frame.rax(), 0, "SIGINT install failed");

    let _ = task::task_signal_raise(&*task_guard, sig_bit(SIGINT));
    make_task_current(task_id);

    // RSP points into an unmapped user address, so the restorer `copy_to_user`
    // fails and delivery must abort without touching the frame.
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
        task_guard.signal_pending() & sig_bit(SIGINT),
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
            &task_guard,
            &mut install_frame,
        )
    });
    assert_eq_test!(install_frame.rax(), 0, "sigaction install failed");

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
            &task_guard,
            &mut block_frame,
        )
    });
    assert_eq_test!(block_frame.rax(), 0, "rt_sigprocmask(SIG_SETMASK) failed");

    let mut kill_frame = zero_frame();
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &task_guard, &mut kill_frame)
    });
    assert_eq_test!(kill_frame.rax(), 0, "kill(SIGUSR1) failed");

    let stack_top = process_vm_get_stack_top(pid.process().expect("a live process"));
    let mut user_frame = zero_frame();
    user_frame.regs_mut().rip = 0x6000_1111;
    user_frame.regs_mut().rsp = stack_top.wrapping_sub(0x200);
    deliver_pending_signal_as_current(task_id, pid, &user_frame);
    assert_eq_test!(
        user_frame.rip(),
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
            &task_guard,
            &mut unblock_frame,
        )
    });
    assert_eq_test!(unblock_frame.rax(), 0, "rt_sigprocmask(SIG_UNBLOCK) failed");

    deliver_pending_signal_as_current(task_id, pid, &user_frame);
    assert_eq_test!(
        user_frame.rip(),
        action.sa_handler,
        "unblocked pending signal was not delivered"
    );

    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_sigchld_and_wait_interaction() -> TestResult {
    // Scope is SIGCHLD's send-time disposition gate: its default is Ignore, so
    // an unblocked default-disposition parent never accumulates the bit, while
    // a parent that blocked it (the signalfd pattern) must still see it pend.
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    // Unblocked + SIG_DFL: the exit-path raise is dropped at the send site.
    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "task_fork failed");
    task_set_state(child_id, TaskStatus::Blocked);

    assert_eq_test!(task_terminate(child_id), 0, "failed to terminate child");

    let pending = parent_guard.signal_pending();
    assert_eq_test!(
        pending & sig_bit(SIGCHLD),
        0,
        "default-ignored SIGCHLD must be dropped at send, not pend"
    );

    // Blocked SIGCHLD must pend so a signalfd drain can observe the exit.
    parent_guard.set_signal_blocked(sig_bit(SIGCHLD));

    let child2_id = task_fork(&parent_guard, None);
    assert_test!(child2_id != INVALID_TASK_ID, "second task_fork failed");
    task_set_state(child2_id, TaskStatus::Blocked);

    assert_eq_test!(
        task_terminate(child2_id),
        0,
        "failed to terminate second child"
    );

    let pending = parent_guard.signal_pending();
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
        crate::syscall::dispatch::dispatch_handler(syscall_arch_prctl, &task_guard, &mut set_frame)
    });
    assert_eq_test!(set_frame.rax(), 0, "ARCH_SET_FS failed");

    let mut get_frame = zero_frame();
    get_frame.regs_mut().rdi = ARCH_GET_FS;
    get_frame.regs_mut().rsi = out_addr;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_arch_prctl, &task_guard, &mut get_frame)
    });
    assert_eq_test!(get_frame.rax(), 0, "ARCH_GET_FS failed");

    let got_fs: u64 = match user_copy_in(pid, out_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(got_fs, expected_fs, "ARCH_GET_FS returned wrong value");

    let child_no_settls = match task_clone(
        &task_guard,
        None,
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
    let child_guard = assert_some!(
        task_find_by_id(child_no_settls),
        "clone child lookup failed"
    );
    assert_eq_test!(
        child_guard.fs_base(),
        expected_fs,
        "clone without CLONE_SETTLS must inherit FS base"
    );

    task_terminate(child_no_settls);
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_pipe_write_read_basic() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

pub fn test_pipe_eof_returns_zero() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");

    let mut out = [0u8; 16];
    let nread = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(nread as usize, payload.len(), "first read wrong count");
    assert_test!(
        &out[..payload.len()] == payload,
        "first read payload mismatch"
    );

    let eof = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut out));
    assert_eq_test!(eof, 0, "EOF read should return 0, not -1");

    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_pipe_broken_pipe() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    assert_eq_test!(file_close_fd(pid, read_fd), 0, "close read failed");

    let payload = b"orphan";
    let result = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(result, -32, "write to broken pipe should return EPIPE(-32)");

    assert_eq_test!(file_close_fd(pid, write_fd), 0, "close write failed");
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_pipe_multi_write_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

pub fn test_pipe_partial_read() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let mut payload = [0u8; 100];
    for i in 0..100 {
        payload[i] = i as u8;
    }
    let written = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&payload));
    assert_eq_test!(written as usize, 100, "write 100 bytes failed");

    let mut buf1 = [0u8; 50];
    let r1 = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut buf1));
    assert_eq_test!(r1 as usize, 50, "first partial read wrong count");
    assert_test!(
        &buf1[..] == &payload[..50],
        "first partial read data mismatch"
    );

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

pub fn test_pipe_buffer_full() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create (nonblock) failed"
    );

    let chunk = [0xABu8; 512];
    let mut total_written: usize = 0;
    for _ in 0..8 {
        let w = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&chunk));
        assert_test!(w > 0, "write chunk failed while filling buffer");
        total_written += w as usize;
    }
    assert_eq_test!(total_written, 4096, "did not fill pipe buffer to 4096");

    let extra = [0xCDu8; 1];
    let over = file_write_fd(pid, write_fd, &mut KernelIoBufRef::new(&extra));
    assert_eq_test!(over, -11, "write to full pipe should return EAGAIN (-11)");

    let mut drain: KBox<[u8; 4096]> = KBox::zeroed().expect("alloc");
    let drained = file_read_fd(pid, read_fd, &mut KernelIoBuf::new(&mut *drain));
    assert_eq_test!(drained as usize, 4096, "drain read wrong count");

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

/// When the current task exits its file table must be destroyed, so pipe writer
/// refs are released and peer readers observe EOF.
pub fn test_exit_current_task_releases_pipe_refs() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );

    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");

    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2.handle().expect("a user process")),
        0,
        "file table clone failed"
    );

    // Keep only the read end in pid2.
    assert_eq_test!(file_close_fd(pid2, write_fd), 0, "pid2 close write failed");

    // Current, so `task_terminate` takes the current-task cleanup path.
    make_task_current(t1);
    assert_eq_test!(task::task_terminate(t1), 0, "current-task terminate failed");
    park_bootstrap_on_current_cpu();

    let mut one = [0u8; 1];
    let r = file_read_fd(pid2, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_eq_test!(r, 0, "reader should observe EOF after current task exit");

    assert_eq_test!(file_close_fd(pid2, read_fd), 0, "pid2 close read failed");
    task_terminate(t2);
    TestResult::Pass
}

/// Fork-style clones keep close-on-exec descriptors; only exec strips them
/// (POSIX fork semantics).
pub fn test_fork_clone_keeps_cloexec_fds() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );

    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");

    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2.handle().expect("a user process")),
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

/// A SlopRing fd is process-private, so a fork-style clone leaves the child's
/// table empty there. The ordinary pipe fd beside it is the control: it must
/// still be inherited, proving the clone ran rather than failing wholesale.
pub fn test_ring_fd_not_inherited_by_fork() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );

    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");
    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let ring_fd = slopos_ring::ring_setup(pid1, 4, |_| Ok(()));
    assert_test!(ring_fd >= 0, "ring_setup failed: {}", ring_fd);

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_clone_table_for_process(pid1, pid2.handle().expect("a user process")),
        0,
        "fork clone failed"
    );

    assert_test!(
        fileio_get_open_file_handle(pid2, ring_fd).is_none(),
        "fork must not carry the ring fd into the child"
    );
    assert_test!(
        fileio_get_open_file_handle(pid2, read_fd).is_some(),
        "fork must still inherit ordinary descriptors"
    );

    let _ = file_close_fd(pid2, read_fd);
    let _ = file_close_fd(pid2, write_fd);
    assert_eq_test!(file_close_fd(pid1, ring_fd), 0, "pid1 close ring failed");
    assert_eq_test!(file_close_fd(pid1, write_fd), 0, "pid1 close write failed");
    assert_eq_test!(file_close_fd(pid1, read_fd), 0, "pid1 close read failed");
    task_terminate(t1);
    task_terminate(t2);
    TestResult::Pass
}

/// `exec` tears a SlopRing fd down: the ring's user mapping does not survive
/// the image replacement, so neither does the descriptor naming it.
pub fn test_ring_fd_closed_on_exec() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p_guard = assert_some!(task_find_by_id(t), "task lookup failed");
    let Some(pid) = p_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let ring_fd = slopos_ring::ring_setup(pid, 4, |_| Ok(()));
    assert_test!(ring_fd >= 0, "ring_setup failed: {}", ring_fd);
    assert_test!(
        fileio_get_open_file_handle(pid, ring_fd).is_some(),
        "ring fd absent right after setup"
    );

    fileio_close_on_exec(pid);

    assert_test!(
        fileio_get_open_file_handle(pid, ring_fd).is_none(),
        "exec must tear the ring fd down"
    );
    assert_test!(
        fileio_get_open_file_handle(pid, 1).is_some(),
        "exec must keep descriptors without FD_CLOEXEC"
    );

    task_terminate(t);
    TestResult::Pass
}

/// A spawned child with no actions starts with an empty fd table — no stdio.
pub fn test_spawn_empty_table_unless_actions() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p_guard = assert_some!(task_find_by_id(t), "task lookup failed");
    let Some(pid) = p_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    fileio_destroy_table_for_process(pid.handle().expect("a user process"));
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid.handle().expect("a user process")),
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
    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");
    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2.handle().expect("a user process")),
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

    assert_eq_test!(file_close_fd(pid1, write_fd), 0, "pid1 close write failed");
    let mut one = [0u8; 1];
    let r = file_read_fd(pid1, read_fd, &mut KernelIoBuf::new(&mut one));
    assert_test!(
        r != 0,
        "reader must not see EOF while child holds a write end"
    );

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
    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");
    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2.handle().expect("a user process")),
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

/// A mid-list bad fd aborts the whole action list: a `Transfer` staged before
/// the failing action must not have emptied its parent slot.
pub fn test_spawn_actions_all_or_nothing() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t1 = create_test_user_task();
    let t2 = create_test_user_task();
    assert_test!(
        t1 != INVALID_TASK_ID && t2 != INVALID_TASK_ID,
        "failed to create tasks"
    );
    let p1_guard = assert_some!(task_find_by_id(t1), "task1 lookup failed");
    let p2_guard = assert_some!(task_find_by_id(t2), "task2 lookup failed");
    let Some(pid1) = p1_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let Some(pid2) = p2_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid1, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
    assert_eq_test!(
        fileio_create_empty_table_for_process(pid2.handle().expect("a user process")),
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

    // The aborted spawn tears the child table down.
    fileio_destroy_table_for_process(pid2.handle().expect("a user process"));
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

/// The spawn ABI reads `SpawnAttrs` by pointer (arg4). A bad pointer is EFAULT
/// before exec; a valid one reaches exec and reports the real load error.
pub fn test_spawn_path_rejects_bad_attrs() -> TestResult {
    use crate::syscall::handlers::syscall_spawn_path;
    use slopos_abi::spawn::SpawnAttrs;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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
        envp_ptr: 0,
        envp_len: 0,
    };
    let attrs_addr = user_page + 512;
    assert_test!(
        user_copy_out(pid, attrs_addr, &attrs),
        "failed to write attrs into user memory"
    );

    let mut frame_bad = zero_frame();
    frame_bad.regs_mut().rdi = user_page; // path_ptr
    frame_bad.regs_mut().rsi = path.len() as u64; // path_len
    frame_bad.regs_mut().rdx = 0; // argv_ptr
    frame_bad.regs_mut().r10 = 0; // argc
    frame_bad.regs_mut().r8 = 0xDEAD_BEEF_CAFE_BABEu64; // attrs_ptr (garbage)
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_spawn_path, &task_guard, &mut frame_bad)
    });
    assert_eq_test!(
        frame_bad.rax(),
        slopos_abi::Errno::EFAULT.as_u64(),
        "garbage attrs pointer must return EFAULT"
    );

    let mut frame_ok = zero_frame();
    frame_ok.regs_mut().rdi = user_page;
    frame_ok.regs_mut().rsi = path.len() as u64;
    frame_ok.regs_mut().rdx = 0;
    frame_ok.regs_mut().r10 = 0;
    frame_ok.regs_mut().r8 = attrs_addr;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_spawn_path, &task_guard, &mut frame_ok)
    });
    // ExecError::NoEntry = -2, returned via ctx.ok(err as i32 as u64).
    let exec_no_entry = (-2i32) as u64;
    assert_eq_test!(
        frame_ok.rax(),
        exec_no_entry,
        "valid attrs with missing binary must reach exec and return NoEntry"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// A spawn request cannot name its own privileges. `NO_PREEMPT` is the worst
/// case: it has no setter anywhere in the tree, and one non-preemptible spinner
/// pinned per CPU wedges the machine. The calling task holds
/// `TASK_FLAG_USER_MODE` and nothing else, which is the principal under test.
pub fn test_spawn_path_rejects_privileged_flags() -> TestResult {
    use crate::syscall::handlers::syscall_spawn_path;
    use slopos_abi::spawn::SpawnAttrs;
    use slopos_abi::task::{
        TASK_FLAG_COMPOSITOR, TASK_FLAG_DISPLAY_EXCLUSIVE, TASK_FLAG_NEW_PGRP,
        TASK_FLAG_NO_PREEMPT, TASK_FLAG_SYSTEM,
    };

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let user_page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    // A path that fails at VFS open, so no case can create a live task.
    let path = b"/bin/noent";
    assert_test!(
        user_copy_out(pid, user_page, path),
        "failed to write path into user memory"
    );
    let attrs_addr = user_page + 512;

    const NO_CONTEXT: u64 = u64::MAX;
    let spawn = |priority: u8, flags: u16| -> u64 {
        let attrs = SpawnAttrs {
            priority,
            _pad: [0; 3],
            flags,
            _pad2: 0,
            actions_ptr: 0,
            actions_len: 0,
            sigdefault_mask: 0,
            envp_ptr: 0,
            envp_len: 0,
        };
        if !user_copy_out(pid, attrs_addr, &attrs) {
            return NO_CONTEXT;
        }
        let mut frame = zero_frame();
        frame.regs_mut().rdi = user_page;
        frame.regs_mut().rsi = path.len() as u64;
        frame.regs_mut().rdx = 0;
        frame.regs_mut().r10 = 0;
        frame.regs_mut().r8 = attrs_addr;
        with_user_process_context(pid, || {
            crate::syscall::dispatch::dispatch_handler(syscall_spawn_path, &task_guard, &mut frame)
        })
        .map(|_| frame.rax())
        .unwrap_or(NO_CONTEXT)
    };

    let eperm = slopos_abi::Errno::EPERM.as_u64();
    let einval = slopos_abi::Errno::EINVAL.as_u64();
    const NORMAL: u8 = 2;

    // EPERM rather than a malformed-request answer: the caller is asking for
    // something real that it may not have.
    assert_eq_test!(
        spawn(NORMAL, TASK_FLAG_COMPOSITOR),
        eperm,
        "spawning with COMPOSITOR must be EPERM"
    );
    assert_eq_test!(
        spawn(NORMAL, TASK_FLAG_DISPLAY_EXCLUSIVE),
        eperm,
        "spawning with DISPLAY_EXCLUSIVE must be EPERM"
    );
    assert_eq_test!(
        spawn(NORMAL, TASK_FLAG_SYSTEM),
        eperm,
        "spawning with SYSTEM must be EPERM"
    );
    assert_eq_test!(
        spawn(NORMAL, TASK_FLAG_NO_PREEMPT),
        eperm,
        "spawning with NO_PREEMPT must be EPERM — it wedges a CPU"
    );
    assert_eq_test!(
        spawn(NORMAL, slopos_abi::task::TASK_FLAG_NET_ADMIN),
        eperm,
        "spawning with NET_ADMIN must be EPERM — it is conferred by program identity"
    );
    assert_eq_test!(
        spawn(NORMAL, slopos_abi::task::TASK_FLAG_CONSOLE_ADMIN),
        eperm,
        "spawning with CONSOLE_ADMIN must be EPERM — it is conferred by program identity"
    );
    assert_eq_test!(
        spawn(NORMAL, slopos_abi::task::TASK_FLAG_PROC_ADMIN),
        eperm,
        "spawning with PROC_ADMIN must be EPERM — it is conferred by program identity"
    );

    // Undefined bits fail closed so the ABI can grow one. Derived from
    // SPAWN_RESERVED rather than a literal, which would age into a defined bit.
    assert_eq_test!(
        spawn(
            NORMAL,
            1 << slopos_abi::task::SPAWN_RESERVED.trailing_zeros()
        ),
        einval,
        "an undefined flag bit must be EINVAL"
    );
    assert_eq_test!(
        spawn(NORMAL, 0x0040),
        einval,
        "the retired FPU_INITIALIZED bit must stay EINVAL"
    );
    // The retired TASK_FLAG_FPU_INITIALIZED. Retired, not freed.
    assert_eq_test!(
        spawn(NORMAL, 0x0040),
        einval,
        "the retired FPU_INITIALIZED bit must stay refused"
    );
    // EINVAL, not the NoMem `task_build`'s `None` would become.
    assert_eq_test!(
        spawn(NORMAL, slopos_abi::task::TASK_FLAG_KERNEL_MODE),
        einval,
        "KERNEL_MODE must be diagnosed as EINVAL, not mislabelled NoMem"
    );
    // Order: a reserved bit is answered before privilege, so probing reserved
    // bits cannot learn from an EPERM that a bit means something.
    assert_eq_test!(
        spawn(
            NORMAL,
            (1 << slopos_abi::task::SPAWN_RESERVED.trailing_zeros()) | TASK_FLAG_COMPOSITOR
        ),
        einval,
        "a reserved bit must be answered before a privileged one"
    );

    // Tier restriction: only Normal and Low are user-requestable.
    assert_eq_test!(spawn(0, TASK_FLAG_USER_MODE), einval, "High must be EINVAL");
    assert_eq_test!(
        spawn(1, TASK_FLAG_USER_MODE),
        einval,
        "KernelIo must be EINVAL"
    );
    assert_eq_test!(spawn(4, TASK_FLAG_USER_MODE), einval, "Idle must be EINVAL");
    assert_eq_test!(
        spawn(5, TASK_FLAG_USER_MODE),
        einval,
        "an out-of-range tier must be EINVAL"
    );

    // Control: without it the table above would also pass against a handler
    // that refused everything.
    assert_eq_test!(
        spawn(NORMAL, TASK_FLAG_USER_MODE | TASK_FLAG_NEW_PGRP),
        (-2i32) as u64,
        "user-settable flags must pass validation and reach exec"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// `set_cpu_affinity` must check the caller-versus-target relation: without it
/// a `NO_PREEMPT` spinner pinned per CPU wedges the machine. Sharing an address
/// space is the boundary — a sibling can already run code inside the target.
pub fn test_set_cpu_affinity_rejects_other_process() -> TestResult {
    use crate::syscall::handlers::{syscall_get_cpu_affinity, syscall_set_cpu_affinity};

    let _fixture = SyscallFixture::new();

    let caller_id = create_test_user_task();
    assert_test!(caller_id != INVALID_TASK_ID, "failed to create caller task");
    let target_id = create_test_user_task();
    assert_test!(target_id != INVALID_TASK_ID, "failed to create target task");

    let caller = assert_some!(task_find_by_id(caller_id), "caller lookup failed");
    let target = assert_some!(task_find_by_id(target_id), "target lookup failed");
    assert_test!(
        caller.process_id != target.process_id,
        "the two fixture tasks must be in different processes"
    );

    let before = target.cpu_affinity();
    let eperm = slopos_abi::Errno::EPERM.as_u64();

    // One frame, reused. An `InterruptFrame` per case would put four of them
    // live at once and push this past the 2 KiB stack-frame ceiling.
    let mut frame = zero_frame();
    let mut call = |handler: crate::syscall::common::SyscallHandler, rdi: u64, rsi: u64| -> u64 {
        frame = zero_frame();
        frame.regs_mut().rdi = rdi;
        frame.regs_mut().rsi = rsi;
        crate::syscall::dispatch::dispatch_handler(handler, &caller, &mut frame);
        frame.rax()
    };

    assert_eq_test!(
        call(syscall_set_cpu_affinity, target_id as u64, 0x2),
        eperm,
        "pinning another process's task must be EPERM"
    );
    assert_eq_test!(
        target.cpu_affinity(),
        before,
        "a refused set_cpu_affinity must not have stamped the mask"
    );
    assert_eq_test!(
        call(syscall_get_cpu_affinity, target_id as u64, 0),
        eperm,
        "reading another process's affinity must be EPERM"
    );

    // Self still works, so this is a relation check and not a blanket refusal.
    assert_eq_test!(
        call(syscall_set_cpu_affinity, 0, 0x2),
        0,
        "pinning the caller's own task must work"
    );
    assert_eq_test!(
        caller.cpu_affinity(),
        0x2,
        "the caller's own affinity mask must have been stamped"
    );

    task_terminate(target_id);
    task_terminate(caller_id);
    TestResult::Pass
}

/// execve resets caught handlers to SIG_DFL but preserves SIG_IGN.
pub fn test_execve_resets_caught_signals_keeps_ignored() -> TestResult {
    let _fixture = SyscallFixture::new();

    let t = create_test_user_task();
    assert_test!(t != INVALID_TASK_ID, "failed to create task");
    let p_guard = assert_some!(task_find_by_id(t), "task lookup failed");

    // SIGINT caught (custom handler), SIGTSTP ignored, SIGTERM default.
    p_guard.signal_actions[(SIGINT - 1) as usize].store(SignalAction {
        handler: 0x4100_0000,
        mask: 0,
        flags: 0,
        restorer: 0,
    });
    p_guard.signal_actions[(SIGTSTP - 1) as usize].store(SignalAction {
        handler: SIG_IGN,
        mask: 0,
        flags: 0,
        restorer: 0,
    });
    p_guard.signal_actions[(SIGTERM - 1) as usize].reset();

    task::task_reset_caught_handlers(&p_guard);

    let ok = p_guard.signal_actions[(SIGINT - 1) as usize].handler() == slopos_abi::signal::SIG_DFL
        && p_guard.signal_actions[(SIGTSTP - 1) as usize].handler() == SIG_IGN
        && p_guard.signal_actions[(SIGTERM - 1) as usize].handler() == slopos_abi::signal::SIG_DFL;

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
    let p_guard = assert_some!(task_find_by_id(t), "task lookup failed");

    p_guard.signal_actions[(SIGINT - 1) as usize].store(SignalAction {
        handler: 0x4100_0000,
        mask: 0,
        flags: 0,
        restorer: 0,
    });
    p_guard.signal_actions[(SIGTSTP - 1) as usize].store(SignalAction {
        handler: SIG_IGN,
        mask: 0,
        flags: 0,
        restorer: 0,
    });

    let mask = slopos_abi::signal::sig_bit(SIGINT) | slopos_abi::signal::sig_bit(SIGTSTP);
    task::task_default_signals_in_mask(&p_guard, mask);

    let ok = p_guard.signal_actions[(SIGINT - 1) as usize].handler() == slopos_abi::signal::SIG_DFL
        && p_guard.signal_actions[(SIGTSTP - 1) as usize].handler() == slopos_abi::signal::SIG_DFL;

    task_terminate(t);
    if ok {
        TestResult::Pass
    } else {
        TestResult::Fail
    }
}

/// Without a controlling terminal, `open("/dev/tty")` must return ENXIO (-6).
pub fn test_dev_tty_no_ctty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    assert_eq_test!(
        task_guard.controlling_tty(),
        None,
        "fresh task should have no controlling_tty"
    );

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(task_id);
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

pub fn test_dev_tty_with_ctty_succeeds() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0; // fd 0 (console)
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &task_guard, &mut frame);
    assert_eq_test!(frame.rax(), 0, "TIOCSCTTY should succeed");
    assert_eq_test!(
        task_guard.controlling_tty(),
        Some(TtyIndex(0)),
        "controlling_tty should be set after TIOCSCTTY"
    );

    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(task_id);
    let fd = file_open_for_process(pid, b"/dev/tty", O_RDONLY);
    park_bootstrap_on_current_cpu();

    assert_test!(fd >= 0, "open(/dev/tty) with ctty should succeed");

    if fd >= 0 {
        let _ = file_close_fd(pid, fd);
    }
    task_terminate(task_id);
    TestResult::Pass
}

pub fn test_setsid_then_dev_tty_returns_enxio() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &parent_guard, &mut frame);
    assert_eq_test!(frame.rax(), 0, "TIOCSCTTY should succeed");

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id), "child lookup failed");

    assert_eq_test!(
        child_guard.controlling_tty(),
        Some(TtyIndex(0)),
        "child should inherit controlling_tty from parent"
    );

    let mut setsid_frame = zero_frame();
    let _ =
        crate::syscall::dispatch::dispatch_handler(syscall_setsid, &child_guard, &mut setsid_frame);
    assert_eq_test!(
        child_guard.controlling_tty(),
        None,
        "setsid should clear controlling_tty"
    );

    let Some(child_pid) = child_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(child_id);
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

pub fn test_fork_child_inherits_dev_tty() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    let mut frame = zero_frame();
    frame.regs_mut().rdi = 0;
    frame.regs_mut().rsi = TIOCSCTTY;
    frame.regs_mut().rdx = 0;
    let _ = crate::syscall::dispatch::dispatch_handler(syscall_ioctl, &parent_guard, &mut frame);
    assert_eq_test!(frame.rax(), 0, "TIOCSCTTY should succeed for parent");

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "failed to fork child");
    task_set_state(child_id, TaskStatus::Blocked);
    let child_guard = assert_some!(task_find_by_id(child_id), "child lookup failed");

    let parent_ctty = parent_guard.controlling_tty();
    let child_ctty = child_guard.controlling_tty();
    assert_eq_test!(
        parent_ctty,
        child_ctty,
        "child should inherit same controlling_tty as parent"
    );

    let Some(child_pid) = child_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    make_task_current(child_id);
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

/// dup must not copy close-on-exec: cloexec is per-fd-entry, not a property of
/// the open file.
pub fn test_dup_does_not_copy_cloexec() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, 0, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    assert_eq_test!(
        file_fcntl_fd(pid, write_fd, F_SETFD, FD_CLOEXEC),
        0,
        "set cloexec failed"
    );

    let dup_fd = file_dup_fd(pid, write_fd);
    assert_test!(dup_fd >= 0, "dup failed");

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

/// Closing an fd twice is safe: the second finds an empty slot and returns
/// EBADF, never a double teardown of the backing object.
pub fn test_close_twice_is_safe() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

/// Closing one of two dup'd write ends must not tear down the shared backing:
/// the peer reader sees EOF only when the *last* write alias closes.
pub fn test_close_while_dup_keeps_object_alive() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut read_fd = -1;
    let mut write_fd = -1;
    assert_eq_test!(
        file_pipe_create(pid, O_NONBLOCK as u32, &mut read_fd, &mut write_fd),
        0,
        "pipe create failed"
    );

    let write_dup = file_dup_fd(pid, write_fd);
    assert_test!(write_dup >= 0, "dup of write end failed");

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

/// Forcing EMFILE on a tty open must tear down only the open that failed: the
/// backing clone minted for the attempt drops exactly once inside the failed
/// install, and the live tty survives untouched.
pub fn test_open_tty_fd_emfile_no_double_teardown() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    // Open a PTY master to get a real, independently-tracked tty index.
    let master_fd = file_open_for_process(pid, b"/dev/ptmx", O_RDONLY);
    assert_test!(master_fd >= 0, "ptmx open failed");
    let Some(master_tty) = slopos_fs::fileio::file_get_tty_index(pid, master_fd) else {
        let _ = file_close_fd(pid, master_fd);
        task_terminate(task_id);
        return TestResult::Fail;
    };

    // Fill the fd table so the next install hits EMFILE.
    loop {
        let fd = file_dup_fd(pid, master_fd);
        if fd < 0 {
            break;
        }
    }

    // Baseline probe clone: fd aliases share one OpenFile, so the count is the
    // fd-table owner plus this probe.
    let probe_backing = match slopos_kernel_services::syscall_services::tty::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            let _ = file_close_fd(pid, master_fd);
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let before = slopos_ostd::KArc::strong_count(&probe_backing);

    // Mirror the syscall caller: mint a backing, then attempt the open with a
    // full table. The failed install consumes and drops the mint.
    let mint = match slopos_kernel_services::syscall_services::tty::open_tty(master_tty) {
        Ok(b) => b,
        Err(_) => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let probe = file_open_tty_fd(pid, master_tty, 0, mint);
    assert_test!(probe < 0, "open should fail with a full fd table");

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
/// (cloning the backing) and closing (dropping a clone) may move the count.
pub fn test_tty_ioctl_never_changes_open_state() -> TestResult {
    use slopos_kernel_services::syscall_services::tty as ttysvc;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
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

    // The ioctl surface the syscall handler dispatches to.
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

/// Passing a TTY fd via SCM_RIGHTS must be lifetime-balanced: in-flight custody
/// shares the sender's open-file description, and the receiver's close never
/// tears down the sender's terminal.
pub fn test_scm_rights_tty_balanced() -> TestResult {
    use slopos_kernel_services::syscall_services::tty as ttysvc;
    use slopos_net::unix_socket;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

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

    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, master_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);
    let n = unix_socket::unix_sendmsg(srv, b"T", &mut files, slopos_ostd::process::quota::root());
    assert_test!(n == 1, "sendmsg returned {}", n);
    assert_eq_test!(
        slopos_ostd::KArc::strong_count(&probe),
        baseline,
        "in-flight custody must not mint a shadow tty reference"
    );

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

/// Synthetic seekable backing for [`test_dup_shares_offset`]: a 64-byte "file"
/// whose content at offset `o` is the byte `o`. No filesystem dependency — the
/// stest phase runs before any disk is mounted.
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

/// dup'd fds share one open file description, so they share the file offset
/// (POSIX): a read through either advances the position seen by both.
pub fn test_dup_shares_offset() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &SEEK_PROBE_OPS,
        0,
        None,
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(fd >= 0, "synthetic open failed");

    let dup = file_dup_fd(pid, fd);
    assert_test!(dup >= 0, "dup failed");

    let mut first = [0u8; 4];
    let r = file_read_fd(pid, fd, &mut KernelIoBuf::new(&mut first));
    assert_eq_test!(r as usize, first.len(), "read via original failed");
    assert_test!(first == [0, 1, 2, 3], "content at offset 0 mismatch");

    let pos = file_seek_fd(pid, dup, 0, slopos_abi::syscall::SEEK_CUR as u32);
    assert_eq_test!(
        pos,
        first.len() as i64,
        "dup must observe the shared offset advanced by the original's read"
    );

    let mut second = [0u8; 4];
    let r = file_read_fd(pid, dup, &mut KernelIoBuf::new(&mut second));
    assert_eq_test!(r as usize, second.len(), "read via dup failed");
    assert_test!(second == [4, 5, 6, 7], "dup read must continue the offset");

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
    name = test_retired_net_syscalls_stay_unhandled,
    suite = syscall_valid
);
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
    name = test_task_cwd_round_trips_through_the_cell,
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
    name = test_ring_fd_not_inherited_by_fork,
    suite = syscall_valid
);
slopos_testing::stest!(name = test_ring_fd_closed_on_exec, suite = syscall_valid);
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
    name = test_process_group_slot_survives_republication,
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
    name = test_retired_net_syscalls_stay_unhandled,
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
    name = test_process_group_slot_survives_republication,
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
slopos_testing::stest!(
    name = test_vhangup_syscall_in_dispatch_table,
    suite = syscall_compat_smoke
);
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

fn unix_create_connected_pair(table: FdTable) -> Option<(i32, i32)> {
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

    let srv_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(
        accepted_handle,
        slopos_ostd::process::quota::root(),
    )?;
    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        table,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
        Some(srv_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    let cli_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(
        cli_handle,
        slopos_ostd::process::quota::root(),
    )?;
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        table,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
        Some(cli_backing),
        slopos_fs::fileio::FdFlags::NONE,
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
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

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
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // `file_poll_register_fd` registers PCR.current_task as the waiter, as the
    // real `syscall_poll` path does.
    make_task_current(task_id);

    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(
        reg.registered,
        "register must succeed when current_task owns the FD"
    );

    let payload = b"wake";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write wrong count");

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

/// `sleep_current_task_ms`'s `CAS(Running, Blocked)` must fail when the task is
/// not `Running`: a wait path that already CAS'd it to `Blocked` under a wait
/// queue's SpinLock must reject a concurrent sleep-blocking attempt.
pub fn test_sleep_ms_cas_overwrites_wakeup() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));

    // Tasks are `Blocked` at creation; publish Ready then Running first.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    let blocked_cas = task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::IoWait,
    );
    assert_eq_test!(blocked_cas, 0);

    // A racing `sleep_current_task_ms` would now try CAS(Running, Blocked).
    let result = task_set_state_from_with_reason(
        task_id,
        TaskStatus::Running,
        TaskStatus::Blocked,
        BlockReason::Sleep,
    );

    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Blocked, "state stays Blocked");
    assert_test!(
        result != 0,
        "CAS(Running, Blocked) should fail when task is already Blocked"
    );

    let _ = task_set_state(task_id, TaskStatus::Ready);
    task_terminate(task_id);
    TestResult::Pass
}

/// After a wake transitions the task `Blocked → Ready`, a stale blocker that
/// retries `CAS(Running, Blocked)` must fail: the state is no longer `Running`.
pub fn test_block_current_task_toctou_allows_reblock() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));

    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    // The wait-queue protocol commits Blocked under the queue lock.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0);
    // The producer wakes it: Blocked → Ready.
    let cas_wake = task_try_transition_from(task_id, TaskStatus::Blocked, TaskStatus::Ready);
    assert_eq_test!(cas_wake, 0);

    // The stale "block again" CAS, sourced from `Running`.
    let result = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Ready, "state should still be Ready");
    assert_test!(result != 0, "CAS(Running, Blocked) should fail from Ready");

    task_terminate(task_id);
    TestResult::Pass
}

/// The wait-queue protocol: a waiter CAS-flips Running → Blocked under the
/// queue lock, and a racing producer observes that and flips it to Ready.
pub fn test_wq_wrong_order_wakeup_lost() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));
    // Stand in for a published-then-blocked task; wakes refuse a nascent one.
    assert!(
        slopos_sched::scheduler::clear_nascent_for_test(task_id),
        "fixture task was not nascent"
    );

    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0);

    let result = unblock_task(&task_guard);
    assert_eq_test!(result, 0, "unblock_task should succeed from Blocked");

    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(
        state,
        TaskStatus::Ready,
        "wakeup transitions Blocked → Ready"
    );

    task_terminate(task_id);
    TestResult::Pass
}

/// `unblock_task` against a still-`Running` task — the producer's `wake_one`
/// beat the consumer into the queue lock — is a benign no-op; the consumer's
/// re-check inside the lock then observes the update and skips the block.
pub fn test_wq_correct_order_wakeup_preserved() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));

    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);
    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);

    let result = unblock_task(&task_guard);
    assert_eq_test!(result, 0, "unblock_task on Running task is a no-op");

    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Running, "state stays Running");

    task_terminate(task_id);
    TestResult::Pass
}

/// `try_transition_from(Running, Blocked)` rejects Ready and succeeds from
/// Running. The wait-queue protocol relies on that asymmetry to detect a wake
/// that already won the race and skip the CAS that would re-sleep the task.
pub fn test_try_transition_from_rejects_wrong_state() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID);
    let task_guard = assert_some!(task_find_by_id(task_id));

    // Ready: a wake has already transitioned the task off Running.
    assert_eq_test!(task_set_state(task_id, TaskStatus::Ready), 0);

    let result = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_test!(
        result != 0,
        "try_transition_from(Running, Blocked) should fail from Ready"
    );

    assert_eq_test!(task_set_state(task_id, TaskStatus::Running), 0);
    let result2 = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(
        result2,
        0,
        "try_transition_from(Running, Blocked) succeeds from Running"
    );
    let state2 = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state2, TaskStatus::Blocked, "state should be Blocked");

    task_terminate(task_id);
    TestResult::Pass
}

/// Exercises [`TaskState`]'s fused (status, reason, epoch) atomic directly.
pub fn test_task_state_fused_cas() -> TestResult {
    use slopos_sched::task_state::TaskState;

    let s = TaskState::invalid();
    s.force_set(TaskStatus::Running, BlockReason::None);
    let before = s.snapshot();
    assert_eq_test!(before.status, TaskStatus::Running);
    assert_eq_test!(before.reason, BlockReason::None);

    // Status and reason flip in one CAS.
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

    let err = s
        .try_transition(TaskStatus::Running, TaskStatus::Ready, BlockReason::None)
        .expect_err("wrong-expected CAS must fail");
    assert_eq_test!(err.status, TaskStatus::Blocked, "view returned on Err");

    let pre_bump = s.snapshot();
    s.bump_epoch();
    let post_bump = s.snapshot();
    assert_eq_test!(post_bump.status, pre_bump.status, "bump preserves status");
    assert_eq_test!(post_bump.reason, pre_bump.reason, "bump preserves reason");
    assert_test!(post_bump.epoch != pre_bump.epoch, "bump advances epoch");

    // Every defined pair, so the bit-field maxima on both axes are covered.
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

    // The u32::MAX → 0 wrap uses the same `wrapping_add(1)` as every other
    // increment, so mod-2^32 over 16 bumps covers it without 2^32 iterations.
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

/// The full kernel poll path end to end: WQ registration, readiness check,
/// `block_current_task_with_timeout`, and wakeup via `unix_send`. Two variants:
/// data already buffered (poll returns immediately) and no data with a short
/// timeout (poll returns 0 within margin).
pub fn test_unix_socket_poll_syscall_e2e() -> TestResult {
    use crate::syscall::fs::syscall_poll;
    use slopos_abi::syscall::UserPollFd;

    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task ptr");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

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

    // Variant 1: data written first, so poll must return immediately.
    let payload = b"e2e-test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write failed");

    let pfd = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd), "copy pollfd to user");

    let mut frame = zero_frame_boxed();
    frame.regs_mut().rdi = upage; // pollfd array pointer
    frame.regs_mut().rsi = 1; // nfds
    frame.regs_mut().rdx = 5000; // timeout_ms; irrelevant, data is ready

    let start = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_poll, &task_guard, &mut *frame)
    });
    let elapsed = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start);

    assert_eq_test!(frame.rax(), 1, "poll should report 1 ready fd");
    assert_test!(
        elapsed < 200,
        "poll with buffered data should return quickly"
    );

    if let Some(result_pfd) = user_copy_in::<UserPollFd>(pid, upage) {
        assert_test!(
            (result_pfd.revents & POLLIN) != 0,
            "poll should set POLLIN on readable socket"
        );
    }

    let mut out = [0u8; 16];
    let _ = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );

    // Variant 2: no data, so poll must return 0 when the timeout expires.
    let pfd_empty = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd_empty), "copy empty pollfd");

    let mut frame2 = zero_frame_boxed();
    frame2.regs_mut().rdi = upage;
    frame2.regs_mut().rsi = 1;
    frame2.regs_mut().rdx = 100; // 100ms timeout

    let start2 = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_poll, &task_guard, &mut *frame2)
    });
    let elapsed2 = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start2);

    assert_eq_test!(frame2.rax(), 0, "poll with no data should timeout");
    // 100 ms nominal; the margin is timer granularity.
    assert_test!(
        elapsed2 >= 50 && elapsed2 <= 500,
        "poll timeout duration out of range"
    );

    // Variant 3: pre-buffered data under a long timeout must not sleep — the
    // compositor handshake pattern.
    let payload2 = b"OutputInfo-sim";
    let written2 = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload2));
    assert_eq_test!(written2 as usize, payload2.len(), "write2 failed");

    let pfd3 = UserPollFd {
        fd: cli_fd,
        events: POLLIN,
        revents: 0,
    };
    assert_test!(user_copy_out(pid, upage, &pfd3), "copy pollfd3");

    let mut frame3 = zero_frame_boxed();
    frame3.regs_mut().rdi = upage;
    frame3.regs_mut().rsi = 1;
    frame3.regs_mut().rdx = 10_000; // 10s timeout (mimics compositor wait_recv)

    let start3 = slopos_kernel_services::platform::get_time_ms();
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_poll, &task_guard, &mut *frame3)
    });
    let elapsed3 = slopos_kernel_services::platform::get_time_ms().wrapping_sub(start3);

    assert_eq_test!(frame3.rax(), 1, "poll should find data immediately");
    assert_test!(
        elapsed3 < 200,
        "poll with pre-buffered data must not sleep (compositor handshake pattern)"
    );

    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    task_terminate(task_id);
    TestResult::Pass
}

/// The compositor-shell handshake: listen → connect(backlog) → accept → send →
/// poll. Catches an accept/send path that fails to make data visible to the
/// client's readiness check.
pub fn test_compositor_handshake_listen_accept_send_poll() -> TestResult {
    use slopos_net::unix_socket;
    use slopos_net::unix_socket_file_ops::UNIX_SOCKET_FILE_OPS;

    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

    let path = b"/test/compositor-handshake";

    let listen_handle = unix_socket::unix_create().expect("listen socket create");
    assert_eq_test!(unix_socket::unix_bind(listen_handle, path), 0, "bind");
    assert_eq_test!(unix_socket::unix_listen(listen_handle, 4), 0, "listen");
    unix_socket::unix_set_nonblocking(listen_handle, true);

    // Connect lands in the backlog; nothing has accepted yet.
    let cli_handle = unix_socket::unix_create().expect("client socket create");
    let rc = unix_socket::unix_connect(cli_handle, path);
    assert_eq_test!(rc, 0, "connect");

    let cli_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(
        cli_handle,
        slopos_ostd::process::quota::root(),
    )
    .expect("cli backing alloc");
    let cli_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        cli_handle.as_usize(),
        Some(cli_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(cli_fd >= 0, "cli fd open");

    let revents0 = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!(
        (revents0 & POLLIN) == 0,
        "client should have no data before server accept+send"
    );

    let accepted_handle = unix_socket::unix_accept(listen_handle).expect("accept");

    let srv_backing = slopos_net::unix_socket_file_ops::unix_socket_backing(
        accepted_handle,
        slopos_ostd::process::quota::root(),
    )
    .expect("srv backing alloc");
    let srv_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        &UNIX_SOCKET_FILE_OPS,
        accepted_handle.as_usize(),
        Some(srv_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(srv_fd >= 0, "srv fd open");

    // 16 bytes, the size of the compositor's `OutputInfo`.
    let payload = b"OutputInfo-simul";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "server write failed");

    let revents1 = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!(
        (revents1 & POLLIN) != 0,
        "client poll must see POLLIN after server send"
    );

    let mut out = [0u8; 32];
    let nread = file_read_fd(
        pid,
        cli_fd,
        &mut KernelIoBuf::new(&mut out[..payload.len()]),
    );
    assert_eq_test!(nread as usize, payload.len(), "read count");
    assert_test!(&out[..payload.len()] == payload, "payload mismatch");

    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    unix_socket::unix_close(listen_handle);
    task_terminate(task_id);
    TestResult::Pass
}

/// A unix socket write reaches the recv wait queue and CAS-flips the registered
/// task `Blocked → Ready`, end to end.
pub fn test_unix_send_wakes_blocked_poll_waiter() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // Dispatch sets the task Running, the precondition for the wait-queue
    // protocol's Running → Blocked CAS.
    make_task_current(task_id);

    // Register before checking readiness; that ordering is the whole point.
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "STEP1: register");
    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "STEP1: no data before write");

    // Commit Blocked under the wait queue's SpinLock, modelled by an explicit
    // CAS — `wait_event`'s lock-held push + CAS closes the same window.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "STEP2: Running → Blocked");

    let payload = b"wake-test";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "STEP3: write");

    let state_after = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
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

/// The check-first-register-second race: `wake_all` fires against an empty
/// queue and the committed-Blocked task stays `Blocked`. That lost-wakeup
/// signature is what the register-first ordering exists to prevent.
pub fn test_poll_fused_gap_demonstrates_race() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    // `enqueue_current` operates on PCR.current_task.
    make_task_current(task_id);

    // Commit Blocked without first registering — the broken ordering.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "STEP1: Running → Blocked");

    let revents = file_poll_fd(pid, cli_fd, POLLIN);
    assert_test!((revents & POLLIN) == 0, "no data yet");

    // Data arrives; `wake_all` fires with nobody on the queue.
    let payload = b"race-demo";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    // Registering now is too late: no `unblock_task` ever ran.
    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);

    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Blocked, "wakeup lost — still Blocked");

    // Undo the manual Blocked transition before terminate.
    let _ = task_set_state(task_id, TaskStatus::Ready);
    slopos_fs::fileio::file_poll_unregister_fd(&reg);
    file_close_fd(pid, srv_fd);
    file_close_fd(pid, cli_fd);
    park_bootstrap_on_current_cpu();
    task_terminate(task_id);
    TestResult::Pass
}

/// Register-first-then-check preserves the wakeup: `wake_all` finds the task on
/// the queue and CAS-flips it Blocked → Ready.
pub fn test_poll_fused_register_first_catches_wakeup() -> TestResult {
    let _fixture = SyscallFixture::new();
    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "create task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv_fd, cli_fd) = match unix_create_connected_pair(pid) {
        Some(pair) => pair,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };

    make_task_current(task_id);

    let reg = slopos_fs::fileio::file_poll_register_fd(pid, cli_fd, POLLIN);
    assert_test!(reg.registered, "register");

    // Commit Blocked under the queue lock, modelled by a direct CAS.
    let cas_block = task_try_transition_from(task_id, TaskStatus::Running, TaskStatus::Blocked);
    assert_eq_test!(cas_block, 0, "Running → Blocked");

    let payload = b"race-fix";
    let written = file_write_fd(pid, srv_fd, &mut KernelIoBufRef::new(payload));
    assert_eq_test!(written as usize, payload.len(), "write");

    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
    assert_eq_test!(state, TaskStatus::Ready, "wakeup preserved — Ready");

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

// `unix_sendmsg` must publish data bytes and ancillary fds together: the peer's
// `unix_recvmsg` sees both or neither.

/// Build a connected AF_UNIX pair and return the raw socket handles rather than
/// fd-table-installed `i32`s, for tests working below the fd layer.
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
    // Non-blocking, so probing an empty FIFO returns EAGAIN instead of parking
    // the test on the per-slot wait queue with no scheduler context.
    unix_socket::unix_set_nonblocking(accepted, true);
    unix_socket::unix_set_nonblocking(cli, true);
    Some((accepted, cli))
}

/// SCM_RIGHTS atomicity: one `unix_sendmsg` carrying data and an fd delivers
/// both to the peer's next `unix_recvmsg`, never the data alone with the fd
/// trailing in a separate ancillary state.
pub fn test_unix_scm_rights_atomic_delivery() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_net::unix_socket;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    let (mfd_handle, mfd_ops, mfd_backing) =
        match slopos_mm::memfd::memfd_create(0, slopos_ostd::process::quota::root()) {
            Some(h) => h,
            None => {
                unix_socket::unix_close(srv);
                unix_socket::unix_close(cli);
                return fail!("memfd_create failed");
            }
        };
    let mfd_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        mfd_ops,
        mfd_handle,
        Some(mfd_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(mfd_fd >= 0, "memfd fd install failed");

    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);

    let payload = b"ATOM";
    let n = unix_socket::unix_sendmsg(
        srv,
        payload,
        &mut files,
        slopos_ostd::process::quota::root(),
    );
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

    // The delivered alias shares the sender's open-file description.
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

/// A `sendmsg` that would push the per-direction anc queue past
/// `MAX_INFLIGHT_FDS` rejects with ENOMEM, leaving the aliases with the caller
/// and publishing nothing to the peer.
pub fn test_unix_scm_rights_anc_queue_full_no_partial() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_net::unix_socket;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    let (mfd_handle, mfd_ops, mfd_backing) =
        match slopos_mm::memfd::memfd_create(0, slopos_ostd::process::quota::root()) {
            Some(p_guard) => p_guard,
            None => return fail!("memfd_create failed"),
        };
    let mfd_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        mfd_ops,
        mfd_handle,
        Some(mfd_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(mfd_fd >= 0, "memfd fd install failed");

    const CAP: usize = 8;
    for _ in 0..CAP {
        let mut one: slopos_ostd::KVec<slopos_fs::FileRef> =
            slopos_ostd::KVec::with_capacity(1).expect("fill vec alloc");
        let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("fill clone failed");
        let _ = one.push(alias);
        let n = unix_socket::unix_sendmsg(srv, &[], &mut one, slopos_ostd::process::quota::root());
        assert_test!(n >= 0, "fill push returned {}", n);
    }

    let mut overflow: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("overflow vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("overflow clone failed");
    let _ = overflow.push(alias);
    let rc = unix_socket::unix_sendmsg(
        srv,
        b"X",
        &mut overflow,
        slopos_ostd::process::quota::root(),
    );
    assert_test!(rc == -12, "expected ENOMEM (-12), got {}", rc);
    assert_eq_test!(
        overflow.len(),
        1,
        "rejected send must leave the alias with the caller"
    );
    drop(overflow);

    // Drain anc with an empty data slice: that skips `unix_recv`, so the empty
    // non-blocking FIFO does not trip EAGAIN. The data FIFO is probed after.
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
    let Some(pid) = task_find_by_id(task_id)
        .and_then(|task| task.process())
        .as_deref()
        .and_then(FdTable::of)
    else {
        return TestResult::Fail;
    };

    // Close the peer immediately so the next send sees EPIPE.
    let (srv, cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };
    unix_socket::unix_close(cli);

    let (mfd_handle, mfd_ops, mfd_backing) =
        match slopos_mm::memfd::memfd_create(0, slopos_ostd::process::quota::root()) {
            Some(p_guard) => p_guard,
            None => return fail!("memfd_create failed"),
        };
    let mfd_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        mfd_ops,
        mfd_handle,
        Some(mfd_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(mfd_fd >= 0, "memfd fd install failed");
    assert_test!(
        slopos_mm::memfd::memfd_ftruncate(mfd_handle, 4096) == 0,
        "ftruncate failed"
    );

    let mut files: slopos_ostd::KVec<slopos_fs::FileRef> =
        slopos_ostd::KVec::with_capacity(1).expect("files vec alloc");
    let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("clone_file_ref failed");
    let _ = files.push(alias);
    let rc = unix_socket::unix_sendmsg(
        srv,
        b"DEAD",
        &mut files,
        slopos_ostd::process::quota::root(),
    );
    assert_test!(rc == -32, "expected EPIPE (-32), got {}", rc);
    assert_eq_test!(
        files.len(),
        1,
        "failed sendmsg must leave the alias with the caller"
    );

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

/// An in-flight `SCM_RIGHTS` descriptor is charged to the **sender** until it
/// stops being in flight. Without the `Custody` axis those `FileRef`s are held
/// by a `ConnectionPair` and by no descriptor table, so they count against
/// nothing; Linux answers the same hole with a per-user in-flight counter.
pub fn test_quota_custody_charges_the_sender() -> TestResult {
    let _fixture = SyscallFixture::new();
    use slopos_abi::quota::ResourceKind;
    use slopos_net::unix_socket;
    use slopos_ostd::process::quota::stats;

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "task creation failed");
    let Some(process) = task_find_by_id(task_id).and_then(|task| task.process()) else {
        return TestResult::Fail;
    };
    let account = process.account();
    let Some(pid) = FdTable::of(&process) else {
        return TestResult::Fail;
    };

    let (srv, _cli) = match unix_create_connected_pair_raw() {
        Some(pair) => pair,
        None => return fail!("could not create connected pair"),
    };

    let (mfd_handle, mfd_ops, mfd_backing) =
        match slopos_mm::memfd::memfd_create(0, slopos_ostd::process::quota::root()) {
            Some(triple) => triple,
            None => return fail!("memfd_create failed"),
        };
    let mfd_fd = slopos_fs::fileio::fileio_open_fd_with_ops(
        pid,
        mfd_ops,
        mfd_handle,
        Some(mfd_backing),
        slopos_fs::fileio::FdFlags::NONE,
    );
    assert_test!(mfd_fd >= 0, "memfd fd install failed");

    let baseline = stats(account, ResourceKind::Custody).map_or(0, |s| s.used);

    // Queued but not received: held by the connection pair alone.
    const SENT: u32 = 3;
    for _ in 0..SENT {
        let mut one: slopos_ostd::KVec<slopos_fs::FileRef> =
            slopos_ostd::KVec::with_capacity(1).expect("vec alloc");
        let alias = slopos_fs::fileio_clone_file_ref(pid, mfd_fd).expect("clone failed");
        let _ = one.push(alias);
        let n = unix_socket::unix_sendmsg(srv, &[], &mut one, account);
        assert_test!(n >= 0, "send returned {}", n);
    }

    let in_flight = stats(account, ResourceKind::Custody).map_or(0, |s| s.used);
    if in_flight != baseline + SENT {
        return fail!(
            "custody {} with {} in flight, want {}",
            in_flight,
            SENT,
            baseline + SENT
        );
    }

    // Receiving moves each reference into the receiver's table, where custody
    // ends and a descriptor number takes over.
    let mut received: slopos_ostd::KVec<slopos_fs::FileRef> = slopos_ostd::KVec::new();
    let _ = unix_socket::unix_recvmsg(_cli, &mut [0u8; 4], &mut received, SENT as usize);
    drop(received);

    let after = stats(account, ResourceKind::Custody).map_or(0, |s| s.used);
    if after != baseline {
        return fail!(
            "custody {} after receive, want the {} it started at",
            after,
            baseline
        );
    }
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_quota_custody_charges_the_sender,
    suite = unix_scm_rights
);

/// The default-action table: informational signals are ignored, job-control
/// signals stop/continue, everything else terminates.
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
    // Send-time droppability is strictly the Ignore class: Stop/Continue stay
    // deliverable so real job control needs no revisit of the raise sites.
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
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");

    // A fresh task has every action at SIG_DFL.
    assert_test!(
        !task::task_signal_post(&*task_guard, SIGWINCH),
        "default-ignored SIGWINCH must be dropped at send"
    );
    assert_eq_test!(
        task_guard.signal_pending(),
        0,
        "dropped signal must not pend"
    );

    assert_test!(
        task::task_signal_post(&*task_guard, SIGTERM),
        "SIGTERM must pend"
    );
    assert_eq_test!(
        task_guard.signal_pending(),
        sig_bit(SIGTERM),
        "SIGTERM pending bit expected"
    );
    task_guard.set_signal_pending(0);

    // Blocked pends regardless of disposition: a signalfd reader or a
    // later-installed handler may drain it after unblocking.
    task_guard.set_signal_blocked(sig_bit(SIGWINCH));
    assert_test!(
        task::task_signal_post(&*task_guard, SIGWINCH),
        "blocked SIGWINCH must pend"
    );
    assert_eq_test!(
        task_guard.signal_pending(),
        sig_bit(SIGWINCH),
        "blocked SIGWINCH pending bit expected"
    );
    task_guard.set_signal_pending(0);
    task_guard.set_signal_blocked(0);

    task_guard.signal_actions[(SIGWINCH - 1) as usize].store(SignalAction {
        handler: 0x4100_0000,
        mask: 0,
        flags: 0,
        restorer: 0x4200_0000,
    });
    assert_test!(
        task::task_signal_post(&*task_guard, SIGWINCH),
        "handled SIGWINCH must pend"
    );
    task_guard.set_signal_pending(0);

    task_guard.signal_actions[(SIGTERM - 1) as usize].store(SignalAction {
        handler: SIG_IGN,
        mask: 0,
        flags: 0,
        restorer: 0,
    });
    assert_test!(
        !task::task_signal_post(&*task_guard, SIGTERM),
        "SIG_IGN SIGTERM must be dropped at send"
    );
    assert_eq_test!(
        task_guard.signal_pending(),
        0,
        "ignored SIGTERM must not pend"
    );

    task_terminate(task_id);
    pass!()
}

/// `kill(SIGWINCH)` against a default-disposition task succeeds per POSIX and
/// the target survives — via the send-time drop, and for a directly-pended bit
/// via the delivery-point discard.
pub fn test_kill_default_ignored_sigwinch_target_survives() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut kill_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    kill_frame.regs_mut().rdi = task_id as u64;
    kill_frame.regs_mut().rsi = SIGWINCH as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &task_guard, &mut *kill_frame)
    });
    assert_eq_test!(kill_frame.rax(), 0, "kill(SIGWINCH) must succeed");
    assert_eq_test!(
        task_guard.signal_pending(),
        0,
        "SIGWINCH must be dropped at the send site"
    );

    // A bit pended directly, bypassing the send gate, must be discarded at the
    // delivery point rather than terminating.
    let _ = task::task_signal_raise(&*task_guard, sig_bit(SIGWINCH));
    let original_rip = 0x5000_4321u64;
    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = original_rip;
    deliver_pending_signal_as_current(task_id, pid, &user_frame);
    assert_eq_test!(
        user_frame.rip(),
        original_rip,
        "ignored delivery must not redirect RIP"
    );
    assert_eq_test!(
        task_guard.signal_pending(),
        0,
        "delivery must consume the ignored signal"
    );
    let state = Some(task_guard.status()).unwrap_or(TaskStatus::Terminated);
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

/// `kill(-pgid)` reaches a task that is registered but never published:
/// `task_create` writes `pgid = task_id` before registering, so the task is
/// registered, Blocked and `SchedPlacement::Nascent` when a group signal names
/// it. The kill must succeed and the signal pend, but placement must stay
/// `Nascent` — no runqueue grows.
pub fn test_kill_process_group_reaches_nascent_task_without_publishing() -> TestResult {
    let _fixture = SyscallFixture::new();

    let caller_id = create_test_user_task();
    assert_test!(caller_id != INVALID_TASK_ID, "failed to create caller task");
    let caller_guard = assert_some!(task_find_by_id(caller_id), "caller lookup failed");
    let Some(caller_pid) = caller_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    // User-mode because a broadcast kill only ever names user tasks.
    let target_id = create_test_user_task();
    assert_test!(target_id != INVALID_TASK_ID, "failed to create target task");
    let target_guard = assert_some!(task_find_by_id(target_id), "target lookup failed");

    assert_eq_test!(
        target_guard.pgid(),
        target_id,
        "task_create must seed pgid = task_id for the group signal to reach it"
    );
    assert_eq_test!(
        target_guard.sched_placement(),
        SchedPlacement::Nascent,
        "a freshly created task must still be Nascent"
    );
    target_guard.set_signal_pending(0);

    let cpu_id = slopos_arch::pcr::get_current_cpu();
    let ready_before =
        slopos_sched::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count())
            .unwrap_or(0);

    let mut frame = zero_frame();
    frame.regs_mut().rdi = (-(target_id as i32) as i64) as u64;
    frame.regs_mut().rsi = SIGUSR1 as u64;
    let _ = with_user_process_context(caller_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &caller_guard, &mut frame)
    });

    assert_eq_test!(
        frame.rax(),
        0,
        "kill(-pgid) must succeed for a task that exists, nascent or not"
    );
    assert_test!(
        (target_guard.signal_pending() & sig_bit(SIGUSR1)) != 0,
        "the signal must pend on the nascent target"
    );
    assert_eq_test!(
        target_guard.sched_placement(),
        SchedPlacement::Nascent,
        "the kill published a half-built task"
    );
    let ready_after =
        slopos_sched::per_cpu::with_cpu_scheduler(cpu_id, |sched| sched.total_ready_count())
            .unwrap_or(0);
    assert_eq_test!(
        ready_after,
        ready_before,
        "the kill enqueued a task that was never published"
    );

    task_terminate(target_id);
    task_terminate(caller_id);
    TestResult::Pass
}

/// A forked child inherits the parent's working directory and the two diverge
/// from there. The exact-slice compare is what catches a copied buffer with an
/// uncopied `cwd_len`: `with_cwd` slices by the length, so a stale one surfaces
/// as a truncated or over-long path rather than a fault.
pub fn test_fork_child_inherits_and_then_diverges_from_parent_cwd() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");

    // A non-default cwd before the fork.
    make_task_current(parent_id);
    let seeded = Current::get().is_some_and(|current| {
        current.task().set_cwd(&current, b"/usr/share")
            && current
                .task()
                .with_cwd(&current, |cwd| cwd == b"/usr/share\0".as_slice())
    });
    park_bootstrap_on_current_cpu();
    assert_test!(seeded, "could not seed the parent cwd");

    let child_id = task_fork(&parent_guard, None);
    assert_test!(child_id != INVALID_TASK_ID, "fork failed");
    task_set_state(child_id, TaskStatus::Blocked);

    make_task_current(child_id);
    let inherited = Current::get().is_some_and(|current| {
        current
            .task()
            .with_cwd(&current, |cwd| cwd == b"/usr/share\0".as_slice())
    });
    let child_moved = Current::get().is_some_and(|current| {
        current.task().set_cwd(&current, b"/tmp")
            && current
                .task()
                .with_cwd(&current, |cwd| cwd == b"/tmp\0".as_slice())
    });
    park_bootstrap_on_current_cpu();

    make_task_current(parent_id);
    let parent_unchanged = Current::get().is_some_and(|current| {
        current
            .task()
            .with_cwd(&current, |cwd| cwd == b"/usr/share\0".as_slice())
    });
    park_bootstrap_on_current_cpu();

    task_terminate(child_id);
    task_terminate(parent_id);

    assert_test!(inherited, "the child did not inherit the parent's cwd");
    assert_test!(child_moved, "the child could not change its own cwd");
    assert_test!(
        parent_unchanged,
        "the child's chdir moved the parent — the cwd cell is shared, not copied"
    );
    TestResult::Pass
}

/// `rt_sigaction` round-trips every field of the disposition, filters the
/// uncatchable signals out of `sa_mask`, and keeps signals independent.
/// `sa_mask` is OR'd into the blocked set at delivery, so an unfiltered one
/// would let a handler block SIGKILL.
pub fn test_rt_sigaction_round_trips_every_field() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let new_addr = page;
    let old_addr = page + 128;
    let set_size = core::mem::size_of::<SigSet>() as u64;

    let installed = UserSigaction {
        sa_handler: 0x4321_0000,
        sa_flags: SA_RESTART | SA_NODEFER,
        sa_restorer: 0x8765_0000,
        // SIGKILL and SIGSTOP must be filtered out; SIGCHLD must survive.
        sa_mask: sig_bit(SIGCHLD) | sig_bit(SIGKILL) | sig_bit(SIGSTOP),
    };
    assert_test!(
        user_copy_out(pid, new_addr, &installed),
        "failed to write the new sigaction"
    );

    let mut install = zero_frame_boxed();
    install.regs_mut().rdi = SIGUSR1 as u64;
    install.regs_mut().rsi = new_addr;
    install.regs_mut().rdx = 0;
    install.regs_mut().r10 = set_size;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigaction, &task_guard, &mut *install)
    });
    assert_eq_test!(install.rax(), 0, "install failed");

    // A null `new` reads back without disturbing anything.
    let mut query = zero_frame_boxed();
    query.regs_mut().rdi = SIGUSR1 as u64;
    query.regs_mut().rsi = 0;
    query.regs_mut().rdx = old_addr;
    query.regs_mut().r10 = set_size;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigaction, &task_guard, &mut *query)
    });
    assert_eq_test!(query.rax(), 0, "query-only rt_sigaction failed");

    let read_back: UserSigaction = match user_copy_in(pid, old_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        read_back.sa_handler,
        installed.sa_handler,
        "sa_handler did not round-trip"
    );
    assert_eq_test!(
        read_back.sa_flags,
        installed.sa_flags,
        "sa_flags did not round-trip"
    );
    assert_eq_test!(
        read_back.sa_restorer,
        installed.sa_restorer,
        "sa_restorer did not round-trip"
    );
    assert_eq_test!(
        read_back.sa_mask,
        sig_bit(SIGCHLD),
        "sa_mask must keep catchable signals and drop SIGKILL/SIGSTOP"
    );

    let mut other = zero_frame_boxed();
    other.regs_mut().rdi = SIGTERM as u64;
    other.regs_mut().rsi = 0;
    other.regs_mut().rdx = old_addr;
    other.regs_mut().r10 = set_size;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigaction, &task_guard, &mut *other)
    });
    assert_eq_test!(other.rax(), 0, "query of SIGTERM failed");
    let untouched: UserSigaction = match user_copy_in(pid, old_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        untouched.sa_handler,
        SIG_DFL,
        "installing on one signal disturbed another — the action index is off by one"
    );

    let mut bad_size = zero_frame_boxed();
    bad_size.regs_mut().rdi = SIGUSR1 as u64;
    bad_size.regs_mut().rsi = 0;
    bad_size.regs_mut().rdx = old_addr;
    bad_size.regs_mut().r10 = set_size + 1;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            &task_guard,
            &mut *bad_size,
        )
    });
    assert_eq_test!(
        bad_size.rax(),
        slopos_abi::Errno::EINVAL.as_u64(),
        "a wrong sigsetsize must be EINVAL"
    );

    let mut uncatchable = zero_frame_boxed();
    uncatchable.regs_mut().rdi = SIGKILL as u64;
    uncatchable.regs_mut().rsi = new_addr;
    uncatchable.regs_mut().rdx = 0;
    uncatchable.regs_mut().r10 = set_size;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_rt_sigaction,
            &task_guard,
            &mut *uncatchable,
        )
    });
    assert_eq_test!(
        uncatchable.rax(),
        slopos_abi::Errno::EINVAL.as_u64(),
        "installing a handler for SIGKILL must be EINVAL"
    );

    // A handler with no restorer cannot return.
    let no_restorer = UserSigaction {
        sa_handler: 0x4444_0000,
        sa_flags: 0,
        sa_restorer: 0,
        sa_mask: 0,
    };
    assert_test!(
        user_copy_out(pid, new_addr, &no_restorer),
        "failed to write the restorer-less sigaction"
    );
    let mut missing = zero_frame_boxed();
    missing.regs_mut().rdi = SIGUSR2 as u64;
    missing.regs_mut().rsi = new_addr;
    missing.regs_mut().rdx = 0;
    missing.regs_mut().r10 = set_size;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_rt_sigaction, &task_guard, &mut *missing)
    });
    assert_eq_test!(
        missing.rax(),
        slopos_abi::Errno::EINVAL.as_u64(),
        "a catching handler with no restorer must be EINVAL"
    );

    task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_kill_process_group_reaches_nascent_task_without_publishing,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_fork_child_inherits_and_then_diverges_from_parent_cwd,
    suite = syscall_valid
);
/// `rt_sigaction` bounds the signal number against `NSIG`, not a literal 64:
/// the handler indexes `[SignalActionCell; NSIG]` with `signum - 1`, and the
/// `old_act` read happens before every other validation.
pub fn test_rt_sigaction_bounds_signum_at_nsig() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let page = match map_user_rw_page(pid) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    let old_addr = page + 128;
    let set_size = core::mem::size_of::<SigSet>() as u64;

    // Query-only (`new == 0`, `old != 0`) is the shortest path to the table
    // read. A failed process-VM activation yields the sentinel, not a pass.
    const NO_CONTEXT: u64 = u64::MAX;
    let query = |signum: u64| -> u64 {
        let mut frame = zero_frame();
        frame.regs_mut().rdi = signum;
        frame.regs_mut().rsi = 0;
        frame.regs_mut().rdx = old_addr;
        frame.regs_mut().r10 = set_size;
        with_user_process_context(pid, || {
            crate::syscall::dispatch::dispatch_handler(
                syscall_rt_sigaction,
                &task_guard,
                &mut frame,
            )
        })
        .map(|_| frame.rax())
        .unwrap_or(NO_CONTEXT)
    };

    let einval = slopos_abi::Errno::EINVAL.as_u64();

    // NSIG itself is a real slot — the last one. An off-by-one in the other
    // direction would make this EINVAL.
    assert_eq_test!(
        query(NSIG as u64),
        0,
        "signal NSIG is the last table slot and must be queryable"
    );
    let at_nsig: UserSigaction = match user_copy_in(pid, old_addr) {
        Some(v) => v,
        None => {
            task_terminate(task_id);
            return TestResult::Fail;
        }
    };
    assert_eq_test!(
        at_nsig.sa_handler,
        SIG_DFL,
        "the last table slot must read back as SIG_DFL"
    );

    assert_eq_test!(
        query(NSIG as u64 + 1),
        einval,
        "signum NSIG+1 has no table slot and must be EINVAL"
    );
    // 64 was the old literal ceiling; it indexes past the end of the table.
    assert_eq_test!(
        query(64),
        einval,
        "signum 64 was the old literal bound and must now be EINVAL"
    );
    assert_eq_test!(query(65), einval, "signum 65 must be EINVAL");
    // `rt_sigaction` has no kill(2)-style existence-probe meaning for 0.
    assert_eq_test!(query(0), einval, "signum 0 must be EINVAL");

    task_terminate(task_id);
    TestResult::Pass
}

slopos_testing::stest!(
    name = test_rt_sigaction_round_trips_every_field,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_rt_sigaction_bounds_signum_at_nsig,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_spawn_path_rejects_privileged_flags,
    suite = syscall_valid
);
slopos_testing::stest!(
    name = test_set_cpu_affinity_rejects_other_process,
    suite = syscall_valid
);

/// A kernel-private pending bit is invisible to signal delivery, and no public
/// writer can disturb it. An unmasked bit at or above `NSIG` would yield
/// `signum = NSIG + 1`, whose `sig_bit` is 0 — the clearing `fetch_and(!0)` is
/// a no-op, so the bit re-delivers forever off the end of the action table.
pub fn test_kernel_private_pending_bit_is_not_a_signal() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    // One bit above the kill flag: private, with no meaning attached, so this
    // exercises the masking alone rather than kill semantics.
    let private_bit: SigSet = 1u64 << (NSIG + 1);
    assert_test!(
        private_bit != slopos_abi::signal::SIGNAL_KILLED,
        "the probe bit must not be the kill flag"
    );
    task_guard
        .signal_pending
        .fetch_or(private_bit, core::sync::atomic::Ordering::AcqRel);

    assert_test!(
        !task::task_has_deliverable_signal(&*task_guard),
        "a kernel-private bit must not read as a deliverable signal"
    );

    let original_rip = 0x5000_8765u64;
    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = original_rip;
    deliver_pending_signal_as_current(task_id, pid, &user_frame);
    assert_eq_test!(
        user_frame.rip(),
        original_rip,
        "a kernel-private bit must not redirect RIP"
    );
    assert_eq_test!(
        task_guard.signal_pending() & private_bit,
        private_bit,
        "delivery must leave a kernel-private bit set"
    );
    let state = task_guard.status();
    assert_test!(
        state != TaskStatus::Zombie && state != TaskStatus::Terminated,
        "a kernel-private bit must not terminate the target"
    );

    // The public writers own the signal range only.
    task_guard.set_signal_pending(0);
    assert_eq_test!(
        task_guard.signal_pending(),
        private_bit,
        "set_signal_pending must preserve kernel-private bits"
    );
    task_guard.clear_signal_pending(private_bit);
    assert_eq_test!(
        task_guard.signal_pending(),
        private_bit,
        "clear_signal_pending must not reach kernel-private bits"
    );
    task_guard
        .signal_pending
        .fetch_and(!private_bit, core::sync::atomic::Ordering::AcqRel);
    assert_test!(
        task_guard.raise_signal_pending(private_bit) & private_bit == 0
            && task_guard.signal_pending() & private_bit == 0,
        "raise_signal_pending must not reach kernel-private bits"
    );

    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_kernel_private_pending_bit_is_not_a_signal,
    suite = syscall_signal
);

/// A broadcast `kill` never names a kernel task: the SIGKILL path is not
/// signal-gated, so collecting one into a fanout would tear down a driver
/// thread that owns device state and an interrupt line.
pub fn test_broadcast_kill_spares_kernel_tasks() -> TestResult {
    let _fixture = SyscallFixture::new();

    let kernel_id = create_test_kernel_task();
    assert_test!(kernel_id != INVALID_TASK_ID, "failed to create kernel task");
    let user_id = create_test_user_task();
    assert_test!(user_id != INVALID_TASK_ID, "failed to create user task");
    let user_guard = assert_some!(task_find_by_id(user_id), "task lookup failed");
    let Some(pid) = user_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut kill_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    kill_frame.regs_mut().rdi = (-1i64) as u64;
    kill_frame.regs_mut().rsi = SIGTERM as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &user_guard, &mut *kill_frame)
    });

    let kernel_guard = assert_some!(task_find_by_id(kernel_id), "kernel task vanished");
    assert_eq_test!(
        kernel_guard.signal_pending(),
        0,
        "a broadcast kill must not pend a signal on a kernel task"
    );
    drop(kernel_guard);

    // The damaging arm: an explicitly named kernel task would be torn down.
    let mut named_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    named_frame.regs_mut().rdi = kernel_id as u64;
    named_frame.regs_mut().rsi = SIGKILL as u64;
    let _ = with_user_process_context(pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &user_guard, &mut *named_frame)
    });
    assert_eq_test!(
        named_frame.rax(),
        slopos_abi::Errno::ESRCH.as_u64(),
        "kill() must not name a kernel task"
    );
    assert_test!(
        task_find_by_id(kernel_id).is_some(),
        "a named SIGKILL tore down a kernel task"
    );

    task_terminate(kernel_id);
    task_terminate(user_id);
    pass!()
}

slopos_testing::stest!(
    name = test_broadcast_kill_spares_kernel_tasks,
    suite = syscall_signal
);

/// A task may not signal one holding a privilege it does not hold itself.
/// `task.flags` is the whole privilege model, so this is the relation POSIX
/// expresses with user ids.
pub fn test_kill_refuses_a_more_privileged_target() -> TestResult {
    let _fixture = SyscallFixture::new();

    let plain_id = create_test_user_task();
    assert_test!(plain_id != INVALID_TASK_ID, "failed to create user task");
    let privileged_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR);
    assert_test!(
        privileged_id != INVALID_TASK_ID,
        "failed to create privileged task"
    );

    let plain_guard = assert_some!(task_find_by_id(plain_id), "task lookup failed");
    let Some(plain_pid) = plain_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let privileged_guard = assert_some!(task_find_by_id(privileged_id), "task lookup failed");
    let Some(privileged_pid) = privileged_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut up: KBox<UserContext> = KBox::zeroed().expect("alloc");
    up.regs_mut().rdi = privileged_id as u64;
    up.regs_mut().rsi = SIGTERM as u64;
    let _ = with_user_process_context(plain_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &plain_guard, &mut *up)
    });
    assert_eq_test!(
        up.rax(),
        slopos_abi::Errno::EPERM.as_u64(),
        "an unprivileged task named a compositor-flagged one"
    );
    assert_eq_test!(
        privileged_guard.signal_pending(),
        0,
        "a refused kill still pended a signal"
    );

    // `kill(pid, 0)` is the existence-and-permission probe, so it answers the
    // permission question too rather than reporting the target as reachable.
    let mut probe: KBox<UserContext> = KBox::zeroed().expect("alloc");
    probe.regs_mut().rdi = privileged_id as u64;
    probe.regs_mut().rsi = 0;
    let _ = with_user_process_context(plain_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &plain_guard, &mut *probe)
    });
    assert_eq_test!(
        probe.rax(),
        slopos_abi::Errno::EPERM.as_u64(),
        "kill(pid, 0) reported a target the caller may not signal as reachable"
    );

    // Downward: a privileged sender reaches an unprivileged target.
    let mut down: KBox<UserContext> = KBox::zeroed().expect("alloc");
    down.regs_mut().rdi = plain_id as u64;
    down.regs_mut().rsi = SIGTERM as u64;
    let _ = with_user_process_context(privileged_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &privileged_guard, &mut *down)
    });
    assert_eq_test!(down.rax(), 0, "a privileged sender was refused a peer");
    assert_test!(
        plain_guard.signal_pending() != 0,
        "a permitted kill pended nothing"
    );

    drop(plain_guard);
    drop(privileged_guard);
    task_terminate(plain_id);
    task_terminate(privileged_id);
    pass!()
}

/// The broadcast arm applies the same relation to every target it collects.
pub fn test_broadcast_kill_spares_privileged_tasks() -> TestResult {
    let _fixture = SyscallFixture::new();

    let sender_id = create_test_user_task();
    assert_test!(sender_id != INVALID_TASK_ID, "failed to create user task");
    let peer_id = create_test_user_task();
    assert_test!(peer_id != INVALID_TASK_ID, "failed to create peer task");
    let privileged_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_NET_ADMIN);
    assert_test!(
        privileged_id != INVALID_TASK_ID,
        "failed to create privileged task"
    );

    let sender_guard = assert_some!(task_find_by_id(sender_id), "task lookup failed");
    let Some(sender_pid) = sender_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = (-1i64) as u64;
    frame.regs_mut().rsi = SIGTERM as u64;
    let _ = with_user_process_context(sender_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &sender_guard, &mut *frame)
    });

    let privileged_guard = assert_some!(task_find_by_id(privileged_id), "privileged task vanished");
    assert_eq_test!(
        privileged_guard.signal_pending(),
        0,
        "a broadcast kill reached a task holding a privilege the sender lacks"
    );
    drop(privileged_guard);

    let peer_guard = assert_some!(task_find_by_id(peer_id), "peer task vanished");
    assert_test!(
        peer_guard.signal_pending() != 0,
        "a broadcast kill missed an equally-privileged peer"
    );
    drop(peer_guard);

    drop(sender_guard);
    task_terminate(sender_id);
    task_terminate(peer_id);
    task_terminate(privileged_id);
    pass!()
}

/// Only the parent reaps: `task_consume_zombie` unlinks from whoever the parent
/// is and drops that owning reference, so a stranger's wait would take the exit
/// code and leave the real parent with `ECHILD`.
pub fn test_waitpid_refuses_a_task_that_is_not_a_child() -> TestResult {
    let _fixture = SyscallFixture::new();

    let child_id = create_test_user_task();
    assert_test!(child_id != INVALID_TASK_ID, "failed to create child task");
    let stranger_id = create_test_user_task();
    assert_test!(
        stranger_id != INVALID_TASK_ID,
        "failed to create stranger task"
    );

    let stranger_guard = assert_some!(task_find_by_id(stranger_id), "task lookup failed");
    let Some(stranger_pid) = stranger_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    // WNOHANG so the permitted case reports "alive, nothing to reap" instead
    // of blocking the test on a task that never exits.
    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = child_id as u64;
    frame.regs_mut().rsi = 1;
    let _ = with_user_process_context(stranger_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_waitpid, &stranger_guard, &mut *frame)
    });
    assert_eq_test!(
        frame.rax(),
        slopos_abi::Errno::ECHILD.as_u64(),
        "a non-parent was allowed to wait on a foreign task"
    );

    // From the real parent, the same call gets past the relation check.
    {
        let child_guard = assert_some!(task_find_by_id(child_id), "child task vanished");
        child_guard.set_parent_task_id(stranger_id);
    }
    let mut parent_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    parent_frame.regs_mut().rdi = child_id as u64;
    parent_frame.regs_mut().rsi = 1;
    let _ = with_user_process_context(stranger_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_waitpid,
            &stranger_guard,
            &mut *parent_frame,
        )
    });
    assert_eq_test!(
        parent_frame.rax(),
        slopos_abi::Errno::EAGAIN.as_u64(),
        "the parent's own wait was refused"
    );

    drop(stranger_guard);
    task_terminate(child_id);
    task_terminate(stranger_id);
    pass!()
}

slopos_testing::stest!(
    name = test_kill_refuses_a_more_privileged_target,
    suite = syscall_signal
);
slopos_testing::stest!(
    name = test_broadcast_kill_spares_privileged_tasks,
    suite = syscall_signal
);
slopos_testing::stest!(
    name = test_waitpid_refuses_a_task_that_is_not_a_child,
    suite = syscall_signal
);

/// A task marked for death exits at its next return-to-user boundary. The kill
/// flag sits outside the deliverable range, so `claim_pending_signal` never
/// sees it and something on the way back to CPL3 has to act on it.
pub fn test_killed_task_exits_at_return_to_user() -> TestResult {
    let _fixture = SyscallFixture::new();

    let task_id = create_test_user_task();
    assert_test!(task_id != INVALID_TASK_ID, "failed to create user task");
    let task_guard = assert_some!(task_find_by_id(task_id), "task lookup failed");
    let Some(pid) = task_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    assert_test!(
        !task_guard.is_killed(),
        "a fresh task must not be marked for death"
    );
    assert_test!(
        task::task_kill_and_wake(&*task_guard),
        "the first mark must report that it did the marking"
    );
    assert_test!(
        !task::task_kill_and_wake(&*task_guard),
        "a second mark must report that the task was already marked"
    );
    assert_test!(task_guard.is_killed(), "the mark must be observable");

    // The kill bit must stay invisible to signal delivery itself.
    assert_test!(
        !task::task_has_deliverable_signal(&*task_guard),
        "the kill bit must not read as a deliverable signal"
    );

    let mut user_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    user_frame.regs_mut().rip = 0x5000_1111;
    deliver_pending_signal_as_current(task_id, pid, &user_frame);

    let status = task_guard.status();
    assert_test!(
        status == TaskStatus::Zombie || status == TaskStatus::Terminated,
        "a marked task must not return to userland"
    );

    drop(task_guard);
    task_terminate(task_id);
    pass!()
}

slopos_testing::stest!(
    name = test_killed_task_exits_at_return_to_user,
    suite = syscall_signal
);

/// SIGKILL marks its target and lets the target exit from its own context, so
/// destructors run on the victim's own stack and the disposition supplies the
/// `128 + signal` exit code POSIX specifies.
pub fn test_sigkill_marks_the_target_and_exits_with_the_signal() -> TestResult {
    let _fixture = SyscallFixture::new();

    let victim_id = create_test_user_task();
    assert_test!(victim_id != INVALID_TASK_ID, "failed to create the victim");
    let victim = assert_some!(task_find_by_id(victim_id), "victim lookup failed");
    let Some(victim_pid) = victim
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let caller_id = create_test_user_task();
    assert_test!(caller_id != INVALID_TASK_ID, "failed to create the caller");
    let caller = assert_some!(task_find_by_id(caller_id), "caller lookup failed");
    let Some(caller_pid) = caller
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = victim_id as u64;
    frame.regs_mut().rsi = SIGKILL as u64;
    let _ = with_user_process_context(caller_pid, || {
        crate::syscall::dispatch::dispatch_handler(syscall_kill, &caller, &mut *frame)
    });
    assert_eq_test!(frame.rax(), 0, "kill(SIGKILL) must succeed");

    assert_test!(victim.is_killed(), "SIGKILL must mark the target for death");
    assert_test!(
        (victim.signal_pending() & sig_bit(SIGKILL)) != 0,
        "SIGKILL must also pend as an ordinary signal, which carries the exit code"
    );
    let status = victim.status();
    assert_test!(
        status != TaskStatus::Zombie && status != TaskStatus::Terminated,
        "the caller must not tear the victim down from its own CPU"
    );

    let mut victim_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    deliver_pending_signal_as_current(victim_id, victim_pid, &victim_frame);
    victim_frame.regs_mut().rax = 0;

    let status = victim.status();
    assert_test!(
        status == TaskStatus::Zombie || status == TaskStatus::Terminated,
        "the victim must exit at its own boundary"
    );
    assert_eq_test!(
        victim.exit_code.load(core::sync::atomic::Ordering::Acquire),
        128 + SIGKILL as u32,
        "a signalled exit reports 128 + the signal"
    );

    drop(victim);
    drop(caller);
    task_terminate(victim_id);
    task_terminate(caller_id);
    pass!()
}

slopos_testing::stest!(
    name = test_sigkill_marks_the_target_and_exits_with_the_signal,
    suite = syscall_signal
);

/// Installing a keyboard layout needs console administration: there is one
/// layout table and it feeds every TTY and the compositor's input path, so this
/// is `loadkeys`, not `setxkbmap`.
pub fn test_keymap_load_requires_console_admin() -> TestResult {
    let _fixture = SyscallFixture::new();

    let plain_id = create_test_user_task();
    assert_test!(plain_id != INVALID_TASK_ID, "failed to create user task");
    let admin_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_CONSOLE_ADMIN);
    assert_test!(admin_id != INVALID_TASK_ID, "failed to create admin task");

    let plain_guard = assert_some!(task_find_by_id(plain_id), "task lookup failed");
    let Some(plain_pid) = plain_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };
    let admin_guard = assert_some!(task_find_by_id(admin_id), "task lookup failed");
    let Some(admin_pid) = admin_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return TestResult::Fail;
    };

    let Some(user_buf) = map_user_rw_page(plain_pid) else {
        return fail!("could not map a user page");
    };

    let mut plain_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    plain_frame.regs_mut().rdi = user_buf;
    plain_frame.regs_mut().rsi = 16;
    let _ = with_user_process_context(plain_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_keymap_load,
            &plain_guard,
            &mut *plain_frame,
        )
    });
    assert_eq_test!(
        plain_frame.rax(),
        slopos_abi::Errno::EPERM.as_u64(),
        "an unprivileged task installed a keyboard layout"
    );

    // The privileged caller is stopped by the validator instead — a different
    // refusal, which is what proves the gate is the only thing between them.
    let Some(admin_buf) = map_user_rw_page(admin_pid) else {
        return fail!("could not map a user page");
    };
    let mut admin_frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    admin_frame.regs_mut().rdi = admin_buf;
    admin_frame.regs_mut().rsi = 16;
    let _ = with_user_process_context(admin_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            syscall_keymap_load,
            &admin_guard,
            &mut *admin_frame,
        )
    });
    assert_test!(
        admin_frame.rax() != slopos_abi::Errno::EPERM.as_u64(),
        "console administration was refused the syscall"
    );

    drop(plain_guard);
    drop(admin_guard);
    task_terminate(plain_id);
    task_terminate(admin_id);
    pass!()
}

slopos_testing::stest!(
    name = test_keymap_load_requires_console_admin,
    suite = syscall_core
);

/// `process_list` reports only tasks that can still run code. A `Zombie` has no
/// address space, no descriptor table and no scheduler placement — it is an
/// exit-status receipt, not something a caller should be invited to kill.
pub fn test_process_list_excludes_exited_tasks() -> TestResult {
    let _fixture = SyscallFixture::new();

    let caller_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_SYSTEM);
    assert_test!(caller_id != INVALID_TASK_ID, "failed to create caller task");
    let caller_guard = assert_some!(task_find_by_id(caller_id), "caller lookup failed");
    let Some(caller_pid) = caller_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return fail!("caller has no fd table");
    };

    let victim_id = create_test_user_task();
    assert_test!(victim_id != INVALID_TASK_ID, "failed to create victim task");
    // A live parent is what lands the exit in `Zombie` rather than `Terminated`.
    {
        let victim_guard = assert_some!(task_find_by_id(victim_id), "victim lookup failed");
        victim_guard.set_parent_task_id(caller_id);
    }

    let listed = |pid: FdTable, buf_va: u64, max: u64| -> u64 {
        let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
        frame.regs_mut().rdi = buf_va;
        frame.regs_mut().rsi = max;
        let _ = with_user_process_context(pid, || {
            crate::syscall::dispatch::dispatch_handler(
                crate::syscall::core_handlers::syscall_process_list,
                &caller_guard,
                &mut *frame,
            )
        });
        frame.rax()
    };

    let Some(buf) = map_user_rw_page(caller_pid) else {
        return fail!("could not map a user page");
    };
    let capacity = (4096 / core::mem::size_of::<slopos_abi::syscall::UserTaskEntry>()) as u64;

    let contains = |count: u64, want: u32| -> bool {
        (0..count).any(|i| {
            let addr = buf + i * core::mem::size_of::<slopos_abi::syscall::UserTaskEntry>() as u64;
            let Ok(ptr) = UserPtr::<slopos_abi::syscall::UserTaskEntry>::try_new(addr) else {
                return false;
            };
            with_user_process_context(caller_pid, || {
                copy_from_user(ptr)
                    .map(|e| e.task_id == want)
                    .unwrap_or(false)
            })
            .unwrap_or(false)
        })
    };

    let before = listed(caller_pid, buf, capacity);
    assert_test!(
        contains(before, victim_id),
        "a live task was missing from process_list"
    );

    task_terminate(victim_id);
    let status = task_find_by_id(victim_id).map(|t| t.status());
    assert_eq_test!(
        status,
        Some(TaskStatus::Zombie),
        "the victim did not become a Zombie, so the test proves nothing"
    );

    let after = listed(caller_pid, buf, capacity);
    assert_test!(
        !contains(after, victim_id),
        "an exited task was reported by process_list"
    );

    drop(caller_guard);
    task_terminate(caller_id);
    pass!()
}

/// Enumeration answers to the same relation `kill` does: an unprivileged caller
/// is not told the id of a task it could not signal, so the two cannot disagree.
pub fn test_process_list_hides_undominated_tasks() -> TestResult {
    let _fixture = SyscallFixture::new();

    let plain_id = create_test_user_task();
    assert_test!(plain_id != INVALID_TASK_ID, "failed to create plain task");
    let plain_guard = assert_some!(task_find_by_id(plain_id), "plain lookup failed");
    let Some(plain_pid) = plain_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return fail!("plain caller has no fd table");
    };

    let privileged_id = create_test_user_task_with(TASK_FLAG_USER_MODE | TASK_FLAG_COMPOSITOR);
    assert_test!(
        privileged_id != INVALID_TASK_ID,
        "failed to create privileged task"
    );

    let Some(buf) = map_user_rw_page(plain_pid) else {
        return fail!("could not map a user page");
    };
    let capacity = (4096 / core::mem::size_of::<slopos_abi::syscall::UserTaskEntry>()) as u64;

    let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
    frame.regs_mut().rdi = buf;
    frame.regs_mut().rsi = capacity;
    let _ = with_user_process_context(plain_pid, || {
        crate::syscall::dispatch::dispatch_handler(
            crate::syscall::core_handlers::syscall_process_list,
            &plain_guard,
            &mut *frame,
        )
    });
    let count = frame.rax();

    let mut saw_privileged = false;
    let mut saw_kernel_task = false;
    let mut saw_self = false;
    for i in 0..count {
        let addr = buf + i * core::mem::size_of::<slopos_abi::syscall::UserTaskEntry>() as u64;
        let Ok(ptr) = UserPtr::<slopos_abi::syscall::UserTaskEntry>::try_new(addr) else {
            continue;
        };
        let Some(Ok(entry)) = with_user_process_context(plain_pid, || copy_from_user(ptr)) else {
            continue;
        };
        if entry.task_id == privileged_id {
            saw_privileged = true;
        }
        if entry.task_id == plain_id {
            saw_self = true;
        }
        if let Some(t) = task_find_by_id(entry.task_id)
            && t.flags & TASK_FLAG_USER_MODE == 0
        {
            saw_kernel_task = true;
        }
    }

    assert_test!(
        saw_self,
        "a task could not see itself, so the filter is too strict"
    );
    assert_test!(
        !saw_privileged,
        "an unprivileged task enumerated a COMPOSITOR task it may not signal"
    );
    assert_test!(
        !saw_kernel_task,
        "an unprivileged task enumerated a kernel task"
    );

    drop(plain_guard);
    task_terminate(plain_id);
    task_terminate(privileged_id);
    pass!()
}

slopos_testing::stest!(
    name = test_process_list_excludes_exited_tasks,
    suite = syscall_core
);
slopos_testing::stest!(
    name = test_process_list_hides_undominated_tasks,
    suite = syscall_core
);

/// `waitpid(-1)` reaps whichever child exited, without being told which.
pub fn test_waitpid_any_reaps_an_unnamed_child() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");
    let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");
    let Some(parent_pid) = parent_guard
        .process()
        .as_deref()
        .and_then(slopos_fs::fileio::FdTable::of)
    else {
        return fail!("parent has no fd table");
    };

    // WNOHANG with no children at all is ECHILD, not EAGAIN: a supervisor loop
    // keys on the difference.
    let wait_any = |wnohang: u64| -> u64 {
        let mut frame: KBox<UserContext> = KBox::zeroed().expect("alloc");
        frame.regs_mut().rdi = u32::MAX as u64;
        frame.regs_mut().rsi = wnohang;
        let _ = with_user_process_context(parent_pid, || {
            crate::syscall::dispatch::dispatch_handler(syscall_waitpid, &parent_guard, &mut *frame)
        });
        frame.rax()
    };

    assert_eq_test!(
        wait_any(1),
        slopos_abi::Errno::ECHILD.as_u64(),
        "wait-any with no children must be ECHILD"
    );

    let child_id = create_test_user_task();
    assert_test!(child_id != INVALID_TASK_ID, "failed to create child task");
    assert_eq_test!(
        slopos_sched::task::task_set_parent(child_id, parent_id),
        0,
        "could not parent the child"
    );

    assert_eq_test!(
        wait_any(1),
        slopos_abi::Errno::EAGAIN.as_u64(),
        "wait-any with a live child must be EAGAIN, not ECHILD"
    );

    task_terminate(child_id);
    assert_eq_test!(
        task_find_by_id(child_id).map(|t| t.status()),
        Some(TaskStatus::Zombie),
        "the child did not become a Zombie"
    );

    // The reap resolves the child without the caller ever naming it.
    let rc = wait_any(1);
    assert_test!(
        rc != slopos_abi::Errno::EAGAIN.as_u64() && rc != slopos_abi::Errno::ECHILD.as_u64(),
        "wait-any did not reap an exited child"
    );
    assert_test!(
        !matches!(
            task_find_by_id(child_id).map(|t| t.status()),
            Some(TaskStatus::Zombie)
        ),
        "the child stayed a Zombie after being reaped"
    );

    drop(parent_guard);
    task_terminate(parent_id);
    pass!()
}

/// An explicit `SIGCHLD = SIG_IGN` parent gets no zombies (POSIX
/// `SA_NOCLDWAIT`), while a `SIG_DFL` parent still does. SlopOS maps SIGCHLD's
/// *default* action to Ignore, so keying the skip on the effective disposition
/// would leave `waitpid` nothing to reap for any ordinary parent.
pub fn test_sigchld_ignore_skips_the_zombie_state() -> TestResult {
    let _fixture = SyscallFixture::new();

    let exit_status_of = |handler: u64| -> Option<TaskStatus> {
        let parent_id = create_test_user_task();
        if parent_id == INVALID_TASK_ID {
            return None;
        }
        {
            let parent_guard = task_find_by_id(parent_id)?;
            parent_guard.set_signal_action(
                (SIGCHLD - 1) as usize,
                SignalAction {
                    handler,
                    mask: 0,
                    flags: 0,
                    restorer: 0,
                },
            );
        }
        let child_id = create_test_user_task();
        if child_id == INVALID_TASK_ID {
            return None;
        }
        if slopos_sched::task::task_set_parent(child_id, parent_id) != 0 {
            return None;
        }
        task_terminate(child_id);
        let status = task_find_by_id(child_id).map(|t| t.status());
        task_terminate(parent_id);
        status
    };

    // SIGCHLD's default is Ignore, but the *status* is still kept.
    assert_eq_test!(
        exit_status_of(SIG_DFL),
        Some(TaskStatus::Zombie),
        "a default-disposition parent lost its child's exit status"
    );

    // An explicit SIG_IGN declares the parent will never reap, so holding a
    // receipt for it would hold one forever.
    assert_test!(
        !matches!(exit_status_of(SIG_IGN), Some(TaskStatus::Zombie)),
        "a SIG_IGN parent still accumulated a zombie"
    );

    pass!()
}

/// A parent that never reaps cannot grow its zombie set without bound: each
/// retained receipt pins a `Task`, a 32 KiB kernel stack, a 16 KiB data stack
/// and a registry slot, so an unbounded set walks the machine to spawn failure.
pub fn test_zombie_budget_is_enforced_per_parent() -> TestResult {
    let _fixture = SyscallFixture::new();

    let parent_id = create_test_user_task();
    assert_test!(parent_id != INVALID_TASK_ID, "failed to create parent task");

    let budget = slopos_sched::task::MAX_ZOMBIES_PER_PARENT;
    let mut spawned = 0usize;
    for _ in 0..(budget + 8) {
        let child_id = create_test_user_task();
        if child_id == INVALID_TASK_ID {
            break;
        }
        if slopos_sched::task::task_set_parent(child_id, parent_id) != 0 {
            task_terminate(child_id);
            break;
        }
        task_terminate(child_id);
        spawned += 1;
    }
    assert_test!(
        spawned > budget,
        "could not create enough children to exceed the budget"
    );

    let held = {
        let parent_guard = assert_some!(task_find_by_id(parent_id), "parent lookup failed");
        parent_guard.children_len()
    };
    assert_test!(
        held <= budget,
        "a parent retained more unreaped children than the budget allows"
    );

    task_terminate(parent_id);
    pass!()
}

slopos_testing::stest!(
    name = test_waitpid_any_reaps_an_unnamed_child,
    suite = syscall_core
);
slopos_testing::stest!(
    name = test_sigchld_ignore_skips_the_zombie_state,
    suite = syscall_core
);
slopos_testing::stest!(
    name = test_zombie_budget_is_enforced_per_parent,
    suite = syscall_core
);
